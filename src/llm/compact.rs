//! Kontext-Kompaktierung: proaktiv und reaktiv.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};

use super::helpers::{
    dump_debug, error_chain, server_error_summary, with_debug, ERROR_SUMMARY_MAX,
};
use super::http::shared_client;
use super::wire::{ensure_reasoning_for_tool_calls, WireMessage};
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

/// Ermittelt den Index, ab dem die letzten `keep_turns` Assistant-Turns
/// (einschließlich der sie einleitenden User-Nachrichten) bei der
/// Kompaktierung unangetastet bleiben. `manual`-Nachrichten (`/run`) zählen
/// nicht als Turns – sie sind reine Anzeige und wandern nicht in die API.
/// Wie `compact_boundary`, aber für das Wire-Format (`WireMessage`): hier
/// zählen Werkzeug-Runden (`tool`-Nachrichten) zum Turn und `manual` gibt es
/// nicht mehr. Dient der reaktiven Kompaktierung mitten im Tool-Loop.
pub(crate) fn wire_compact_boundary(msgs: &[WireMessage], keep_turns: usize) -> usize {
    let mut assists = 0;
    let mut i = msgs.len();
    while i > 0 {
        i -= 1;
        if msgs[i].role == "assistant" {
            assists += 1;
            if assists >= keep_turns.max(1) {
                while i > 0 && msgs[i - 1].role != "user" {
                    i -= 1;
                }
                return i.saturating_sub(1);
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

/// Ein separater, NICHT-streamender Zusammenfassungs-Aufruf an
/// `/chat/completions`: Werkzeuge sind deaktiviert, `max_tokens` begrenzt die
/// Länge der Zusammenfassung (Obergrenze, nicht Zielgröße).
pub(crate) fn request_summary(
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
    let mut body_msgs: Vec<Value> = Vec::with_capacity(msgs.len() + 2);
    body_msgs.push(json!({"role":"system","content": COMPACT_SYSTEM_PROMPT}));
    for m in ensure_reasoning_for_tool_calls(msgs) {
        body_msgs.push(serde_json::to_value(m).map_err(|e| e.to_string())?);
    }
    // Abschließende, explizite Aufgabe als User-Nachricht – so wird die
    // Zusammenfassung klar aufgetragen, statt dass das Modell das Gespräch in
    // der assistant-Rolle „weiterführt“ (→ Verbatim-Kopien/abbrechende Antwort).
    body_msgs.push(json!({"role":"user","content": COMPACT_REQUEST}));
    let body = json!({
        "model": ep.api_model,
        "stream": false,
        "max_tokens": compact_summary_tokens,
        "messages": body_msgs,
    });

    let url = format!("{}/chat/completions", ep.base_url);
    let resp = match client
        .post(&url)
        .bearer_auth(&ep.api_key)
        .header("user-agent", &ep.user_agent)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(err) => return Err(format!("Netzwerkfehler: {}", error_chain(&err))),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let raw = resp.text().unwrap_or_default();
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
    let v: Value = resp
        .json()
        .map_err(|err| format!("invalid response: {err}"))?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            "Zusammenfassung: „choices[0].message.content“ fehlt in der Antwort.".to_string()
        })?;
    // `completion_tokens` = Länge der Summary (die Ausgabe des Kompaktierungs-
    // Aufrufs wird später als User-Nachricht Teil des Kontexts). Falls der
    // Endpunkt kein `usage` liefert, wird als Fallback die Zeichen-Schätzung
    // verwendet (`estimate_tokens`), analog zu anderen Nachrichten ohne Usage.
    let tokens = super::http::parse_usage(&v)
        .map(|u| u.completion_tokens)
        .unwrap_or_else(|| super::estimate_tokens(&content));
    Ok((content, tokens))
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
        match compact_chat_messages(client, &config, &ep, &messages, &cancel) {
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
/// unveränderter Tail), damit `spawn_worker` die proaktive Kompaktierung direkt
/// einsetzen kann.
pub(crate) fn compact_chat_messages(
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
    let (summary, tokens) =
        request_summary(client, ep, config.compact_summary_tokens, old, cancel)?;
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

/// Reaktive Kompaktierung der Wire-Historie mitten im Tool-Loop (bei
/// `context_length`-Fehlern): die ältesten Runden werden zusammengefasst,
/// die letzten Turns bleiben unangetastet. Die Session-Historie wird dabei
/// NICHT umgeschrieben – die Werkzeug-Runden sind ohnehin flüchtig.
pub(crate) fn compact_wire_messages(
    client: &reqwest::blocking::Client,
    config: &Config,
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    cancel: &AtomicBool,
) -> Result<Vec<WireMessage>, String> {
    // Identische Logik wie die proaktive Kompaktierung; nur `content` wird
    // hier nicht benötigt (die Session-Historie bleibt unangetastet).
    compact_chat_messages(client, config, ep, msgs, cancel).map(|(repl, _, _)| repl)
}
