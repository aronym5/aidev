//! Kontext-Kompaktierung: Planung (variables `compact_keep_turns`), Entscheidung,
//! Ausführung und Protokoll (Debug-Ablage wie bei Fehlerantworten).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde_json::Value;

use super::helpers::{
    debug_dir, dump_debug, error_chain, server_error_summary, timestamp, with_debug,
    ERROR_SUMMARY_MAX,
};
use super::http::shared_client;
use super::wire::WireMessage;
use super::WorkerEvent;
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

/// Auslöser der Kompaktierung – bestimmt die Entscheidung über den Schnitt
/// (variables `compact_keep_turns`) und wird im Protokoll festgehalten.
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

impl CompactTrigger {
    /// Menschenlesbares Label für Protokoll/Status.
    pub(crate) fn label(self) -> &'static str {
        match self {
            CompactTrigger::AutoTurn => "auto (nach Turn)",
            CompactTrigger::Proactive => "proaktiv (vor Turn)",
            CompactTrigger::Reactive => "reaktiv (context_length-Fehler)",
            CompactTrigger::Manual => "manuell (/compact)",
        }
    }

    /// Kurzes, dateinamen-taugliches Kürzel für den Protokoll-Ordner.
    fn dir_tag(self) -> &'static str {
        match self {
            CompactTrigger::AutoTurn => "auto-nach-turn",
            CompactTrigger::Proactive => "proaktiv",
            CompactTrigger::Reactive => "reaktiv",
            CompactTrigger::Manual => "manuell",
        }
    }
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
pub(crate) fn wire_compact_boundary(msgs: &[WireMessage], keep_turns: usize) -> usize {
    let mut users = 0;
    let mut i = msgs.len();
    while i > 0 {
        i -= 1;
        if msgs[i].role == "user" {
            users += 1;
            if users > keep_turns.max(1) {
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
/// d. h. mindestens ein `user`-Turn wird archiviert UND mindestens ein Turn
/// überlebt. (Die Zählung von `wire_compact_boundary` braucht dafür
/// `users >= keep + 2`.)
fn possible_max_keep(msgs: &[WireMessage]) -> usize {
    let users = msgs.iter().filter(|m| m.role == "user").count();
    users.saturating_sub(2)
}

/// Ob die Wire-Projektion überhaupt einen echten Schnitt zulässt (mindestens
/// 3 `user`-Nachrichten: 1 archivierbar, 2 überlebend). Ist das nicht der
/// Fall, wird eine Kompaktierung gar nicht erst versucht – es gäbe sonst nur
/// die Meldung „keine zu kompaktierenden Turns“ (z. B. bei einem einzigen
/// laufenden Query oder direkt nach einer vorherigen Kompaktierung).
pub(crate) fn can_compact(msgs: &[WireMessage]) -> bool {
    possible_max_keep(msgs) > 0
}

/// Grobe Token-Schätzung einer einzelnen Wire-Nachricht (4 Zeichen ≈ 1 Token)
/// für die Planung/den Schnitt-Vergleich und das Protokoll.
fn wire_tokens_of(m: &WireMessage) -> u64 {
    crate::llm::estimate_tokens(m.content.as_deref().unwrap_or_default())
}

/// Geschätzte Tokens einer Wire-Nachrichtenliste.
fn wire_tokens(msgs: &[WireMessage]) -> u64 {
    msgs.iter().map(wire_tokens_of).sum()
}

/// Ein möglicher Schnitt (variables `compact_keep_turns`): wo der Schnitt
/// läge (`boundary`), wie viel wegfällt (`dropped_tokens`) und was übrig
/// bliebe (`kept_tokens`). Die Größen sind bewusst Schätzungen (Zeichen-
/// Heuristik), damit die Entscheidung ohne LLM-Aufruf möglich ist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutCandidate {
    pub keep_turns: usize,
    /// Wire-Index, ab dem der überlebende Tail beginnt (0 = kein Schnitt).
    pub boundary: usize,
    /// Anzahl der archivierten Wire-Nachrichten (alles vor `boundary`).
    pub archived_msgs: usize,
    /// Geschätzte Tokens des entfernten Teils.
    pub dropped_tokens: u64,
    /// Geschätzte Tokens des überlebenden Tails.
    pub kept_tokens: u64,
}

/// Plant alle möglichen Schnitte für `keep_turns = 1..=max_keep`. Es werden
/// nur Kandidaten mit echtem Schnitt (`boundary > 0`) geliefert.
pub(crate) fn plan_candidates(msgs: &[WireMessage], max_keep: usize) -> Vec<CutCandidate> {
    let mut out = Vec::new();
    for k in 1..=max_keep {
        let boundary = wire_compact_boundary(msgs, k);
        if boundary == 0 {
            continue;
        }
        out.push(CutCandidate {
            keep_turns: k,
            boundary,
            archived_msgs: boundary,
            dropped_tokens: msgs[..boundary].iter().map(wire_tokens_of).sum(),
            kept_tokens: msgs[boundary..].iter().map(wire_tokens_of).sum(),
        });
    }
    out
}

/// Entscheidet über die konkrete Schnittstelle (`keep_turns`) aus den
/// Kandidaten. Liefert die gewählte `keep_turns`-Zahl und die vollständige
/// Kandidaten-Tabelle (für das Protokoll).
///
/// Heuristik:
/// - **Normal (Auto/Proaktiv/Manuell):** Maximaler Erhalt – größtes `keep`,
///   dessen echter Folge-Kontext (überlebender Tail + konservativ veranschlagtes
///   Summary-Budget `compact_summary_tokens`) wieder UNTER der Auslastungs-
///   Schwelle (`context_window × compact_at`) liegt. So wird der Nutzer-Kontext
///   so wenig wie nötig beschnitten und die Kompaktierung löst sich nach dem
///   Einbau nicht sofort erneut aus.
/// - **Reaktiv (context_length-Fehler):** Stärkster Schnitt – kleinstes
///   `keep`, dessen Folge-Kontext sicher unter das VOLLE Fenster
///   (`context_window`) passt, damit der Retry nicht erneut überläuft.
///
/// Erfüllt kein Kandidat das Ziel konservativ, fällt die Entscheidung auf das
/// konfigurierte `compact_keep_turns` zurück (falls das einen echten Schnitt
/// liefert), sonst auf den stärksten Schnitt.
pub(crate) fn decide_keep(
    msgs: &[WireMessage],
    config: &Config,
    trigger: CompactTrigger,
    context_window: u64,
) -> (usize, Vec<CutCandidate>) {
    let max_keep = possible_max_keep(msgs);
    if max_keep == 0 {
        // Nichts zu schneiden – es bleibt beim konfigurierten Wunsch (der
        // wird von `compact_chat_messages` als „keine zu kompaktierenden
        // Turns“ abgefangen).
        return (config.compact_keep_turns, Vec::new());
    }
    let candidates = plan_candidates(msgs, max_keep);
    let budget = config.compact_summary_tokens;
    let feasible = |kept: u64, window: u64| kept.saturating_add(budget) <= window;
    let preferred = match trigger {
        CompactTrigger::Reactive => candidates
            .iter()
            .filter(|c| feasible(c.kept_tokens, context_window))
            .min_by_key(|c| c.keep_turns)
            .map(|c| c.keep_turns),
        _ => {
            let threshold = (context_window as f64 * config.compact_at) as u64;
            candidates
                .iter()
                .filter(|c| feasible(c.kept_tokens, threshold))
                .max_by_key(|c| c.keep_turns)
                .map(|c| c.keep_turns)
        }
    };
    let chosen = preferred.unwrap_or_else(|| {
        if candidates.iter().any(|c| c.keep_turns == config.compact_keep_turns) {
            config.compact_keep_turns
        } else {
            candidates
                .iter()
                .map(|c| c.keep_turns)
                .min()
                .unwrap_or(config.compact_keep_turns)
        }
    });
    (chosen, candidates)
}

/// Ergebnis-Objekt des Zusammenfassungs-Aufrufs: Inhalt, Tokenlänge und die
/// HTTP-Daten fürs Protokoll (Request-Body + Roh-Antwort).
pub(crate) struct SummaryResult {
    pub content: String,
    pub tokens: u64,
    pub url: String,
    pub request: Value,
    pub response: String,
}

/// Ein separater, NICHT-streamender Zusammenfassungs-Aufruf – in der
/// API-Shape des Endpunkts (Chat Completions `/chat/completions` oder
/// Responses `/responses`): Werkzeuge sind deaktiviert, `max_tokens`/
/// `max_output_tokens` begrenzt die Länge der Zusammenfassung (Obergrenze,
/// nicht Zielgröße).
pub(crate) fn request_summary(
    session: usize,
    client: &reqwest::blocking::Client,
    ep: &ResolvedEndpoint,
    compact_summary_tokens: u64,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
) -> Result<SummaryResult, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("abgebrochen".into());
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
    });
    compact_msgs.extend_from_slice(msgs);
    compact_msgs.push(WireMessage {
        role: "user".into(),
        content: Some(COMPACT_REQUEST.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    });

    // API-Shape aus dem Cache ermitteln (Default Chat Completions); der
    // Fallback schaltet bei einem eindeutigen Format-Hinweis ODER einer
    // geratenen Shape mit Server-Fehler einmalig um (analog zum Chat-Pfad).
    let shape_info = super::api::resolve_shape_info(ep);
    let mut shape = shape_info.shape;
    let mut attempts = 0;
    loop {
        let (url, body) = super::api::build_body(
            ep,
            shape,
            &compact_msgs,
            false,
            crate::perm::Permission::default(),
            false,
            false,
            Some(compact_summary_tokens),
        );
        let mut req = client
            .post(&url)
            .bearer_auth(&ep.api_key)
            .header("user-agent", &ep.user_agent);
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
            Err(err) => return Err(format!("Netzwerkfehler: {}", error_chain(&err))),
        };
        if resp.status().is_success() {
            // Diese Shape hat funktioniert → als „bestimmt“ merken (geteilter
            // Cache mit dem Chat-Pfad), damit nachfolgende Aufrufe direkt
            // dieses Format nutzen.
            super::api::remember_shape(ep, shape);
            let raw = resp
                .text()
                .map_err(|err| format!("invalid response: {err}"))?;
            let v: Value =
                serde_json::from_str(&raw).map_err(|err| format!("invalid response: {err}"))?;
            let content = super::api::content_from_nonstream(shape, &v)
                .ok_or_else(|| "Zusammenfassung: kein Antworttext in der Antwort.".to_string())?;
            // `completion_tokens` = Länge der Summary (die Ausgabe des
            // Kompaktierungs-Aufrufs wird später als User-Nachricht Teil des
            // Kontexts). Falls der Endpunkt kein `usage` liefert, wird als
            // Fallback die Zeichen-Schätzung verwendet (`estimate_tokens`).
            let tokens = super::api::parse_usage(&v)
                .map(|u| u.completion_tokens)
                .unwrap_or_else(|| super::estimate_tokens(&content));
            return Ok(SummaryResult {
                content,
                tokens,
                url,
                request: body,
                response: raw,
            });
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
            &format!("zusammenfassung-{status}"),
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

/// Kurzübersicht der für die Kompaktierung relevanten Historie (eine Zeile je
/// Wire-Nachricht: Index, Rolle, geschätzte Tokens, gekürzter Inhalt) – für
/// das Protokoll.
fn message_overview(msgs: &[WireMessage]) -> String {
    let mut out = String::new();
    for (i, m) in msgs.iter().enumerate() {
        let t = wire_tokens_of(m);
        let raw = m.content.as_deref().unwrap_or_default();
        let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let shown: String = one_line.chars().take(120).collect();
        let truncated = shown.chars().count() < one_line.chars().count();
        let _ = writeln!(
            out,
            "[{:3}] {:<10} {:>6} T  {}{}",
            i,
            m.role,
            t,
            shown,
            if truncated { "…" } else { "" }
        );
    }
    out
}

/// Sammelt alle Informationen eines Kompaktierungs-Vorgangs für das Protokoll.
pub(crate) struct CompactionLog {
    pub trigger: CompactTrigger,
    pub model: String,
    pub base_url: String,
    /// Konkrete Request-URL des Zusammenfassungs-Aufrufs.
    pub url: String,
    pub context_window: u64,
    pub compact_at: f64,
    /// `compact_summary_tokens` – konservativer Ansatz für die Summary-Länge.
    pub summary_budget: u64,
    pub threshold: u64,
    pub current_tokens: u64,
    pub decided_keep: usize,
    pub candidates: Vec<CutCandidate>,
    pub chosen: CutCandidate,
    pub archived_msgs: usize,
    pub summary: String,
    pub summary_tokens: u64,
    pub overview: String,
    pub request: Value,
    pub response: String,
}

/// Baut den vollständigen Inhalt des Kompaktierungs-Protokolls (Dateiname →
/// Inhalt) – rein, ohne Datei-I/O, damit testbar:
///
/// - `meta.txt`      – Auslöser, Randbedingungen (Kontextgröße, Konfiguration)
///   und getroffene Entscheidung samt erreichten Summary-Tokens.
/// - `overview.txt`  – Kurzübersicht der relevanten Historie.
/// - `plan.txt`      – Kandidaten-Tabelle (variables `compact_keep_turns`).
/// - `summary.txt`   – die erreichte Zusammenfassung mit ihrer Tokenlänge.
/// - `request.json` / `response.txt` – der Zusammenfassungs-Aufruf.
pub(crate) fn render_compaction_log(log: &CompactionLog) -> Vec<(String, String)> {
    let mut files = Vec::new();

    // ── meta.txt: Auslöser + Randbedingungen + Entscheidung ────────────────
    let mut meta = String::new();
    let _ = writeln!(meta, "time:            {}", timestamp());
    let _ = writeln!(meta, "trigger:         {}", log.trigger.label());
    let _ = writeln!(meta, "model:           {}", log.model);
    let _ = writeln!(meta, "base_url:        {}", log.base_url);
    let _ = writeln!(meta, "url:             {}", log.url);
    let _ = writeln!(meta, "context_window:  {}", log.context_window);
    let _ = writeln!(meta, "compact_at:      {}", log.compact_at);
    let _ = writeln!(meta, "threshold:       {} (context_window × compact_at)", log.threshold);
    let _ = writeln!(meta, "current_tokens:  {} (aktueller Kontext)", log.current_tokens);
    let _ = writeln!(meta, "decided_keep:    {} (Schnitt an Wire-Index {})", log.decided_keep, log.chosen.boundary);
    let _ = writeln!(meta, "archived_msgs:   {}", log.archived_msgs);
    let _ = writeln!(meta, "dropped_tokens:  {} (geschätzt, wegfällender Kontext)", log.chosen.dropped_tokens);
    let _ = writeln!(meta, "kept_tokens:     {} (geschätzt, bleibender Kontext)", log.chosen.kept_tokens);
    let _ = writeln!(meta, "summary_tokens:  {} (erreichte Zusammenfassung)", log.summary_tokens);
    files.push(("meta.txt".into(), meta));

    // ── overview.txt: Kurzübersicht der relevanten Historie ────────────────
    files.push((
        "overview.txt".into(),
        format!("Relevante Historie (Wire-Projektion):\n\n{}", log.overview),
    ));

    // ── plan.txt: Kandidaten (variables compact_keep_turns) ────────────────
    let mut plan = String::new();
    let _ = writeln!(
        plan,
        "Kandidaten (variables compact_keep_turns) – Ziel konservativ veranschlagt \
         inkl. Summary-Budget {}:",
        log.summary_budget
    );
    for c in &log.candidates {
        let marker = if c.keep_turns == log.decided_keep {
            "  <-- gewählt"
        } else {
            ""
        };
        let _ = writeln!(
            plan,
            "  keep={:<3} Schnitt@Wire-{:<5} archiviert: {:<4} Msg | ~{} T weg | ~{} T bleiben | unter Ziel: {}{}",
            c.keep_turns,
            c.boundary,
            c.archived_msgs,
            c.dropped_tokens,
            c.kept_tokens,
            match log.trigger {
                CompactTrigger::Reactive => {
                    c.kept_tokens.saturating_add(log.summary_budget) <= log.context_window
                }
                _ => c.kept_tokens.saturating_add(log.summary_budget) <= log.threshold,
            },
            marker
        );
    }
    files.push(("plan.txt".into(), plan));

    // ── summary.txt: erreichte Zusammenfassung + Tokenlänge ────────────────
    let summary = format!(
        "summary_tokens: {}\narchived_msgs:  {}\n\n{}\n",
        log.summary_tokens, log.archived_msgs, log.summary
    );
    files.push(("summary.txt".into(), summary));

    // ── request.json / response.txt ────────────────────────────────────────
    files.push((
        "request.json".into(),
        serde_json::to_string_pretty(&log.request).unwrap_or_default(),
    ));
    files.push(("response.txt".into(), log.response.clone()));

    files
}

/// Schreibt das Kompaktierungs-Protokoll nach
/// `$XDG_DATA_HOME/aidev/debug/<zeitstempel>_kompaktierung-<auslöser>/`
/// (bzw. `~/.local/share/aidev/debug/…`) – analog zu den Fehler-Dumps (siehe
/// `render_compaction_log`). Liefert den Ordnerpfad, sonst `None`. In Tests
/// wird nichts geschrieben.
pub(crate) fn write_compaction_log(log: &CompactionLog) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let base = debug_dir()?;
    let dir = base.join(format!(
        "{}_kompaktierung-{}",
        timestamp(),
        log.trigger.dir_tag()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    for (name, content) in render_compaction_log(log) {
        std::fs::write(dir.join(name), content).ok()?;
    }
    Some(dir.display().to_string())
}

/// Ergebnis einer ausgeführten Kompaktierung:
/// `(neue Wire-Historie, fertige Summary, Summary-Tokens, verwendetes
/// `keep_turns`, Protokollpfad)`.
pub(crate) type CompactionResult = (Vec<WireMessage>, String, u64, usize, Option<String>);

/// Führt die Kompaktierung für EINEN Auslöser aus (Plan → Entscheidung →
/// Zusammenfassung → Protokoll). Liefert die neue Wire-Historie (`repl`:
/// Summary-User + unveränderter Tail), die fertige Summary (`content`), ihre
/// Token-Zahl, das verwendete `keep_turns` und – falls geschrieben – den
/// Protokollordner.
///
/// Alle vier Auslöser laufen über diese EINE Quelle:
///
/// - **auto nach Turn** (`spawn_compact`): `repl` wird verworfen, nur
///   `content`/`tokens`/`keep` bauen das `Archive` ein;
/// - **manuell** (`/compact`, `spawn_compact`): wie auto – `repl` verworfen;
/// - **proaktiv** (`spawn_worker` vor dem Turn): `repl` wird als
///   Nachrichtenliste des kommenden Turns verwendet;
/// - **reaktiv** (`spawn_worker` mitten im Tool-Loop): wie proaktiv.
///
/// `current_tokens` ist der (genauere) gemessene/gespeicherte Kontextstand aus
/// der Session; `None` → Schätzung über die Wire-Nachrichten.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compact_chat_messages(
    session: usize,
    client: &reqwest::blocking::Client,
    config: &Config,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
    trigger: CompactTrigger,
    current_tokens: Option<u64>,
) -> Result<CompactionResult, String> {
    let (keep, candidates) = decide_keep(msgs, config, trigger, ep.context_window);
    let boundary = wire_compact_boundary(msgs, keep);
    if boundary == 0 {
        return Err("Die Konversation hat noch keine zu kompaktierenden Turns.".into());
    }
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
        "[Compressed history - {} earlier messages]\n\n{}",
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
    });
    out.extend_from_slice(tail);

    // Protokoll (Debug-Ablage wie bei Fehlerantworten): Randbedingungen,
    // Kandidaten, Entscheidung, erreichte Summary + Tokenlänge.
    let chosen = candidates
        .iter()
        .find(|c| c.keep_turns == keep)
        .copied()
        .unwrap_or(CutCandidate {
            keep_turns: keep,
            boundary,
            archived_msgs: old.len(),
            dropped_tokens: wire_tokens(old),
            kept_tokens: wire_tokens(tail),
        });
    let log = CompactionLog {
        trigger,
        model: ep.model.clone(),
        base_url: ep.base_url.clone(),
        url: res.url,
        context_window: ep.context_window,
        compact_at: config.compact_at,
        summary_budget: config.compact_summary_tokens,
        threshold: (ep.context_window as f64 * config.compact_at) as u64,
        current_tokens: current_tokens.unwrap_or_else(|| wire_tokens(msgs)),
        decided_keep: keep,
        candidates,
        chosen,
        archived_msgs: old.len(),
        summary: res.content,
        summary_tokens: res.tokens,
        overview: message_overview(msgs),
        request: res.request,
        response: res.response,
    };
    let log_path = write_compaction_log(&log);

    Ok((out, content, res.tokens, keep, log_path))
}

/// Startet die Kontext-Kompaktierung in einem eigenen Thread – für die
/// manuelle (`/compact`) und die automatische (direkt nach der finalen
/// Antwort, parallel zum Tippen) Kompaktierung. Sendet `Compacting`, dann
/// `Compacted` (mit Summary, Tokenzahl, gewähltem `keep_turns` und
/// Protokollpfad) oder bei einem Fehler (z. B. nichts zu kompaktieren)
/// `Error`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_compact(
    tx: Sender<WorkerEvent>,
    session: usize,
    config: Config,
    ep: ResolvedEndpoint,
    messages: Vec<WireMessage>,
    cancel: Arc<AtomicBool>,
    trigger: CompactTrigger,
    current_tokens: Option<u64>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = shared_client();
        let _ = tx.send(WorkerEvent::Compacting(session));
        match compact_chat_messages(
            session,
            client,
            &config,
            &ep,
            &messages,
            &cancel,
            trigger,
            current_tokens,
        ) {
            Ok((_repl, content, tokens, keep, log_path)) => {
                let _ = tx.send(WorkerEvent::Compacted(session, content, tokens, keep, log_path));
            }
            Err(err) => {
                let _ = tx.send(WorkerEvent::Error(session, err));
            }
        }
    })
}