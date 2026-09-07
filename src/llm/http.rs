//! HTTP-Client, Request/Response, SSE-Streaming, Retry/Backoff.

use crate::config::ResolvedEndpoint;
use crate::perm::Permission;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use super::api;

use serde_json::Value;

use super::helpers::{
    dump_debug, error_chain, error_message, reasoning_contract_hint, server_error_summary,
    take_head, truncate, with_debug, ERROR_SUMMARY_MAX,
};
use super::tools_def::{
    apply_tool_delta, sanitize_arguments, Step, ToolCallAcc, ToolInvocation,
};
use super::wire::{WireFunction, WireMessage, WireToolCall};
use super::{CompletionParts, Usage, WorkerEvent};

/// Prozessweit geteilter blocking-HTTP-Client. Die Verbindung zum Endpunkt
/// bleibt als Keep-Alive im Pool liegen und wird für den nächsten Turn
/// wiederverwendet (spart TLS-Handshake pro Nachricht). Wird eine
/// zwischenzeitlich verworfene Verbindung erwischt, stellt reqwest sie
/// transparent neu her – kein Korrektheitsrisiko.
static SHARED_CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();

pub(crate) fn shared_client() -> &'static reqwest::blocking::Client {
    SHARED_CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(30))
            // Leerlaufende Verbindungen 2 min vorhalten; pro Host reicht eine,
            // da Anfragen seriell laufen.
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(1)
            .build()
            .expect("reqwest-Client bauen (TLS-Backend statisch vorhanden)")
    })
}
/// Hängt ein `data:`-Fragment an das offene SSE-Event an und liefert es als
/// `Value`, sobald es als Ganzes gültiges JSON ist (`None` = Fragment noch
/// unvollständig → nächste `data:`-Zeile weiter anhängen). Manche
/// OpenAI-kompatiblen Endpunkte/Proxys zerlegen große Events (z. B. lange
/// `edit`/`write`-Argumente mit mehrzeiligen Blöcken) über mehrere
/// `data:`-Zeilen; jede einzelne ist dann kein gültiges JSON. Die übliche
/// „eine Zeile = ein Event“-Form bleibt unverändert (ein solches Fragment ist
/// sofort gültig und wird direkt geliefert).
pub(crate) fn accumulate_sse_event(pending: &mut String, payload: &str) -> Option<serde_json::Value> {
    if !pending.is_empty() {
        pending.push('\n');
    }
    pending.push_str(payload);
    match serde_json::from_str::<serde_json::Value>(&pending) {
        Ok(v) => {
            pending.clear();
            Some(v)
        }
        Err(_) => None,
    }
}

/// Eine HTTP-Runde. Bei (vermuteter) fehlender Werkzeug-Unterstützung des
/// Endpunkts wird einmal ohne `tools` wiederholt (graceful degradation).
#[allow(clippy::too_many_arguments)]
pub(crate) fn request_once(
    tx: &Sender<WorkerEvent>,
    session: usize,
    client: &reqwest::blocking::Client,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
    with_tools: bool,
    permission: Permission,
) -> (Step, bool) {
    if !with_tools {
        return (
            do_request(tx, session, client, ep, msgs, cancel, false, permission),
            false,
        );
    }
    let first = do_request(tx, session, client, ep, msgs, cancel, true, permission);
    if let Step::Err(msg) = &first {
        // Nur eine 400-Antwort mit Tool-/Funktions-Hinweis deutet auf fehlende
        // Werkzeug-Unterstützung hin – DANN ohne `tools` wiederholen. Andere
        // Statuscodes (401/403/429/5xx …) behandelt der Backoff-Retry in
        // `do_request`; ein zusätzlicher Fallback würde die eigentliche
        // Ursache nur maskieren (z. B. verdoppelte Requests bei Rate-Limits).
        if msg.starts_with("API error (400")
            && (msg.contains("tool") || msg.contains("function"))
            && !cancel.load(Ordering::Relaxed)
        {
            // Retry ohne Tools: die Info geht sonst als WorkerEvent/Fehler
            // in die UI; ein Konsolen-Print würde das TUI-Layout zerschießen.
            let retry = do_request(tx, session, client, ep, msgs, cancel, false, permission);
            return (retry, false);
        }
    }
    (first, true)
}

/// Eine einzelne streamende Anfrage an `/chat/completions`.
/// Obergrenze, bis zu der der SSE-Stream nach `[DONE]` noch zu Ende gelesen
/// wird, bevor wir die Verbindung aufgeben (Altverhalten).
const DRAIN_AFTER_DONE: Duration = Duration::from_secs(10);

/// HTTP-Statuscodes, die mit Backoff wiederholt werden (transiente Fehler).
/// 401/403/400 laufen NICHT hierüber: Auth-Fehler helfen keine Retries, und
/// 400 geht ggf. in den No-Tools-Fallback (`request_once`) bzw. die reaktive
/// Kompaktierung (context_length).
const RETRYABLE_STATUS: &[u16] = &[408, 429, 500, 502, 503, 504];
/// Maximale Anzahl Versuche pro HTTP-Runde.
const RETRY_MAX_ATTEMPTS: usize = 3;
/// Basis-Verzögerung des exponentiellen Backoffs (2 s → 4 s → …).
const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
/// Obergrenze für eine einzelne Wartezeit (auch für `Retry-After`), damit ein
/// böswilliger/fehlkonfigurierter Server die Session nicht blockieren kann.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);

/// Wartezeit für den exponentiellen Backoff (`2 s * 2^attempt`), gecappt auf
/// `RETRY_MAX_DELAY`. Ein serverseitiges `Retry-After` wird separat behandelt.
pub(crate) fn retry_delay(attempt: usize) -> Duration {
    RETRY_BASE_DELAY
        .saturating_mul(1u32 << attempt.min(4))
        .min(RETRY_MAX_DELAY)
}

/// Kleiner deterministischer Jitter (±30 %) für den Backoff, damit
/// parallel gestartete Clients sich nicht synchron wiederholen. Ohne
/// Zufalls-Dependency (kein `rand` nötig).
pub(crate) fn jitter(d: Duration) -> Duration {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return d;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0) as u128;
    let factor = 70 + (seed % 61); // 70..=130 → ±30 %
    Duration::from_nanos((nanos * factor / 100) as u64)
}

/// Schläft bis `until`, bricht aber bei gesetztem `cancel` vorzeitig ab –
/// Esc während eines Retry-Fensters muss den Turn genauso unterbrechen können
/// wie während einer laufenden Antwort. Liefert `false`, wenn abgebrochen.
fn sleep_with_cancel(cancel: &AtomicBool, until: Instant) -> bool {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        let remain = until
            .saturating_duration_since(now)
            .min(Duration::from_millis(50));
        thread::sleep(remain);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn do_request(
    tx: &Sender<WorkerEvent>,
    session: usize,
    client: &reqwest::blocking::Client,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
    with_tools: bool,
    permission: Permission,
) -> Step {
    // API-Shape ermitteln (Probe am Modell-Endpunkt + Cache). `determined`
    // sagt, ob die Shape aus Metadaten stammt oder nur geraten wurde – bei
    // einer geratenen Shape wird unten bei einem Server-Fehler (5xx) auch die
    // andere API probiert.
    let shape_info = api::resolve_shape_info(client, ep);
    let mut shape = shape_info.shape;
    let (mut url, mut body) = api::build_body(ep, shape, msgs, with_tools, permission, true, None);
    let mut shape_flipped = false;

    // Retry-Schleife: 429/5xx/Netzwerkfehler werden mit Backoff wiederholt.
    // Zwischen den Versuchen geht ein `Retrying`-Event an die UI (Statuszeile
    // mit Countdown); `cancel` (Esc) bricht auch während des Wartens ab. Erst
    // beim endgültigen Aufgeben werden Request + Antwort als Debug-Material
    // abgelegt und die einzeilige Kurzfassung nach oben gereicht.
    let mut attempt: usize = 0;
    let resp = loop {
        if cancel.load(Ordering::Relaxed) {
            return Step::Cancelled;
        }
        let req = client
            .post(&url)
            .bearer_auth(&ep.api_key)
            .header("user-agent", &ep.user_agent)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        // opencode.ai erwartet zusätzlich `x-opencode-client: cli` +
        // `x-opencode-project: global` + generierte Request-/Session-IDs –
        // nur für diese Ziel-URL. Der `x-opencode-session`-Header bleibt über
        // die ganze Konversation stabil, `x-opencode-request` ist pro Request
        // neu (beide werden beim ersten Bedarf generiert).
        let req = if crate::config::is_opencode_base(&ep.base_url) {
            req.header("x-opencode-client", "cli")
                .header("x-opencode-project", "global")
                .header("x-opencode-request", super::ident::message_id())
                .header("x-opencode-session", super::ident::session_id_for(session))
        } else {
            req
        };
        let resp = match req
            .json(&body)
            .send()
        {
            Ok(r) => r,
            Err(err) => {
                let chain = error_chain(&err);
                let summary = take_head(&format!("Netzwerkfehler: {chain}"), ERROR_SUMMARY_MAX);
                if attempt + 1 >= RETRY_MAX_ATTEMPTS {
                    // Keine Antwort vorhanden: Request + Fehlerkette ablegen.
                    let debug = dump_debug("netz", &ep.model, &url, &body, None, Some(&summary));
                    // Fehler läuft bereits als Step::Err → WorkerEvent::Error in
                    // die UI (Chat); ein Konsolen-Print würde das TUI zerschießen.
                    return Step::Err(with_debug(format!("Netzwerkfehler: {chain}"), debug));
                }
                let wait = jitter(retry_delay(attempt));
                let retry_at = Instant::now() + wait;
                let _ = tx.send(WorkerEvent::Retrying(session, truncate(&summary, 80), retry_at));
                if !sleep_with_cancel(cancel, retry_at) {
                    return Step::Cancelled;
                }
                attempt += 1;
                continue;
            }
        };
        if resp.status().is_success() {
            // Diese Shape hat funktioniert → als „bestimmt“ merken, damit
            // nachfolgende Requests (und andere Pfade) direkt dieses Format
            // nutzen, statt bei jedem Turn erneut zu raten.
            api::remember_shape(ep, shape);
            break resp;
        }
        let status = resp.status();
        // `Retry-After` VOR dem Lesen des Bodys merken (`resp.text()` verbraucht
        // die Response und damit auch die Header).
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let raw = resp.text().unwrap_or_default();

        // Format-Fallback: Einmalig die ANDERE API-Shape probieren –
        // (a) bei einem eindeutigen Format-Hinweis in der Fehlermeldung, oder
        // (b) wenn die Shape nur geraten wurde (keine Modell-Metadaten) und der
        //     Server generisch mit 5xx antwortet (er könnte das gesendete
        //     Format schlicht nicht kennen, z. B. ein Responses-Server).
        // Der Wechsel wird NICHT sofort gecacht – nur ein späterer Erfolg
        // (`remember_shape`) bestätigt die neue Shape im Cache.
        if !shape_flipped
            && !cancel.load(Ordering::Relaxed)
            && api::should_try_other_shape(status, &raw, shape, shape_info.determined)
        {
            shape_flipped = true;
            shape = shape.flipped();
            let (u, b) = api::build_body(ep, shape, msgs, with_tools, permission, true, None);
            url = u;
            body = b;
            continue;
        }
        let summary = reasoning_contract_hint(&server_error_summary(&raw, ERROR_SUMMARY_MAX));
        let final_failure = attempt + 1 >= RETRY_MAX_ATTEMPTS
            || !RETRYABLE_STATUS.contains(&status.as_u16())
            || cancel.load(Ordering::Relaxed);
        // Beim endgültigen Aufgeben: Request + Antwort fürs Debuggen ablegen.
        let debug = if final_failure {
            dump_debug(
                &format!("api-{status}"),
                &ep.model,
                &url,
                &body,
                Some(&raw),
                Some(&summary),
            )
        } else {
            None
        };
        // Fehler läuft bereits als Step::Err → WorkerEvent::Error in die UI
        // (Chat, inkl. Debug-Pfad über `with_debug`); ein Konsolen-Print
        // würde das TUI-Layout zerschießen.
        if final_failure {
            return Step::Err(with_debug(
                format!("API error ({status}): {summary}"),
                debug,
            ));
        }
        let summary_line = format!("API error ({status}): {summary}");
        // Server-`Retry-After` exakt ehren (gecappt); sonst Backoff mit Jitter.
        let wait = match retry_after {
            Some(d) => d.min(RETRY_MAX_DELAY),
            None => jitter(retry_delay(attempt)),
        };
        let retry_at = Instant::now() + wait;
        let _ = tx.send(WorkerEvent::Retrying(session, truncate(&summary_line, 80), retry_at));
        if !sleep_with_cancel(cancel, retry_at) {
            return Step::Cancelled;
        }
        attempt += 1;
    };

    // Response-Header der erfolgreichen Antwort festhalten – VOR dem Bewegen
    // von `resp` in den SSE-Leser-Thread (danach wäre die `reqwest::Response`
    // verbraucht). Die Session merkt sich den Satz für den `Alt+H`-Dialog der
    // letzten LLM-Antwort.
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(name, value)| {
            let v = value.to_str().unwrap_or("<binary>").to_string();
            (name.as_str().to_string(), v)
        })
        .collect();
    let _ = tx.send(WorkerEvent::HttpHeaders(session, headers));

    // Nicht-SSE-Antworten (JSON mit 200): Nicht-streamende Antwort oder ein
    // Fehler im Body. Ohne diese Prüfung würde der JSON-Body als SSE gelesen
    // und die echte Fehlermeldung ginge als irreführendes „SSE-Stream ohne
    // [DONE] beendet.“ verloren.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.contains("text/event-stream") {
        let raw = resp.text().unwrap_or_default();
        return handle_nonstream(tx, session, shape, ep, &url, &body, &raw);
    }

    // SSE-Streaming: Der zeilenweise Stream-Lesevorgang läuft in einem
    // Neben-Thread, damit das blockierende `read_line` den Abbruch (Esc) nicht
    // verzögert. Der Haupt-Thread konsumiert die Zeilen mit `recv_timeout` und
    // prüft zwischen zwei Zeilen das Abbruch-Flag – Esc bricht damit innerhalb
    // von ~100 ms ab, statt erst bei der nächsten Server-Zeile.
    let (line_tx, line_rx) = std::sync::mpsc::channel::<Option<String>>();
    let _reader_thread = {
        let mut reader = std::io::BufReader::new(resp);
        std::thread::spawn(move || {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    // Ok(0) = Response-Body verbraucht (EOF) → Signal ans Haupt-SSE.
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line_tx.send(Some(line.clone())).is_err() {
                            break; // Haupt-Thread beendet → aufhören
                        }
                    }
                }
            }
            let _ = line_tx.send(None); // Stream-Ende
        })
    };

    // Responses-Streaming: getypte Events (output_text.delta, 
    // function_call_arguments.delta, response.completed) – komplett andere
    // Verarbeitung als Chat (choices/delta/tool_calls). Der vorhandene
    // Chat-Loop darunter ist der ChatCompletions-Zweig.
    if shape == api::ApiShape::Responses {
        return stream_responses(tx, session, cancel, &line_rx);
    }

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_accs: Vec<ToolCallAcc> = Vec::new();
    let mut usage: Option<Usage> = None;
    // Misst die Completion-Token je Bereich (reasoning/content/tool_calls)
    // über die usage-Inkremente + Byte-Längen der Deltas.
    let mut parts_acc = RoundPartsAccumulator::default();
    let mut done = false;
    // Manche OpenAI-kompatiblen Endpunkte/Proxys zerlegen große Events (z. B.
    // lange `edit`/`write`-Argumente mit mehrzeiligen Blöcken) über mehrere
    // `data:`-Zeilen – jede Zeile für sich ist dann kein gültiges JSON und
    // würde beim zeilenweisen Parsen still verworfen (→ abgeschnittene
    // `arguments` → `sanitize_arguments` → `{}` → „Argument … missing“). Wir
    // puffern daher so lange Fragmente, bis sie zusammen wieder als komplettes
    // Event parsebar sind; die übliche „eine Zeile = ein Event“-Form bleibt
    // dabei unverändert (ein solches Fragment ist sofort gültig und wird direkt
    // verarbeitet).
    let mut pending = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Step::Cancelled;
        }
        let line = match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(l)) => l,
            Ok(None) => break, // Stream/Response-Ende
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue, // Abbruch prüfen
            Err(_) => break,   // Lesethread beendet (weggefallen)
        };

        let data = line.trim();
        // `data:`-Prefix mit oder ohne Leerzeichen (Proxys variieren).
        let Some(payload) = data
            .strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        else {
            continue;
        };

        // `[DONE]` ist ein eigenständiges Event – nur prüfen, wenn kein
        // zusammenhängendes Event-Fragment noch offen ist.
        if pending.is_empty() && payload.trim() == "[DONE]" {
            done = true;
            break;
        }
        // Event-Fragment anhängen und erst verarbeiten, wenn das (ggf. aus
        // mehreren `data:`-Zeilen zusammengesetzte) Event als Ganzes gültiges
        // JSON ist; sonst kommt die nächste `data:`-Zeile dazu.
        let Some(json) = accumulate_sse_event(&mut pending, payload) else {
            continue; // Fragment noch unvollständig
        };

        // Viele OpenAI-kompatible Endpunkte melden Fehler MID-SEAM über ein
        // `{"error": …}`-Event und schließen dann den Stream (mit oder ohne
        // `[DONE]`). Ohne diese Auswertung würde die echte Servermeldung
        // verschluckt – die Antwort würde still leer bleiben bzw. eine
        // irreführende „ohne [DONE]“-Meldung erscheinen.
        if let Some(e) = json.get("error") {
            return Step::Err(format!(
                "API error (stream): {}",
                truncate(&error_message(e), 400)
            ));
        }

        let Some(choice) = json.get("choices").and_then(|c| c.get(0)) else {
            // Nur usage (z. B. finaler Chunk ohne choices) → trotzdem anwenden.
            apply_usage_if_any(tx, session, &mut usage, &mut parts_acc, &json);
            continue;
        };
        // Fehlendes `index` (manche Proxies lassen es weg) = Delta 0; nur ein
        // vorhandener und von 0 verschiedener Index gehört nicht zu uns.
        if choice
            .get("index")
            .and_then(|i| i.as_u64())
            .is_some_and(|i| i != 0)
        {
            apply_usage_if_any(tx, session, &mut usage, &mut parts_acc, &json);
            continue;
        }
        let Some(delta) = choice.get("delta") else {
            apply_usage_if_any(tx, session, &mut usage, &mut parts_acc, &json);
            continue;
        };

        // Thinking-/Reasoning-Fragmente (z. B. deepseek-reasoner) live senden.
        // Neben dem String-Fall werden auch Objekt-/Array-Formen (z. B.
        // `{"text": …}` bei manchen Proxies) extractiert, damit das
        // `reasoning_content` des assistant-Tool-Calls nie still verloren
        // geht (Thinking-Mode-Vertrag).
        for key in ["reasoning_content", "reasoning"] {
            if let Some(part) = reasoning_fragment(delta.get(key)) {
                parts_acc.track_reasoning(part.len() as u64);
                reasoning.push_str(&part);
                let _ = tx.send(WorkerEvent::Reasoning(session, part));
            }
        }
        let Some(content_delta) = delta.get("content").and_then(|c| c.as_str()) else {
            apply_tool_delta_measured(&mut tool_accs, delta, &mut parts_acc);
            apply_usage_if_any(tx, session, &mut usage, &mut parts_acc, &json);
            continue; // Role-/Gedanken-Delta oder leere Chunks
        };
        if !content_delta.is_empty() {
            parts_acc.track_content(content_delta.len() as u64);
            content.push_str(content_delta);
            apply_tool_delta_measured(&mut tool_accs, delta, &mut parts_acc);
            let _ = tx.send(WorkerEvent::Chunk(session, content_delta.to_string()));
        } else {
            apply_tool_delta_measured(&mut tool_accs, delta, &mut parts_acc);
        }
        // usage NACH dem Delta derselben Zeile: Das Inkrement gehört zu den
        // Deltas, die SEIT dem letzten usage erzeugt wurden (inkl. dieses).
        apply_usage_if_any(tx, session, &mut usage, &mut parts_acc, &json);
    }

    if !done {
        return Step::Err("SSE-Stream ohne [DONE] beendet.".into());
    }

    // Verbindungs-Pool: Nach [DONE] wird der Rest des Streams noch zu Ende
    // gelesen (der Lesevorgang läuft ohnehin im Neben-Thread weiter). Damit die
    // Response verbraucht ist und hyper die Verbindung als Keep-Alive
    // wiederverwenden kann (statt RST_STREAM / „Client disconnected."), werden
    // hier noch kurz die Rest-Zeilen konsumiert, bis der Lesethread das EOF
    // meldet – oder bis Esc (Abbruch) bzw. das Zeitfenster `DRAIN_AFTER_DONE`
    // dazwischenfunken.
    let deadline = Instant::now() + DRAIN_AFTER_DONE;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match line_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Some(_)) => {}  // Rest verwerfen (nur für Keep-Alive)
            Ok(None) => break, // vollständig geleert → Verbindung im Pool
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let assistant =
        |content: Option<String>, reasoning: Option<String>, calls: Option<Vec<WireToolCall>>| {
            WireMessage {
                role: "assistant".into(),
                content,
                // Thinking-Mode: Gedanken müssen an die API zurückgegeben werden.
                reasoning_content: reasoning,
                tool_calls: calls,
                tool_call_id: None,
            }
        };

    if tool_accs.is_empty() {
        // Keine Werkzeug-Aufrufe → finale Runde.
        return Step::Final {
            usage,
            parts: parts_acc.parts().clone(),
        };
    }

    let mut tools = Vec::new();
    let mut calls = Vec::new();
    for acc in tool_accs.iter() {
        // Unvollständiger Funktions-Aufruf: fehlt der Name, ist er nicht
        // ausführbar → überspringen (sonst gäbe es eine leere `tool_calls`-
        // Message mit `content: null`, was der Endpunkt als „content or
        // tool_calls must be set“ → 400 ablehnt).
        if acc.name.is_empty() {
            continue;
        }
        // Fehlt die `id` (manche Streams liefern sie erst verspätet oder gar
        // nicht), wird eine stabile lokale id synthetisiert – sie muss nur
        // mit `tool_call_id` in der Folge-Runde zusammenpassen.
        let id = if acc.id.is_empty() {
            format!("call_aidev_{}", acc.index)
        } else {
            acc.id.clone()
        };
        // Thinking-Mode-Stream liefert gelegentlich abgeschnittene oder leere
        // `arguments` → auf gültiges JSON normalisieren, sonst 400 beim
        // Zurücksenden („function.arguments must be valid JSON").
        let args = sanitize_arguments(&acc.arguments);
        tools.push(ToolInvocation {
            id: id.clone(),
            name: acc.name.clone(),
            arguments: args.clone(),
        });
        calls.push(WireToolCall {
            id,
            ty: "function".into(),
            function: WireFunction {
                name: acc.name.clone(),
                arguments: args,
            },
        });
    }

    // Per-Sektion gemessene Completion-Tokens aus dem Streaming; `tool_calls`
    // deckungsgleich mit dem tools-Filter (leere Namen werden übersprungen),
    // damit die Reihenfolge zu den tatsächlich ausgeführten Tools passt.
    let tool_parts: Vec<u64> = tool_accs
        .iter()
        .enumerate()
        .filter(|(_, acc)| !acc.name.is_empty())
        .map(|(i, _)| parts_acc.parts().tool_calls.get(i).copied().unwrap_or(0))
        .collect();
    let parts = CompletionParts {
        reasoning: parts_acc.parts().reasoning,
        content: parts_acc.parts().content,
        tool_calls: tool_parts,
    };

    // Nur wenn tatsächlich ausführbare Aufrufe übrig sind, ist das eine
    // Werkzeug-Runde; ansonsten wie eine finale Runde ohne Inhalt behandeln.
    if calls.is_empty() {
        // (Hinweis: eine „ins Leere gelaufene" Tool-Runde wird still als
        // Final-Runde behandelt; ein Konsolen-Print würde das TUI zerschießen.)
        return Step::Final { usage, parts };
    }

    Step::Tools {
        assistant: assistant(
            if content.is_empty() {
                None
            } else {
                Some(content)
            },
            if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            Some(calls),
        ),
        tools,
        usage,
        parts,
    }
}

/// Nicht-SSE-Antwort mit 200: nicht-streamende Antwort oder Fehler im Body –
/// für beide API-Shapes (Chat `choices[0].message.content` vs Responses
/// `output`/`output_text`). Behandelt eine vorhandene Antwort wie eine
/// einmalige Chunk-Nachricht für die UI.
fn handle_nonstream(
    tx: &Sender<WorkerEvent>,
    session: usize,
    shape: api::ApiShape,
    ep: &ResolvedEndpoint,
    url: &str,
    body: &Value,
    raw: &str,
) -> Step {
    match serde_json::from_str::<Value>(raw) {
        Ok(json) => {
            if let Some(e) = json.get("error") {
                let summary = take_head(&error_message(e), ERROR_SUMMARY_MAX);
                let debug = dump_debug(
                    "api-200",
                    &ep.model,
                    url,
                    body,
                    Some(raw),
                    Some(&summary),
                );
                return Step::Err(with_debug(format!("API error (200): {summary}"), debug));
            }
            // Non-Streaming-Fallback: Antworttext als einmalige Runde behandeln.
            if let Some(content) = api::content_from_nonstream(shape, &json) {
                if !content.is_empty() {
                    let _ = tx.send(WorkerEvent::Chunk(session, content.clone()));
                }
                return Step::Final {
                    usage: api::parse_usage(&json),
                    parts: CompletionParts::default(),
                };
            }
            let summary = take_head(raw, ERROR_SUMMARY_MAX);
            let debug = dump_debug("api-200", &ep.model, url, body, Some(raw), Some(&summary));
            Step::Err(with_debug(format!("API error (200): {summary}"), debug))
        }
        Err(_) => {
            let summary = take_head(raw, ERROR_SUMMARY_MAX);
            let debug = dump_debug("api-200", &ep.model, url, body, Some(raw), Some(&summary));
            Step::Err(with_debug(
                format!("API error (200): neither SSE nor JSON — {summary}"),
                debug,
            ))
        }
    }
}

/// Findet (oder legt an) den `ToolCallAcc`-Slot für einen `output_index`.
fn ensure_tool_slot(accs: &mut Vec<ToolCallAcc>, index: usize) -> &mut ToolCallAcc {
    match accs.iter().position(|a| a.index == index) {
        Some(pos) => &mut accs[pos],
        None => {
            accs.push(ToolCallAcc {
                index,
                ..ToolCallAcc::default()
            });
            let last = accs.len() - 1;
            &mut accs[last]
        }
    }
}

/// Responses-API-Streaming: verarbeitet die getypten SSE-Events von
/// `POST /responses` (`stream: true`). Die Datenzeilen tragen ein `type`-Feld;
/// relevante Events:
/// - `response.output_text.delta` / `response.reasoning_*_text.delta`
/// - `response.output_item.added` (function_call: Name/ID)
/// - `response.function_call_arguments.delta` / `.done`
/// - `response.completed` (finale Antwort inkl. `usage`)
///
/// Fehler (`error`-Event, `response.failed`) beenden als `Step::Err`;
/// `response.incomplete` meldet einen Abbruch.
fn stream_responses(
    tx: &Sender<WorkerEvent>,
    session: usize,
    cancel: &AtomicBool,
    line_rx: &std::sync::mpsc::Receiver<Option<String>>,
) -> Step {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_accs: Vec<ToolCallAcc> = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut parts_acc = RoundPartsAccumulator::default();
    let mut done = false;
    let mut aborted: Option<String> = None;
    let mut pending = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Step::Cancelled;
        }
        let line = match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(l)) => l,
            Ok(None) => break, // Stream/Response-Ende
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue, // Abbruch prüfen
            Err(_) => break,   // Lesethread beendet (weggefallen)
        };

        let data = line.trim();
        // `data:`-Prefix mit oder ohne Leerzeichen (Proxys variieren).
        let Some(payload) = data
            .strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        else {
            continue;
        };

        if pending.is_empty() && payload.trim() == "[DONE]" {
            done = true;
            break;
        }
        let Some(json) = accumulate_sse_event(&mut pending, payload) else {
            continue; // Fragment noch unvollständig
        };

        let ty = json.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "error" => {
                let msg = json
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown responses stream error");
                return Step::Err(format!(
                    "API error (responses stream): {}",
                    truncate(msg, 400)
                ));
            }
            "response.failed" => {
                let msg = json
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("response failed");
                return Step::Err(format!(
                    "API error (responses stream): {}",
                    truncate(msg, 400)
                ));
            }
            "response.incomplete" => {
                aborted = Some("response incomplete (aborted or inference limit)".into());
                break;
            }
            "response.output_text.delta" => {
                if let Some(d) = json.get("delta").and_then(|d| d.as_str()) {
                    parts_acc.track_content(d.len() as u64);
                    content.push_str(d);
                    let _ = tx.send(WorkerEvent::Chunk(session, d.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = json.get("delta").and_then(|d| d.as_str()) {
                    parts_acc.track_reasoning(d.len() as u64);
                    reasoning.push_str(d);
                    let _ = tx.send(WorkerEvent::Reasoning(session, d.to_string()));
                }
            }
            "response.output_item.added" => {
                let item = json.get("item");
                let is_function = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("function_call");
                if is_function {
                    let index = json
                        .get("output_index")
                        .and_then(|i| i.as_u64())
                        .map(|n| n as usize)
                        .unwrap_or(0);
                    let slot = ensure_tool_slot(&mut tool_accs, index);
                    if let Some(id) = item
                        .and_then(|i| i.get("call_id"))
                        .and_then(|x| x.as_str())
                    {
                        if slot.id.is_empty() {
                            slot.id = id.to_string();
                        }
                    }
                    if let Some(name) = item.and_then(|i| i.get("name")).and_then(|x| x.as_str()) {
                        if slot.name.is_empty() {
                            slot.name = name.to_string();
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = json
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(0);
                if let Some(d) = json.get("delta").and_then(|d| d.as_str()) {
                    let slot = ensure_tool_slot(&mut tool_accs, index);
                    slot.arguments.push_str(d);
                    parts_acc.track_tool(index, d.len() as u64);
                }
            }
            "response.function_call_arguments.done" => {
                let index = json
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(0);
                if let Some(args) = json.get("arguments").and_then(|a| a.as_str()) {
                    let slot = ensure_tool_slot(&mut tool_accs, index);
                    // Falls Deltas unvollständig waren: vollständige Arguments setzen.
                    if slot.arguments.is_empty() && !args.is_empty() {
                        slot.arguments = args.to_string();
                    }
                }
            }
            "response.completed" => {
                if let Some(u) = api::parse_usage(&json) {
                    usage = Some(u);
                    parts_acc.apply_usage(u.completion_tokens);
                    let _ = tx.send(WorkerEvent::UsageUpdate(session, u.total_tokens));
                }
                done = true;
                break;
            }
            // Restliche Lebenszyklus-/Metadaten-Events ignorieren.
            _ => {}
        }
    }

    if !done {
        if let Some(e) = aborted {
            return Step::Err(e);
        }
        return Step::Err("SSE-Stream ohne Abschluss-Event beendet.".into());
    }

    // Verbindungs-Drain (analog zum Chat-Pfad): Rest-Zeilen konsumieren, damit
    // hyper die Verbindung als Keep-Alive wiederverwenden kann.
    let deadline = Instant::now() + DRAIN_AFTER_DONE;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match line_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Some(_)) => {}  // Rest verwerfen (nur für Keep-Alive)
            Ok(None) => break, // vollständig geleert → Verbindung im Pool
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    finish_round(content, reasoning, tool_accs, usage, parts_acc)
}

/// Baut aus den gesammelten Stream-Ergebnissen (Text, Reasoning, Tool-Calls,
/// Usage, gemessene Parts) den finalen `Step`. Gemeinsam für Chat- und
/// Responses-Pfad: keine Tools → `Final`, sonst `Tools` mit der fertigen
/// assistant-Message inkl. `reasoning_content` (Thinking-Mode-Vertrag).
#[allow(clippy::too_many_arguments)]
fn finish_round(
    content: String,
    reasoning: String,
    tool_accs: Vec<ToolCallAcc>,
    usage: Option<Usage>,
    parts_acc: RoundPartsAccumulator,
) -> Step {
    let assistant =
        |content: Option<String>, reasoning: Option<String>, calls: Option<Vec<WireToolCall>>| {
            WireMessage {
                role: "assistant".into(),
                content,
                // Thinking-Mode: Gedanken müssen an die API zurückgegeben werden.
                reasoning_content: reasoning,
                tool_calls: calls,
                tool_call_id: None,
            }
        };

    if tool_accs.is_empty() {
        // Keine Werkzeug-Aufrufe → finale Runde.
        return Step::Final {
            usage,
            parts: parts_acc.parts().clone(),
        };
    }

    let mut tools = Vec::new();
    let mut calls = Vec::new();
    for acc in tool_accs.iter() {
        // Unvollständiger Funktions-Aufruf: fehlt der Name, ist er nicht
        // ausführbar → überspringen (sonst gäbe es eine leere `tool_calls`-
        // Message mit `content: null`, was der Endpunkt als „content or
        // tool_calls must be set“ → 400 ablehnt).
        if acc.name.is_empty() {
            continue;
        }
        // Fehlt die `id` (manche Streams liefern sie erst verspätet oder gar
        // nicht), wird eine stabile lokale id synthetisiert – sie muss nur
        // mit `tool_call_id` in der Folge-Runde zusammenpassen.
        let id = if acc.id.is_empty() {
            format!("call_aidev_{}", acc.index)
        } else {
            acc.id.clone()
        };
        // Thinking-Mode-Stream liefert gelegentlich abgeschnittene oder leere
        // `arguments` → auf gültiges JSON normalisieren, sonst 400 beim
        // Zurücksenden („function.arguments must be valid JSON").
        let args = sanitize_arguments(&acc.arguments);
        tools.push(ToolInvocation {
            id: id.clone(),
            name: acc.name.clone(),
            arguments: args.clone(),
        });
        calls.push(WireToolCall {
            id,
            ty: "function".into(),
            function: WireFunction {
                name: acc.name.clone(),
                arguments: args,
            },
        });
    }

    // Per-Sektion gemessene Completion-Tokens aus dem Streaming; `tool_calls`
    // deckungsgleich mit dem tools-Filter (leere Namen werden übersprungen),
    // damit die Reihenfolge zu den tatsächlich ausgeführten Tools passt.
    let tool_parts: Vec<u64> = tool_accs
        .iter()
        .enumerate()
        .filter(|(_, acc)| !acc.name.is_empty())
        .map(|(i, _)| parts_acc.parts().tool_calls.get(i).copied().unwrap_or(0))
        .collect();
    let parts = CompletionParts {
        reasoning: parts_acc.parts().reasoning,
        content: parts_acc.parts().content,
        tool_calls: tool_parts,
    };

    // Nur wenn tatsächlich ausführbare Aufrufe übrig sind, ist das eine
    // Werkzeug-Runde; ansonsten wie eine finale Runde ohne Inhalt behandeln.
    if calls.is_empty() {
        // (Hinweis: eine „ins Leere gelaufene" Tool-Runde wird still als
        // Final-Runde behandelt; ein Konsolen-Print würde das TUI zerschießen.)
        return Step::Final { usage, parts };
    }

    Step::Tools {
        assistant: assistant(
            if content.is_empty() {
                None
            } else {
                Some(content)
            },
            if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            Some(calls),
        ),
        tools,
        usage,
        parts,
    }
}

/// Extrahiert ein Reasoning-Fragment aus einem Delta-Wert. Unterstützt
/// Strings (`"Gedanken"`), Objekte (`{"text":"…"}`, `{"content":"…"}`) und
/// Arrays von Strings/Objekten – liefert `None` für leer/unbekannt.
pub(crate) fn reasoning_fragment(value: Option<&Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::String(_) => None,
        Value::Object(map) => {
            for key in ["text", "content", "value", "tokens"] {
                if let Some(frag) = reasoning_fragment(map.get(key)) {
                    return Some(frag);
                }
            }
            None
        }
        Value::Array(arr) => {
            let mut out = String::new();
            let mut any = false;
            for item in arr {
                if let Some(part) = reasoning_fragment(Some(item)) {
                    out.push_str(&part);
                    any = true;
                }
            }
            (any).then_some(out)
        }
        _ => None,
    }
}

pub(crate) fn parse_usage(json: &Value) -> Option<Usage> {
    // Chat- (`prompt_tokens`/`completion_tokens`) und Responses-Usage
    // (`input_tokens`/`output_tokens`) werden zentral in `api` gelesen.
    api::parse_usage(json)
}

/// Verteilt `total` proportional zu `weights`; der letzte positive Anteil
/// schluckt den Rundungsrest, damit die Summe EXAKT `total` ergibt.
pub(crate) fn distribute_weights(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum: u128 = weights.iter().map(|&w| w as u128).sum();
    if total == 0 || sum == 0 {
        return vec![0; weights.len()];
    }
    let mut out: Vec<u64> = weights
        .iter()
        .map(|&w| (total as u128 * w as u128 / sum) as u64)
        .collect();
    let used: u128 = out.iter().map(|&v| v as u128).sum();
    if let Some(i) = weights.iter().rposition(|&w| w > 0) {
        out[i] += (total as u128 - used) as u64;
    }
    out
}

/// Misst die Completion-Token je Bereich einer einzelnen HTTP-Runde beim
/// SSE-Streaming: die angefallenen Bytes der aktiven Sektionen werden gezählt,
/// und bei jedem `usage`-Event wird das completion-Inkrement proportional zu
/// diesen Bytes verteilt (danach werden die Zähler zurückgesetzt).
///
/// Wichtig für die Korrektheit: Das Delta MUSS VOR dem usage derselben Zeile
/// verarbeitet werden (sonst wird das Inkrement auf die Bytes des VORIGEN
/// Events verteilt). Außerdem wird eine „aktive Sektion" mitgeführt: Ein
/// Tool-Kopf-Delta (id/name, leere Argumente) erzeugt 0 messbare Bytes, obwohl
/// der Server dafür Completion-Tokens zählt – ein solches Inkrement wird dann
/// per Fallback komplett der zuletzt berührten Sektion zugeordnet statt
/// verloren zu gehen.
#[derive(Debug, Clone, Default)]
pub(crate) struct RoundPartsAccumulator {
    bytes_reasoning: u64,
    bytes_content: u64,
    /// Byte je Tool-Slot (Index = Position in `tool_accs`), seit letztem usage.
    bytes_tool: Vec<u64>,
    /// Zuletzt von einem Delta berührte Sektion (auch bei 0 Bytes gesetzt).
    active: Option<Section>,
    parts: CompletionParts,
    last_completion: u64,
}

#[derive(Debug, Clone, Copy)]
enum Section {
    Reasoning,
    Content,
    Tool(usize),
}

impl RoundPartsAccumulator {
    pub(crate) fn track_reasoning(&mut self, bytes: u64) {
        self.bytes_reasoning += bytes;
        self.active = Some(Section::Reasoning);
    }
    pub(crate) fn track_content(&mut self, bytes: u64) {
        self.bytes_content += bytes;
        self.active = Some(Section::Content);
    }

    /// Zählt die neu angefallenen Argument-Bytes eines Tool-Deltas am Slot
    /// `i` (Position in `tool_accs`, NICHT unbedingt der Stream-Index) und
    /// markiert diesen Slot als aktiv – auch wenn `bytes == 0`.
    pub(crate) fn track_tool(&mut self, i: usize, bytes: u64) {
        if self.bytes_tool.len() <= i {
            self.bytes_tool.resize(i + 1, 0);
        }
        self.bytes_tool[i] += bytes;
        self.active = Some(Section::Tool(i));
    }

    /// Verarbeitet ein gelesenes `usage`-Event: verteilt das
    /// completion-Inkrement (kumuliert seit dem letzten Stand) proportional
    /// nach den seit dem letzten usage gemessenen Bytes und setzt die Zähler
    /// danach zurück. `completion_tokens` ist der kumulierte Endwert des
    /// aktuellen usage.
    ///
    /// Hat das Inkrement messbare Bytes, wird proportional verteilt (der
    /// letzte positive Anteil erhält den Rundungsrest). Sind ALLE Bytes 0
    /// (z. B. nur ein Tool-Kopf/leeres Delta dazwischen), geht das Inkrement
    /// komplett an die zuletzt berührte Sektion.
    pub(crate) fn apply_usage(&mut self, completion_tokens: u64) {
        let inc = completion_tokens.saturating_sub(self.last_completion);
        self.last_completion = completion_tokens;

        let mut weights: Vec<u64> = vec![self.bytes_reasoning, self.bytes_content];
        weights.extend(self.bytes_tool.iter().copied());
        let sum: u64 = weights.iter().sum();

        if inc > 0 && sum == 0 {
            // Keine Messwerte in diesem Fenster → Fallback auf die aktive
            // Sektion (z. B. Tool-Kopf-Delta mit leerem Argument).
            match self.active {
                Some(Section::Reasoning) => self.parts.reasoning += inc,
                Some(Section::Content) => self.parts.content += inc,
                Some(Section::Tool(i)) => {
                    if self.parts.tool_calls.len() <= i {
                        self.parts.tool_calls.resize(i + 1, 0);
                    }
                    self.parts.tool_calls[i] += inc;
                }
                None => {} // nichts aktiv → nicht zuordbar (selten)
            }
        } else if inc > 0 {
            let shares = distribute_weights(inc, &weights);
            self.parts.reasoning += shares[0];
            self.parts.content += shares[1];
            for (k, &t) in shares[2..].iter().enumerate() {
                if self.parts.tool_calls.len() <= k {
                    self.parts.tool_calls.resize(k + 1, 0);
                }
                self.parts.tool_calls[k] += t;
            }
        }

        // Bytes zurücksetzen; `active` bleibt für usage-only Events erhalten
        // (der Schluss-Inkrement gehört zur zuletzt erzeugten Sektion).
        self.bytes_reasoning = 0;
        self.bytes_content = 0;
        self.bytes_tool.clear();
    }

    pub(crate) fn parts(&self) -> &CompletionParts {
        &self.parts
    }
}

/// Wendet ein `tool_calls`-Delta an und misst dabei die neu angefallenen
/// Argument-Bytes je Tool-Slot für die Completion-Verteilung. Wichtig: Auch
/// ein Tool-Kopf-Delta (id/name, noch leere Argumente → 0 Bytes) ruft
/// `track_tool` auf, damit der Slot als „aktiv“ markiert wird – sonst ginge
/// das usage-Inkrement des Kopfes im 0-Byte-Fallback verloren.
fn apply_tool_delta_measured(
    accs: &mut Vec<ToolCallAcc>,
    delta: &Value,
    parts_acc: &mut RoundPartsAccumulator,
) {
    if delta.get("tool_calls").is_none() {
        return;
    }
    let before: Vec<usize> = accs.iter().map(|a| a.arguments.len()).collect();
    apply_tool_delta(accs, delta);
    for (i, acc) in accs.iter().enumerate() {
        let prev = before.get(i).copied().unwrap_or(0);
        let added = acc.arguments.len().saturating_sub(prev) as u64;
        // Track auch mit `added == 0` (Header) → setzt die aktive Sektion.
        parts_acc.track_tool(i, added);
    }
}

/// Wendet das `usage`-Feld eines Events an (falls vorhanden) – NACH dem
/// Delta-Tracking, damit das Inkrement dieses Events auch dessen Bytes sieht.
/// Wird im Stream ein usage erkannt, reicht es die serverbestätigte
/// `total_tokens` sofort als `UsageUpdate` an die UI weiter, damit die
/// Statusleiste den aktuellen Kontextstand live anzeigen kann.
fn apply_usage_if_any(
    tx: &Sender<WorkerEvent>,
    session: usize,
    usage: &mut Option<Usage>,
    acc: &mut RoundPartsAccumulator,
    json: &Value,
) {
    if let Some(u) = parse_usage(json) {
        *usage = Some(u);
        acc.apply_usage(u.completion_tokens);
        let _ = tx.send(WorkerEvent::UsageUpdate(session, u.total_tokens));
    }
}
