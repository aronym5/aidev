//! Kontext-Kompaktierung: Planung, Entscheidung und Ausführung.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde_json::Value;

use super::api::ApiShape;
use super::helpers::{
    dump_debug, error_chain, error_message, server_error_summary, with_debug, ERROR_SUMMARY_MAX,
};
use super::http::{accumulate_sse_event, shared_client};
use super::wire::{WireMessage, WireTokens};
use super::{Usage, WorkerEvent};
use crate::config::{Config, ResolvedEndpoint};

/// System-Prompt für den separaten Kompaktierungs-Aufruf: fasst den ältesten
/// Teil der Historie zu einer präzisen Zusammenfassung zusammen.
const COMPACT_SYSTEM_PROMPT: &str = "\
You are a context compressor. Summarize the following chat history as \
precise context for continuation. Keep: user intent and given tasks, \
decisions made and their reasons, affected files/paths, errors and their \
fixes, and open points. Write in reporting form (not first person), no \
dialogue, no filler. Reply only with the summary in the users language.";

/// Abschließende User-Nachricht des Kompaktierungs-Aufrufs: Sie stellt die
/// Zusammenfassung als EXPLIZITE Aufgabe, statt die Historie als offenes
/// Gespräch enden zu lassen. Ohne sie läge die letzte Nachricht in der
/// `assistant`-Rolle – viele Modelle „reden dann im Chat weiter“ statt
/// zusammenzufassen (kopierte Verbatim-Fragmente oder eine abbrechende,
/// leere Antwort waren die Folge).
const COMPACT_REQUEST: &str = "\
Summarize the chat history above. Reply only with the precise summary - \
no introduction, no comment, and no continuation of the dialogue. Write the \
summary in the language of the conversation / the user.";

/// Auslöser der Kompaktierung – bestimmt die Entscheidung über den Schnitt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactTrigger {
    /// Automatisch direkt nach Abschluss der finalen Antwort – läuft parallel
    /// zur Eingabe des nächsten Prompts.
    AutoTurn,
    /// Vor dem Senden eines neuen Turns (Fallback, wenn die Auto-Kompaktierung
    /// nicht gegriffen hat).
    Proactive,
    /// Bei einem `context_length`-Fehler mitten im Turn (einmalig).
    Reactive,
    /// Manuell per Slash-Befehl `/compact`.
    Manual,
}

/// Ermittelt den Index, ab dem die letzten `keep_turns` Turns bei der
/// Kompaktierung unangetastet bleiben. Ein Turn wird im Wire-Format an seiner
/// `user`-Nachricht gezählt – die zugehörige Assistant-Runde samt `tool`-
/// Nachrichten gehört dazu (eine mehrteilige Tool-Runde zählt NICHT als
/// mehrere Turns, sonst wichen Wire- und Session-Grenze auseinander).
/// Identisch zur Zählung von `compact_boundary` auf der Session-Seite
/// (`Chat.order`): so verwenden Wire-Kompaktierung und der Einbau des
/// `Archive`-Events in die Session-Historie bei JEDEM Auslöser (auto nach
/// Turn, proaktiv, manuell, reaktiv) dieselbe Grenze. Die letzte, ggf. noch
/// unbeantwortete `user`-Nachricht zählt immer mit. `manual`-Nachrichten
/// (`/run`) gibt es im Wire-Format nicht (sie wandern nicht in die API) und
/// sind daher ohnehin nicht Teil der Projektion.
///
/// Semantik: `keep_turns = k` bedeutet, dass GENAU die letzten `k` Turns
/// überleben (Schnitt am `k`-letzten User). Damit ist `k = 1` (nur der letzte
/// Turn bleibt) exakt derselbe normale Fall wie jedes andere `k` – kein
/// Sonderfall. Größeres `k` lässt mehr überleben (schwächerer Schnitt).
/// `keep_turns = 0` verhält sich wie `keep_turns = 1`: der laufende Turn bleibt immer stehen.
pub(crate) fn wire_compact_boundary(msgs: &[WireMessage], keep_turns: usize) -> usize {
    let mut users = 0;
    let mut i = msgs.len();
    while i > 0 {
        i -= 1;
        if msgs[i].role == "user" {
            users += 1;
            if users == keep_turns.max(1) {
                return i;
            }
        }
    }
    0
}

/// Bauermuster für `context_length`-Meldungen (Status-Codes variieren, daher
/// wird der Text gescannt).
pub(crate) fn looks_like_context_error(msg: &str) -> bool {
    let l = msg.to_lowercase();
    [
        "context length",
        "maximum context",
        "max context",
        "prompt is too long",
        "too many tokens",
        "context_length",
        "context window",
        "token limit",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// Suche das größte `keep_turns`, das noch einen ECHTEN Schnitt ergibt –
/// d.h. mindestens ein `user`-Turn wird archiviert. (`keep_turns = users`
/// ließe gar nichts archivieren → boundary 0; daher ist der größte sinnvolle
/// Wert `users - 1`: die letzten `users - 1` Turns überleben, nur der älteste
/// fällt weg. Für `keep_turns = 1` bleibt nur der letzte Turn – exakt
/// derselbe normale Fall wie jedes andere `keep`.)
fn possible_max_keep(msgs: &[WireMessage]) -> usize {
    let users = msgs.iter().filter(|m| m.role == "user").count();
    users.saturating_sub(1)
}

/// Marker, mit dem eine Kompaktierungs-Summary in der Wire-Projektion beginnt
/// (siehe `compact_chat_messages`). Er ist zugleich das Erkennungsmerkmal für
/// „an dieser Stelle wurde bereits geschnitten“.
const SUMMARY_PREFIX: &str = "[Compressed history - ";

/// Steht am Kopf der Wire-Projektion die Summary einer früheren Kompaktierung?
/// `api_messages` projiziert erst ab dem letzten `Archive`-Event, dessen
/// Summary als erste `user`-Nachricht erscheint – der Kopf ist also genau die
/// Stelle, an der zuletzt geschnitten wurde.
fn has_head_summary(msgs: &[WireMessage]) -> bool {
    msgs.iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .is_some_and(|c| c.trim_start().starts_with(SUMMARY_PREFIX))
}

/// Obergrenze des `keep_turns`-Bereichs, aus dem gewählt wird: der schwächste
/// Schnitt, der noch etwas Neues archiviert. Grundlage ist `possible_max_keep`
/// – bei einer Summary am Kopf fällt der schwächste Schnitt jedoch weg (siehe
/// unten), weil er die letzte Kompaktierung lediglich wiederholen würde.
fn cut_limit(msgs: &[WireMessage]) -> usize {
    let max = possible_max_keep(msgs);
    // Steht die Summary einer früheren Kompaktierung am Kopf, läge `keep == max`
    // exakt auf der Schnittstelle der LETZTEN Kompaktierung: Es würde nichts
    // als die bereits zusammengefasste Historie erneut zusammenfassen
    // (Kompression auf Kompression, wachsender Informationsverlust) und dabei
    // keinen einzigen echten Turn dazugewinnen. Dieser Schnitt fällt daher
    // ersatzlos weg – es muss immer mindestens ein weiterer Turn neu
    // archiviert werden. Die Grenze selbst bleibt bei `possible_max_keep`:
    // ein `keep > max` ergäbe über `wire_compact_boundary` gar keinen Schnitt.
    if has_head_summary(msgs) {
        max.saturating_sub(1)
    } else {
        max
    }
}

/// Ob die Wire-Projektion überhaupt etwas zu kompaktieren lässt – also ob EIN
/// zulässiger Schnitt existiert (`1..=cut_limit`). Ist das nicht der Fall, wird
/// eine Kompaktierung gar nicht erst versucht, damit nicht nur ein
/// „Compacting“-Blip und die Meldung „keine zu kompaktierenden Turns“ entstehen:
/// bei einem einzigen laufenden Query ebenso wie direkt nach einer
/// Kompaktierung, wenn seitdem kein weiterer Turn hinzugekommen ist.
pub(crate) fn can_compact(msgs: &[WireMessage]) -> bool {
    cut_limit(msgs) > 0
}

/// Ein möglicher Schnitt: wo der Schnitt läge (`boundary`), wie viel wegfällt
/// (`dropped_tokens`) und was übrig bliebe (`kept_tokens`). Die Größen kommen
/// je Bestandteil aus dem Event-Log (bestätigte Usage), sonst aus der
/// Zeichen-Heuristik – die Entscheidung kommt damit ohne LLM-Aufruf aus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutCandidate {
    pub keep_turns: usize,
    /// Wire-Index, ab dem der überlebende Tail beginnt (0 = kein Schnitt).
    pub boundary: usize,
    /// Anzahl der archivierten Wire-Nachrichten (alles vor `boundary`).
    pub archived_msgs: usize,
    /// Tokens des entfernten Teils.
    pub dropped_tokens: u64,
    /// Tokens des überlebenden Tails.
    pub kept_tokens: u64,
    /// Dieser Schnitt läge auf der Stelle der letzten Kompaktierung (Summary
    /// am Kopf) und ist damit vom Spiel ausgeschlossen – siehe `cut_limit`.
    pub previous_cut: bool,
}

/// Plant alle möglichen Schnitte für `keep_turns = 1..=max_keep`. Es werden
/// nur Kandidaten mit echtem Schnitt (`boundary > 0`) geliefert; Schnitte an
/// der Stelle der letzten Kompaktierung sind als `previous_cut` markiert
/// (siehe `cut_limit`) und werden von `decide_keep` ausgeschlossen.
pub(crate) fn plan_candidates(msgs: &[WireMessage], max_keep: usize) -> Vec<CutCandidate> {
    let limit = cut_limit(msgs);
    let mut out = Vec::new();
    for k in 1..=max_keep {
        let boundary = wire_compact_boundary(msgs, k);
        if boundary == 0 {
            continue;
        }
        // Tokens je Seite = Summe der pro Nachricht bereits bewerteten
        // Bestandteile (`WireTokens::total`); die Entscheidung „bestätigt oder
        // geschätzt“ ist in der Projektion (`api_messages`) schon gefallen.
        out.push(CutCandidate {
            keep_turns: k,
            boundary,
            archived_msgs: boundary,
            dropped_tokens: msgs[..boundary].iter().map(|m| m.tokens.total()).sum(),
            kept_tokens: msgs[boundary..].iter().map(|m| m.tokens.total()).sum(),
            previous_cut: k > limit,
        });
    }
    out
}

/// Zielgröße des erhaltenen Historienteils in Tokens: `context_window` mal
/// `compact_keep_ratio` – von diesem Ziel wird angestrebt, dass ~80 % des
/// Fensters frei werden. Das Ziel wird auf `compact_at` gedeckelt: ein
/// Ziel oberhalb der Auslöseschwelle hieße, dass der Kontext nach der
/// Kompaktierung immer noch über der Schwelle liegt und die Kompaktierung im
/// nächsten Turn sofort wieder auslöst.
fn target_kept_tokens(config: &Config, context_window: u64) -> u64 {
    let ratio = config.compact_keep_ratio.min(config.compact_at);
    (context_window as f64 * ratio) as u64
}

/// Entscheidet über die konkrete Schnittstelle (`keep_turns`) aus den
/// Kandidaten. Liefert `None`, wenn es nichts zu kompaktieren gibt (kein
/// echter Schnitt möglich, oder nur die Stelle der letzten Kompaktierung
/// infrage käme); sonst die gewählte `keep_turns`-Zahl.
///
/// Der Spielraum ist `1..=cut_limit(msgs)`: `keep_turns` zählt die GENAU
/// überlebenden Turns (so bleibt bei `keep_turns = 1` nur der letzte Turn –
/// exakt derselbe normale Fall wie jedes andere `k`). Es wird nie an der Stelle
/// der letzten Kompaktierung erneut geschnitten (nur die vorhandene Summary neu
/// zusammenfassen).
///
/// Kriterium je Auslöser:
/// - **Normal (Auto/Proaktiv/Manuell):** Freiziel – es wird der Schnitt
///   gewählt, dessen verbleibender Tail dem Ziel
///   `context_window × compact_keep_ratio` (Default 20 %) am nächsten kommt,
///   also der Schnitt, der ~80 % des Fensters frei gibt. Ist das Ziel nicht
///   erreichbar, greift zwangsläufig der beste Annäherungswert: liegen alle
///   Tails ÜBER dem Ziel, ist das der stärkste Schnitt (ein einzelner riesiger
///   letzter Turn dominiert den Tail), liegen alle darunter, der schwächste –
///   es wird also nie weniger archiviert als nötig, um ins Ziel zu kommen.
/// - **Reaktiv (context_length-Fehler):** Stärkster Schnitt – kleinstes
///   `keep`, dessen Folge-Kontext sicher unter das VOLLE Fenster
///   (`context_window`) passt, damit der Retry nicht erneut überläuft. Passt
///   keiner, der stärkste überhaupt – die Tokengrößen sind Schätzungen, ein
///   Besserungsversuch ist besser als keiner.
///
/// Bei Gleichstand im Normalfall gewinnt das GRÖßERE `keep_turns`: von
/// zwei gleich weit vom Ziel entfernten Schnitten bleibt beim schwächeren mehr
/// Originalkontext unangetastet.
pub(crate) fn decide_keep(
    msgs: &[WireMessage],
    config: &Config,
    trigger: CompactTrigger,
    context_window: u64,
) -> Option<usize> {
    let candidates = plan_candidates(msgs, possible_max_keep(msgs));
    let selectable: Vec<&CutCandidate> = candidates.iter().filter(|c| !c.previous_cut).collect();
    let budget = config.compact_summary_tokens;
    match trigger {
        // Reaktiv: stärkster Schnitt, der das volle Fenster sicher einhält.
        CompactTrigger::Reactive => selectable
            .iter()
            .filter(|c| c.kept_tokens.saturating_add(budget) <= context_window)
            .min_by_key(|c| c.keep_turns)
            .or_else(|| {
                // Rückfall: kein Kandidat passt – stärkster Schnitt überhaupt.
                selectable.iter().min_by_key(|c| c.keep_turns)
            })
            .map(|c| c.keep_turns),
        // Normal: der Schnitt, dessen Tail dem Freiziel am nächsten liegt.
        // `Reverse` im Schlüssel löst Gleichstände zugunsten des schwächeren
        // Schnitts (mehr unangetasteter Originalkontext).
        _ => {
            let target = target_kept_tokens(config, context_window);
            selectable
                .iter()
                .map(|c| {
                    (
                        c.kept_tokens.abs_diff(target),
                        std::cmp::Reverse(c.keep_turns),
                    )
                })
                .min()
                .map(|(_, std::cmp::Reverse(k))| k)
        }
    }
}

/// Ergebnis des Zusammenfassungs-Aufrufs: Inhalt und Tokenlänge.
pub(crate) struct SummaryResult {
    pub content: String,
    pub tokens: u64,
}

/// Ein separater Zusammenfassungs-Aufruf – in der API-Shape des Endpunkts
/// (Chat Completions `/chat/completions` oder Responses `/responses`).
///
/// **Streamend, mit Werkzeug-Definitionen** – beides ist keine Optimierung,
/// sondern Bedingung bei zen/opencode: der dortige Free Tier beantwortet
/// ausschließlich *streamende* Chat-Completions-Requests mit nicht-leerem
/// `tools`-Array. Ein nicht-streamender Aufruf ohne Tools wird mit
/// `403 {"type":"error","error":{"type":"FreeTierError",…}}` abgewiesen (Live-
/// Messung: dieselbe Historie liefert als `stream:true` + `tools` 200 mit
/// fertiger Zusammenfassung, als `stream:false` 403). Die Tools selbst werden
/// von der Zusammenfassung nie aufgerufen – sie sind nur der Eintritts-
/// Nachweis; Inhalt und `max_tokens`/`max_output_tokens` bleiben wie gehabt.
pub(crate) fn request_summary(
    session: usize,
    client: &reqwest::blocking::Client,
    ep: &ResolvedEndpoint,
    compact_summary_tokens: u64,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
) -> Result<SummaryResult, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("aborted".into());
    }
    if msgs.is_empty() {
        return Err("nothing to compact".into());
    }
    // Wire-Nachrichten um System-Prompt (vorn) und abschließende Aufgabe (hinten)
    // ergänzen – die Abschluss-Message stellt die Zusammenfassung als EXPLIZITE
    // Aufgabe, statt die Historie als offenes Gespräch enden zu lassen. Ohne sie
    // läge die letzte Nachricht in der `assistant`-Rolle – viele Modelle „reden
    // dann im Chat weiter“ statt zusammenzufassen.
    let mut compact_msgs: Vec<WireMessage> = Vec::with_capacity(msgs.len() + 2);
    compact_msgs.push(WireMessage {
        role: "system".into(),
        content: Some(COMPACT_SYSTEM_PROMPT.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens::default(),
    });
    compact_msgs.extend_from_slice(msgs);
    compact_msgs.push(WireMessage {
        role: "user".into(),
        content: Some(COMPACT_REQUEST.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens::default(),
    });

    // API-Shape aus dem Cache ermitteln (Default Chat Completions); der
    // Fallback schaltet bei einem eindeutigen Format-Hinweis ODER einer
    // geratenen Shape mit Server-Fehler einmalig um (analog zum Chat-Pfad).
    let shape_info = super::api::resolve_shape_info(ep);
    let mut shape = shape_info.shape;
    let mut attempts = 0;
    loop {
        // `ForceOnly`: die Zusammenfassung bekommt **nur** die `force_tools`
        // als Dummies – unabhängig von einem gebundenen Kanal und ohne
        // `webfetch`. Bei leerem `force_tools` (Provider ohne Default) entfällt
        // das `tools`-Feld ganz.
        let (url, body) = super::api::build_body(
            ep,
            shape,
            &compact_msgs,
            super::api::ToolOffer::ForceOnly,
            true,
            Some(compact_summary_tokens),
        );
        let mut req = client
            .post(&url)
            .bearer_auth(&ep.api_key)
            .header("user-agent", &ep.user_agent)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        // opencode.ai erwartet zusätzlich `x-opencode-client: cli` +
        // `x-opencode-project: global` samt generierter Request-/Session-IDs –
        // dieselbe Bedingung wie beim Chat-Request (`request_once`), damit beide
        // Pfade konsistent sind. Der `x-opencode-session`-Header bleibt über die
        // ganze Konversation stabil, `x-opencode-request` ist pro Request neu.
        if crate::config::is_opencode_base(&ep.base_url) {
            let sid = super::ident::session_id_for(session);
            req = req
                .header("x-opencode-client", "cli")
                .header("x-opencode-project", "global")
                .header("x-opencode-request", super::ident::message_id())
                .header("x-opencode-session", sid);
        }
        let resp = match req.json(&body).send() {
            Ok(r) => r,
            Err(err) => return Err(format!("Network error: {}", error_chain(&err))),
        };
        if resp.status().is_success() {
            // Diese Shape hat funktioniert → als „bestätigt“ merken (geteilter
            // Cache mit dem Chat-Pfad), damit nachfolgende Aufrufe direkt
            // dieses Format nutzen.
            super::api::remember_shape(ep, shape);
            let raw = resp
                .text()
                .map_err(|err| format!("invalid response: {err}"))?;
            // Angefragt wurde ein Stream (siehe oben): SSE-Fragmente werden zu
            // Text zusammengesetzt; ein vom Proxy ignorierter `stream` bleibt
            // ein normales JSON – beides wertet `summary_from_response` aus.
            let (content, usage) = summary_from_response(shape, &raw)?;
            // `completion_tokens` = Länge der Summary (die Ausgabe des
            // Kompaktierungs-Aufrufs wird später als User-Nachricht Teil des
            // Kontexts). Falls der Endpunkt kein `usage` liefert, wird als
            // Fallback die Zeichen-Schätzung verwendet (`estimate_tokens`).
            let tokens = usage
                .map(|u| u.completion_tokens)
                .unwrap_or_else(|| super::estimate_tokens(&content));
            return Ok(SummaryResult { content, tokens });
        }
        let status = resp.status();
        let raw = resp.text().unwrap_or_default();
        // Format-Fallback: Einmalig die ANDERE Shape probieren – bei eindeutigem
        // Format-Hinweis ODER geratener Shape mit Server-Fehler (5xx).
        if attempts == 0
            && super::api::should_try_other_shape(status, &raw, shape, shape_info.determined)
        {
            shape = shape.flipped();
            attempts += 1;
            continue;
        }
        let summary = server_error_summary(&raw, ERROR_SUMMARY_MAX);
        let debug = dump_debug(
            &format!("summary-{status}"),
            &ep.model,
            &url,
            &body,
            Some(&raw),
            Some(&summary),
        );
        return Err(with_debug(
            format!("API error ({status}): {summary}"),
            debug,
        ));
    }
}
/// Text + `usage` aus der Antwort eines Zusammenfassungs-Aufrufs.
///
/// Der Aufruf wird **gestreamt** angefragt (zen/opencode beantwortet nur
/// streamende Chat-Completions-Requests; ein nicht-streamender Aufruf wird mit
/// `403 FreeTierError` abgewiesen). Die Antwort ist damit normalerweise ein
/// SSE-Stream aus `data:`-Zeilen; Endpunkte, die `stream` ignorieren, liefern
/// ein normales JSON – beides wird hier ausgewertet (rein, damit testbar).
///
/// Chat Completions: `choices[0].delta.content` je Fragment, `[DONE]` als
/// Schlussmarke. Responses: `response.output_text.delta` mit `delta`.
/// Fehler mitten im Stream (`{"error": …}`) werden als `Err` mit der echten
/// Servermeldung durchgereicht – eine leere Antwort wäre ein Rätsel. (Bewusst
/// *kein* `eprintln!`: Der Kompaktierungs-Aufruf läuft im TUI, stderr-Ausgaben
/// würden das Layout zerschießen.)
fn summary_from_response(shape: ApiShape, raw: &str) -> Result<(String, Option<Usage>), String> {
    let kein_text = || "Summary: no answer text in the response.".to_string();
    let is_sse = raw.lines().any(|l| l.trim_start().starts_with("data:"));
    if !is_sse {
        let v: Value =
            serde_json::from_str(raw).map_err(|err| format!("invalid response: {err}"))?;
        if let Some(e) = v.get("error") {
            return Err(format!("API error: {}", error_message(e)));
        }
        let content = super::api::content_from_nonstream(shape, &v).ok_or_else(kein_text)?;
        return Ok((content, super::api::parse_usage(&v)));
    }
    let mut out = String::new();
    let mut usage: Option<Usage> = None;
    let mut pending = String::new();
    for line in raw.lines() {
        let data = line.trim();
        let Some(payload) = data
            .strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            break;
        }
        // Mehrteilige `data:`-Fragmente (große Argumente) erst zusammenführen.
        let Some(json) = accumulate_sse_event(&mut pending, payload) else {
            continue;
        };
        if let Some(e) = json.get("error") {
            return Err(format!("API error (stream): {}", error_message(e)));
        }
        if let Some(u) = super::api::parse_usage(&json) {
            usage = Some(u);
        }
        let part = match shape {
            ApiShape::ChatCompletions => json
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(|c| c.as_str()),
            ApiShape::Responses => json.get("delta").and_then(|d| d.as_str()),
        };
        if let Some(part) = part {
            out.push_str(part);
        }
    }
    if out.trim().is_empty() {
        return Err(kein_text());
    }
    Ok((out, usage))
}

/// Ergebnis einer ausgeführten Kompaktierung:
/// `(neue Wire-Historie, fertige Summary, Summary-Tokens, verwendetes
/// `keep_turns`)`.
pub(crate) type CompactionResult = (Vec<WireMessage>, String, u64, usize);

/// Führt die Kompaktierung für EINEN Auslöser aus (Plan → Entscheidung →
/// Zusammenfassung). Liefert die neue Wire-Historie (`repl`: Summary-User +
/// unveränderter Tail), die fertige Summary (`content`), ihre Token-Zahl und
/// das verwendete `keep_turns`.
///
/// Alle vier Auslöser laufen über diese EINE Quelle:
///
/// - **auto nach Turn** (`spawn_compact`): `repl` wird verworfen, nur
///   `content`/`tokens`/`keep` bauen das `Archive` ein;
/// - **manuell** (`/compact`, `spawn_compact`): wie auto – `repl` verworfen;
/// - **proaktiv** (`spawn_worker` vor dem Turn): `repl` wird als
///   Nachrichtenliste des kommenden Turns verwendet;
/// - **reaktiv** (`spawn_worker` mitten im Tool-Loop): wie proaktiv.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_chat_messages(
    session: usize,
    client: &reqwest::blocking::Client,
    config: &Config,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
    trigger: CompactTrigger,
) -> Result<CompactionResult, String> {
    let Some(keep) = decide_keep(msgs, config, trigger, ep.context_window) else {
        return Err(
            "Nothing to compact – this needs at least two turns.".into(),
        );
    };
    let boundary = wire_compact_boundary(msgs, keep);
    let old = &msgs[..boundary];
    let tail = &msgs[boundary..];
    let res = request_summary(
        session,
        client,
        ep,
        config.compact_summary_tokens,
        old,
        cancel,
    )?;
    let content = format!(
        "{SUMMARY_PREFIX}{} earlier messages]\n\n{}",
        old.len(),
        res.content.trim()
    );
    let mut out = Vec::with_capacity(tail.len() + 1);
    out.push(WireMessage {
        role: "user".into(),
        content: Some(content.clone()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        // Die Summary ist das Ergebnis des Komprimierungs-Aufrufs: dessen
        // gemeldete Tokenzahl ist bestätigt, ohne Messung wird über den Text
        // geschätzt – nie 0, damit die bereits komprimierte Historie in der
        // Planung nicht als kostenlos erscheint.
        tokens: WireTokens {
            content: WireTokens::part(res.tokens, &content),
            ..Default::default()
        },
    });
    out.extend_from_slice(tail);

    Ok((out, content, res.tokens, keep))
}

/// Startet die Kontext-Kompaktierung in einem eigenen Thread – für die
/// manuelle (`/compact`) und die automatische (direkt nach der finalen
/// Antwort, parallel zum Tippen) Kompaktierung. Sendet `Compacting`, dann
/// `Compacted` (mit Summary, Tokenzahl und gewähltem `keep_turns`) oder bei
/// einem Fehler (z. B. nichts zu kompaktieren) `Error`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_compact(
    tx: Sender<WorkerEvent>,
    session: usize,
    config: Config,
    ep: ResolvedEndpoint,
    messages: Vec<WireMessage>,
    cancel: Arc<AtomicBool>,
    trigger: CompactTrigger,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = shared_client();
        let _ = tx.send(WorkerEvent::Compacting(session));
        match compact_chat_messages(session, client, &config, &ep, &messages, &cancel, trigger) {
            Ok((_repl, content, tokens, keep)) => {
                let _ = tx.send(WorkerEvent::Compacted(session, content, tokens, keep));
            }
            Err(err) => {
                let _ = tx.send(WorkerEvent::Error(session, err));
            }
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Kernfall: der Zusammenfassungs-Aufruf wird gestreamt angefragt, also
    /// kommt SSE zurück – der Text muss aus den `delta.content`-Fragmenten
    /// zusammengesetzt werden (nicht aus einem einzelnen JSON-Feld).
    #[test]
    fn sse_stream_wird_zusammengesetzt() {
        let raw = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Aufgabe\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\": \": 3\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":7,\"total_tokens\":17}}\n\n",
            "data: [DONE]\n\n",
        );
        let (text, usage) = summary_from_response(ApiShape::ChatCompletions, raw).expect("Text");
        assert_eq!(text, "Aufgabe: 3");
        assert_eq!(usage.map(|u| u.completion_tokens), Some(7));
    }

    /// Endpunkte, die `stream` ignorieren, liefern ein normales JSON – das muss
    /// weiterhin funktionieren (sonst wäre die Umstellung eine Regression für
    /// alle lokalen/OSS-Endpunkte).
    #[test]
    fn json_antwort_bleibt_auswertbar() {
        let raw = r#"{"choices":[{"message":{"content":"Zusammenfassung"}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
        let (text, usage) = summary_from_response(ApiShape::ChatCompletions, raw).expect("Text");
        assert_eq!(text, "Zusammenfassung");
        assert_eq!(usage.map(|u| u.completion_tokens), Some(2));
    }

    /// Responses-Shape im Stream: `response.output_text.delta`-Events.
    #[test]
    fn responses_stream_wird_ausgewertet() {
        let raw = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Teil \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"eins\"}\n\n",
            "data: [DONE]\n\n",
        );
        let (text, _) = summary_from_response(ApiShape::Responses, raw).expect("Text");
        assert_eq!(text, "Teil eins");
    }

    /// Fehler mitten im Stream dürfen nicht zu „leerer Zusammenfassung"
    /// führen, sondern müssen als Fehler mit der echten Servermeldung
    /// auffallen.
    #[test]
    fn stream_fehler_wird_durchgereicht() {
        let raw = "data: {\"error\":{\"message\":\"rate limited\"}}\n\ndata: [DONE]\n\n";
        let err = summary_from_response(ApiShape::ChatCompletions, raw).expect_err("Fehler");
        assert!(
            err.contains("rate limited"),
            "Servertext muss durchkommen: {err}"
        );
    }

    /// Leerer Stream → Fehler mit der bisherigen Meldung, damit der Nutzer
    /// nicht einen leeren Kontext bekommt.
    #[test]
    fn leerer_stream_ist_fehler() {
        let err = summary_from_response(ApiShape::ChatCompletions, "data: [DONE]\n\n")
            .expect_err("Fehler");
        assert!(err.contains("no answer text"), "{err}");
    }

    /// Der Aufruf MUSS streamen (zen/opencode: sonst `403 FreeTierError`),
    /// zusätzlich begrenzt `max_tokens` die Länge der Zusammenfassung. Den
    /// Body-Umfang pinnt `summary_bietet_nur_force_dummies`.
    #[test]
    fn summary_body_streamt() {
        let (url, body) = summary_body(vec!["read".into(), "bash".into()]);
        assert_eq!(url, "https://opencode.ai/zen/v1/chat/completions");
        assert_eq!(body["stream"], Value::Bool(true));
        assert_eq!(body["max_tokens"], Value::from(4000));
    }

    /// Summary-Umfang: **nur** die `force_tools` als Dummies. Kein `webfetch`,
    /// keine vollen Definitionen – und unabhängig davon, ob ein Kanal gebunden
    /// ist (der Chat-Pfad hängt davon ab, der Summary-Aufruf nicht).
    #[test]
    fn summary_bietet_nur_force_dummies() {
        let (_, body) = summary_body(vec!["read".into(), "bash".into()]);
        let tools = body["tools"].as_array().expect("tools-Array");
        let namen: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            namen,
            ["read", "bash"],
            "nur die Force-Tools, ohne webfetch"
        );
        for t in tools {
            assert_eq!(
                t["function"]["description"],
                Value::from("This tool is disabled."),
                "nur Dummies, keine vollwertigen Definitionen"
            );
        }
    }

    /// Provider ohne `force_tools` → **kein** `tools`-Feld (nicht leer, nicht
    /// `[]`): Der Summary-Aufruf bietet dann gar keine Werkzeuge an.
    #[test]
    fn summary_ohne_force_tools_ohne_tools_feld() {
        let (_, body) = summary_body(vec![]);
        assert!(
            body.get("tools").is_none(),
            "ohne force_tools darf kein tools-Feld im Body stehen: {body}"
        );
    }

    /// Test-Hilfe: Body eines Summary-Aufrufs für einen Endpoint mit den
    /// angegebenen `force_tools`.
    fn summary_body(force_tools: Vec<String>) -> (String, Value) {
        let ep = crate::config::ResolvedEndpoint {
            model: "zen/big-pickle".into(),
            api_model: "big-pickle".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: "public".into(),
            user_agent: crate::config::ZEN_USER_AGENT.into(),
            context_window: 200_000,
            force_tools,
        };
        let msgs = vec![WireMessage {
            role: "user".into(),
            content: Some("hi".into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            tokens: WireTokens::default(),
        }];
        crate::llm::api::build_body(
            &ep,
            ApiShape::ChatCompletions,
            &msgs,
            crate::llm::api::ToolOffer::ForceOnly,
            true,
            Some(4000),
        )
    }
}
