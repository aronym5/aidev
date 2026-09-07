//! Kontext-Kompaktierung: proaktiv und reaktiv.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde_json::Value;

use super::helpers::{
    dump_debug, error_chain, server_error_summary, with_debug, ERROR_SUMMARY_MAX,
};
use super::http::shared_client;
use super::wire::WireMessage;
use super::{WorkerEvent};
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

/// Ermittelt den Index, ab dem die letzten `keep_turns` Turns bei der
/// Kompaktierung unangetastet bleiben. Ein Turn wird im Wire-Format an seiner
/// `user`-Nachricht gezählt – die zugehörige Assistant-Runde samt `tool`-
/// Nachrichten gehört dazu (eine mehrteilige Tool-Runde zählt NICHT als
/// mehrere Turns, sonst wichen Wire- und Session-Grenze auseinander).
/// Identisch zur Zählung von `compact_boundary` auf der Session-Seite
/// (`Chat.order`): so verwenden Wire-Kompaktierung und der Einbau des
/// `Archive`-Events in die Session-Historie bei JEDEM Auslöser (proaktiv,
/// manuell, reaktiv) dieselbe Grenze. Die letzte, ggf. noch unbeantwortete
/// `user`-Nachricht zählt immer mit. `manual`-Nachrichten (`/run`) gibt es im
/// Wire-Format nicht (sie wandern nicht in die API) und sind daher ohnehin
/// nicht Teil der Projektion.
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
) -> Result<(String, u64), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("abgebrochen".into());
    }
    if msgs.is_empty() {
        return Err("nothing to compact".into());
    }
    // Wire-Nachrichten um System-Prompt (vorn) und abschließende Aufgabe (hinten)
    // ergänzen – die Abschluss-Message stellt die Zusammenfassung als EXPLIZITE
    // Aufgabe, statt das Modell in der assistant-Rolle „weiterreden“ zu lassen.
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

    // API-Shape des Endpunkts ermitteln; der Fallback schaltet bei einem
    // eindeutigen Format-Hinweis ODER einer geratenen Shape mit Server-Fehler
    // einmalig um (analog zum Chat-Pfad).
    let shape_info = super::api::resolve_shape_info(client, ep);
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
            let v: Value = resp
                .json()
                .map_err(|err| format!("invalid response: {err}"))?;
            let content = super::api::content_from_nonstream(shape, &v).ok_or_else(|| {
                "Zusammenfassung: kein Antworttext in der Antwort.".to_string()
            })?;
            // `completion_tokens` = Länge der Summary (die Ausgabe des
            // Kompaktierungs-Aufrufs wird später als User-Nachricht Teil des
            // Kontexts). Falls der Endpunkt kein `usage` liefert, wird als
            // Fallback die Zeichen-Schätzung verwendet (`estimate_tokens`).
            let tokens = super::api::parse_usage(&v)
                .map(|u| u.completion_tokens)
                .unwrap_or_else(|| super::estimate_tokens(&content));
            return Ok((content, tokens));
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

/// Proaktive Kompaktierung der Session-Historie: die ältesten Nachrichten
/// werden per separatem LLM-Aufruf zusammengefasst, die letzten
/// `compact_keep_turns` bleiben unangetastet. Liefert die neue
/// Nachrichtenliste und den fertigen Inhalt der Zusammenfassungs-Nachricht.
/// Manuelle Kontext-Kompaktierung (Slash-Befehl `/compact`): startet einen
/// Thread, der die ältesten Turns der Session-Historie durch eine
/// Zusammenfassung ersetzt – ohne einen LLM-Turn auszulösen. Sendet
/// `Compacting`, dann `Compacted` (die UI baut die Historie selbst um) oder bei
/// einem Fehler (z. B. nichts zu kompaktieren) `Error`.
pub fn spawn_compact(
    tx: Sender<WorkerEvent>,
    session: usize,
    config: Config,
    ep: ResolvedEndpoint,
    messages: Vec<WireMessage>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = shared_client();
        let _ = tx.send(WorkerEvent::Compacting(session));
        match compact_chat_messages(session, client, &config, &ep, &messages, &cancel) {
            Ok((_repl, content, tokens)) => {
                let _ = tx.send(WorkerEvent::Compacted(session, content, tokens));
            }
            Err(err) => {
                let _ = tx.send(WorkerEvent::Error(session, err));
            }
        }
    })
}

/// Erzeugt die Kompaktierungs-Zusammenfassung aus dem Wire-Format der Chat-
/// Projektion (`api_messages(&chat)`). Liefert die fertige Zusammenfassungs-
/// User-Nachricht (`content`) UND die umgebaute Wire-Historie (Summary-User +
/// unveränderter Tail). Das ist die EINZIGE Quelle für alle drei Auslöser:
///
/// - **proaktiv** (vor dem Turn, `spawn_worker`): `repl` wird als
///   Nachrichtenliste des kommenden Turns verwendet, `content`/`tokens`
///   gehen über `Compacted` an die UI (→ `Archive` in der Session-Historie);
/// - **manuell** (`/compact`, `spawn_compact`): `repl` wird verworfen, nur
///   `content`/`tokens` bauen das `Archive` ein;
/// - **reaktiv** (`spawn_worker` mitten im Tool-Loop): wie proaktiv – `repl`
///   für den weiteren Turns, `content`/`tokens` über `Compacted` als
///   `Archive`, damit auch hier Historie und gesendete Anfrage konsistent
///   bleiben.
pub(crate) fn compact_chat_messages(
    session: usize,
    client: &reqwest::blocking::Client,
    config: &Config,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
) -> Result<(Vec<WireMessage>, String, u64), String> {
    let boundary = wire_compact_boundary(msgs, config.compact_keep_turns);
    if boundary == 0 {
        return Err("Die Konversation hat noch keine zu kompaktierenden Turns.".into());
    }
    let old = &msgs[..boundary];
    let tail = &msgs[boundary..];
    let (summary, tokens) = request_summary(
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
        summary.trim()
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
    Ok((out, content, tokens))
}
