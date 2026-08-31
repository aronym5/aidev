//! App-Ebene: Session-Fluss, Worker-Event-Drain, Kompaktierung und Key-Handling.
//!
//! Nach der Migration auf das Event-Log (`chat`) prüfen diese Tests die
//! inkrementelle Event-Erzeugung über Worker-Events, die `api_messages`-
//! Projektion und die Kompaktierungs-Grenzen. Dialog-/Picker-Tests (die sich
//! gegen die Kanal-/Modell-UI richten) sind bewusst auf ein Minimum reduziert,
//! da sich die UI noch ändert.

use super::commands::parse_run_line;
use super::session::compact_boundary;
use super::*;
use crate::channel::ChannelRegistry;
use crate::chat::EventKind;
use crate::config::{ProviderConfig, SymbolMode};
use crate::llm;
use crate::perm::Permission;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

fn test_provider(base_url: &str) -> HashMap<String, ProviderConfig> {
    let mut m = HashMap::new();
    m.insert(
        "test".to_string(),
        ProviderConfig {
            base_url: base_url.to_string(),
            api_key: Some("x".into()),
            user_agent: None,
        },
    );
    m
}

fn base_config() -> Config {
    Config {
        model: "test/m".into(),
        provider: test_provider("http://127.0.0.1:1"),
        max_tool_rounds: 16,
        default_channel: None,
        channels: std::collections::HashMap::new(),
        symbols: SymbolMode::Glyph,
        context_window: 200_000,
        compact_at: 0.8,
        compact_keep_turns: 3,
        compact_summary_tokens: 4_000,
        compact_auto: true,
        mouse: false,
        models: indexmap::IndexMap::new(),
        paths: Default::default(),
        ..Config::default()
    }
}

fn app() -> App {
    let (tx, rx) = mpsc::channel();
    App::new(base_config(), ChannelRegistry::new(&base_config()), tx, rx)
}

fn alt_d() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT)
}

fn alt_h() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn tool_activity() -> llm::ToolActivity {
    llm::ToolActivity::default()
}

// ── Session-Event-Aufbau über App::drain_events ───────────────────────────

#[test]
fn send_prompt_legt_user_event_an_und_setzt_phase() {
    let mut a = app();
    a.sessions[0].editor.set_text("hallo");
    a.send_prompt(0);

    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::WaitingForLLM);
    let ids: Vec<_> = s.chat.order().to_vec();
    assert_eq!(ids.len(), 1, "genau ein UserPrompt-Event");
    match &s.chat.event(ids[0]).unwrap().kind {
        EventKind::UserPrompt { text, .. } => assert_eq!(text, "hallo"),
        other => panic!("erwartet UserPrompt, bin {other:?}"),
    }
}

#[test]
fn worker_chunk_und_done_bauen_assistant_event() {
    let mut a = app();
    {
        let s = &mut a.sessions[0];
        s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    }

    a.tx.send(llm::WorkerEvent::Chunk(0, "Hel".into())).unwrap();
    a.tx.send(llm::WorkerEvent::Chunk(0, "lo".into())).unwrap();
    a.tx.send(llm::WorkerEvent::Done(0)).unwrap();
    assert!(a.drain_events(), "Events müssen verarbeitet werden");

    let assistant = a.sessions[0]
        .chat
        .iter()
        .find(|e| matches!(e.kind, EventKind::Assistant { .. }))
        .expect("Assistant-Event erzeugt");
    match &assistant.kind {
        EventKind::Assistant { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("erwartet Assistant: {other:?}"),
    }
    assert_eq!(a.sessions[0].phase, Phase::Idle);
}

#[test]
fn tool_round_wird_als_assistant_plus_tool_events_abgebildet() {
    let mut a = app();
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());

    a.tx
        .send(llm::WorkerEvent::ToolStart {
            session: 0,
            tool_call_id: "call_1".into(),
            function_name: "read".into(),
            arguments: "{\"path\":\"x\"}".into(),
            label: "read x".into(),
        })
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolOutput(0, "inhalt".into())).unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity())).unwrap();
    a.tx.send(llm::WorkerEvent::Done(0)).unwrap();
    assert!(a.drain_events());

    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::Idle);
    // Assistant-Runde mit einem Tool-Kind.
    let assistant = s
        .chat
        .iter()
        .find(|e| matches!(e.kind, EventKind::Assistant { .. }))
        .expect("Assistant vorhanden");
    let tool_ids = match &assistant.kind {
        EventKind::Assistant { tool_event_ids, .. } => tool_event_ids,
        _ => unreachable!(),
    };
    assert_eq!(tool_ids.len(), 1, "ein Tool-Event registriert");

    // API-Projektion: assistant mit tool_calls + tool-Antwort.
    let wire = crate::chat::api_messages(&s.chat);
    assert_eq!(wire.len(), 3, "user + assistant(tool) + tool");
    assert!(wire[1].tool_calls.is_some(), "assistant trägt tool_calls");
    assert_eq!(wire[2].role, "tool");
    assert_eq!(wire[2].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn tool_zwischenrunde_mit_usage_ableitet_text_tokens() {
    use crate::llm::Usage;
    let mut a = app();
    a.sessions[0]
        .push_user_message("frage".into(), Some(Permission::Read), "m".into());

    // Zwischenrunde (assistant mit tool_calls): Text, dann DAS USAGE DIESER
    // RUNDE, dann Tool-Aufruf – Reihenfolge wie im Worker nach §4.3-Fix.
    a.tx.send(llm::WorkerEvent::Chunk(0, "Ich schaue nach.".into()))
        .unwrap();
    a.tx
        .send(llm::WorkerEvent::Usage(
            0,
            Usage {
                prompt_tokens: 500,
                completion_tokens: 40,
                total_tokens: 540,
                cached_tokens: None,
            },
            llm::CompletionParts::default(),
        ))
        .unwrap();
    a.tx
        .send(llm::WorkerEvent::ToolStart {
            session: 0,
            tool_call_id: "call_1".into(),
            function_name: "read".into(),
            arguments: "{\"path\":\"x\"}".into(),
            label: "read x".into(),
        })
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity())).unwrap();
    // Finale Runde (ohne tool_calls), danach Done.
    a.tx.send(llm::WorkerEvent::Chunk(0, "Antwort.".into())).unwrap();
    a.tx
        .send(llm::WorkerEvent::Usage(
            0,
            Usage {
                prompt_tokens: 600,
                completion_tokens: 30,
                total_tokens: 630,
                cached_tokens: None,
            },
            llm::CompletionParts::default(),
        ))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Done(0)).unwrap();
    assert!(a.drain_events());

    let s = &a.sessions[0];
    let assistants: Vec<_> = s
        .chat
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Assistant { .. }))
        .collect();
    assert_eq!(assistants.len(), 2, "Zwischenrunde + finale Runde");
    let mid = match &assistants[0].kind {
        EventKind::Assistant {
            num_tokens_text,
            tool_event_ids,
            ..
        } => {
            assert_eq!(tool_event_ids.len(), 1, "Tool hängt an der Zwischenrunde");
            *num_tokens_text
        }
        _ => unreachable!(),
    };
    let fin = match &assistants[1].kind {
        EventKind::Assistant { num_tokens_text, .. } => *num_tokens_text,
        _ => unreachable!(),
    };
    // Beide leiten ihre Text-Tokens aus dem Usage IHRER Runde ab (kein ~0).
    assert!(mid > 0, "Zwischenantwort liefert abgeleitete Tokens, war {mid}");
    assert!(fin > 0, "finale Antwort liefert abgeleitete Tokens, war {fin}");
}

#[test]
fn end_to_end_gemessene_parts_ueberleben_bis_zum_tool_event() {
    use crate::llm::Usage;
    // Abbild des `response.txt`-Mitschnitts: Reasoning + 2 Tool-Calls in einer
    // Runde, danach finale Antwort. Die beim Streaming gemessenen Parts müssen
    // bis in die Tool-Events durchkommen (und den zweiten derive-Lauf der
    // finalen Runde überleben).
    let mut a = app();
    a.sessions[0]
        .push_user_message("frage".into(), Some(Permission::Read), "m".into());

    a.tx.send(llm::WorkerEvent::Reasoning(0, "Gedanken.".into())).unwrap();
    a.tx
        .send(llm::WorkerEvent::Usage(
            0,
            Usage {
                prompt_tokens: 987,
                completion_tokens: 111,
                total_tokens: 1098,
                cached_tokens: None,
            },
            llm::CompletionParts {
                reasoning: 38,
                content: 0,
                tool_calls: vec![36, 37],
            },
        ))
        .unwrap();
    // Tool 1 (call_8f…) und Tool 2 (call_042…) – der Worker sendet sie
    // VERSCHACHTELT: Start→End→Start→End (jedes Tool ausgeführt, dann End).
    a.tx
        .send(llm::WorkerEvent::ToolStart {
            session: 0,
            tool_call_id: "call_8f1dd6088e2d4f46a434135b".into(),
            function_name: "glob".into(),
            arguments: "{\"pattern\": \"*\"}".into(),
            label: "glob *".into(),
        })
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity())).unwrap();
    a.tx
        .send(llm::WorkerEvent::ToolStart {
            session: 0,
            tool_call_id: "call_04276ce441d9441786fe240d".into(),
            function_name: "glob".into(),
            arguments: "{\"pattern\": \"**/*\"}".into(),
            label: "glob **/*".into(),
        })
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity())).unwrap();
    // Finale Antwort + Done (dritter derive-Lauf über den ganzen Turn).
    a.tx.send(llm::WorkerEvent::Chunk(0, "Zusammenfassung.".into())).unwrap();
    a.tx.send(llm::WorkerEvent::Done(0)).unwrap();
    assert!(a.drain_events());

    let s = &a.sessions[0];
    // Tool-Events nach ID einsammeln und ihre num_tokens_input prüfen.
    let mut by_call: HashMap<String, u64> = HashMap::new();
    for ev in s.chat.iter() {
        if let EventKind::Tool {
            tool_call_id,
            num_tokens_input,
            ..
        } = &ev.kind
        {
            let _ = by_call.insert(tool_call_id.clone(), *num_tokens_input);
        }
    }
    assert_eq!(
        by_call.get("call_8f1dd6088e2d4f46a434135b"),
        Some(&36),
        "Tool 1 behält gemessene 36"
    );
    assert_eq!(
        by_call.get("call_04276ce441d9441786fe240d"),
        Some(&37),
        "Tool 2 behält gemessene 37"
    );
    // Reasoning-Anteil der Tool-Runde bleibt ebenfalls exakt.
    let assistant = s
        .chat
        .iter()
        .find(|e| matches!(e.kind, EventKind::Assistant { .. }))
        .expect("Tool-Runde");
    assert!(matches!(
        &assistant.kind,
        EventKind::Assistant { num_tokens_reasoning: 38, .. }
    ));
}

#[test]
fn abbruch_erzeugt_abort_event_und_phase_idle() {
    let mut a = app();
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());
    a.tx.send(llm::WorkerEvent::Chunk(0, "halb".into())).unwrap();
    a.tx.send(llm::WorkerEvent::Cancelled(0)).unwrap();
    assert!(a.drain_events());

    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::Idle);
    assert!(s.aborted, "Abbruch markiert");
    assert!(
        s.chat
            .iter()
            .any(|e| matches!(e.kind, EventKind::Abort)),
        "Abort-Event vorhanden"
    );
}

// ── Kompaktierung ─────────────────────────────────────────────────────────

#[test]
fn compact_boundary_behaelt_letzte_turns() {
    use crate::llm::Usage;
    let mut a = app();
    {
        let s = &mut a.sessions[0];
        for i in 0..5 {
            s.push_user_message(format!("frage {i}"), Some(Permission::Read), "m".into());
            let aid = s.open_assistant(String::new(), format!("antwort {i}"));
            s.chat.finalize_assistant(
                aid,
                std::time::Instant::now(),
                Usage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cached_tokens: None,
                },
                0,
                0,
                false,
            );
        }
    }
    let b = compact_boundary(&a.sessions[0], 3);
    assert!(b > 0, "mindestens ein Turn ist kompaktierbar");
}

// ── /run und Key-Handling ─────────────────────────────────────────────────

#[test]
fn parse_run_line_extrahiert_ausdruck() {
    assert_eq!(parse_run_line("/run cargo build && git status").as_deref(), Some("cargo build && git status"));
    assert_eq!(parse_run_line("/run"), Some(String::new()));
    assert_eq!(parse_run_line("/runfoo"), None);
    assert_eq!(parse_run_line("normal"), None);
}

#[test]
fn alt_d_dupliziert_kanal_der_session() {
    // Ohne gebundenen Kanal zeigt Alt+D einen Fehler statt zu crashen.
    let mut a = app();
    a.handle_key(alt_d());
    assert_eq!(a.sessions.len(), 1, "Session-Anzahl bleibt");
}

#[test]
fn ctrl_c_leert_editor_statt_zu_beenden() {
    let mut a = app();
    a.sessions[0].editor.set_text("halb geschrieben");
    a.handle_key(ctrl_c());
    assert!(!a.quit, "nicht beenden bei nicht-leerem Editor");
    assert!(a.sessions[0].editor.is_empty(), "Editor geleert");
}

#[test]
fn http_headers_event_setzt_dialog_daten_der_session() {
    // Worker-Event `HttpHeaders` hinterlegt die Response-Header der letzten
    // LLM-Antwort in der Session (Datenquelle für den Alt+H-Dialog).
    let mut a = app();
    a.tx
        .send(llm::WorkerEvent::HttpHeaders(
            0,
            vec![
                ("content-type".into(), "text/event-stream".into()),
                ("date".into(), "Tue, 01 Jan 2025 00:00:00 GMT".into()),
            ],
        ))
        .unwrap();
    assert!(a.drain_events());
    let h = a.sessions[0].last_http_headers.as_ref().expect("Header gespeichert");
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].0, "content-type");
    assert_eq!(h[0].1, "text/event-stream");
}

#[test]
fn alt_h_oeffnet_http_header_dialog_und_esc_schliesst_ihn() {
    let mut a = app();
    // Kein offener Dialog zu Beginn.
    assert!(!a.http_headers_dialog);
    // Alt+H öffnet den Dialog.
    a.handle_key(alt_h());
    assert!(a.http_headers_dialog, "Alt+H öffnet den Header-Dialog");
    // Esc schließt ihn wieder; ein anderer Key schließt ihn nicht.
    a.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(a.http_headers_dialog, "andere Tasten behalten den Dialog offen");
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!a.http_headers_dialog, "Esc schließt den Header-Dialog");
}
