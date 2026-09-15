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
use crate::config::ProviderConfig;
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
    App::new(base_config(), ChannelRegistry::new_with_warnings(&base_config()).0, tx, rx)
}

fn alt_d() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT)
}

fn alt_h() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT)
}

fn ctrl_o() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
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

// ── Enter während aktivem Stream / Tool-Einsatz ─────────────────────────────

#[test]
fn enter_waehrend_stream_model_befehl_wird_ausgefuehrt() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/fast", None)]);
    a.sessions[0].editor.set_text("erste nachricht");
    a.send_prompt(0); // phase → WaitingForLLM
                      // Während des Streams `/model` eingeben und mit Enter bestätigen.
    a.sessions[0].editor.set_text("/model test/fast");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        a.sessions[0].phase,
        Phase::WaitingForLLM,
        "Stream läuft weiter"
    );
    assert_eq!(
        a.sessions[0].model_alias.as_deref(),
        Some("test/fast"),
        "/model wird trotz aktivem Stream ausgeführt"
    );
    assert!(
        a.sessions[0].editor.text_string().is_empty(),
        "Editor nach /model geleert"
    );
}

#[test]
fn enter_waehrend_stream_theme_befehl_wird_ausgefuehrt() {
    let mut a = app();
    a.sessions[0].editor.set_text("erste nachricht");
    a.send_prompt(0); // phase → WaitingForLLM
    a.sessions[0].editor.set_text("/theme dark");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(a.sessions[0].phase, Phase::WaitingForLLM);
    assert!(
        a.sessions[0]
            .error
            .as_deref()
            .unwrap_or("")
            .contains("theme → dark"),
        "/theme läuft trotz aktivem Stream"
    );
}

#[test]
fn enter_waehrend_stream_dialog_befehle_oeffnen_dialoge() {
    let mut a = app();
    a.sessions[0].editor.set_text("erste nachricht");
    a.send_prompt(0); // phase → WaitingForLLM
                      // `/options` öffnet trotz aktivem Stream den Options-Dialog.
    a.sessions[0].editor.set_text("/options");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        a.options_dialog.is_some(),
        "/options öffnet Dialog trotz Stream"
    );
    // Dialog schließen, dann `/header` – ebenfalls ein Dialog-Befehl.
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    a.sessions[0].editor.set_text("/header");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.http_headers_dialog, "/header öffnet Dialog trotz Stream");
    assert_eq!(
        a.sessions[0].phase,
        Phase::WaitingForLLM,
        "Stream läuft weiter"
    );
}

#[test]
fn enter_waehrend_stream_normaler_text_wird_ignoriert() {
    let mut a = app();
    a.sessions[0].editor.set_text("erste nachricht");
    a.send_prompt(0); // phase → WaitingForLLM
                      // Neuen Text während des Streams eintippen und Enter drücken.
    a.sessions[0].editor.set_text("zweite nachricht");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::WaitingForLLM, "Phase bleibt unverändert");
    let ids: Vec<_> = s.chat.order().to_vec();
    assert_eq!(ids.len(), 1, "kein zweiter UserPrompt während des Streams");
    assert_eq!(
        s.editor.text_string(),
        "zweite nachricht",
        "Text bleibt zum Weitertippen im Feld"
    );
}

#[test]
fn enter_waehrend_stream_run_und_kompaktierung_bleiben_gesperrt() {
    let mut a = app();
    a.sessions[0].editor.set_text("erste nachricht");
    a.send_prompt(0); // phase → WaitingForLLM
                      // `/run` während des Streams: kein zweiter Tool-Worker, Text bleibt stehen.
    a.sessions[0].editor.set_text("/run echo hi");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::WaitingForLLM, "kein neuer Tool-Lauf");
    assert!(s.error.is_none(), "user_run wird gar nicht erreicht");
    assert_eq!(
        s.editor.text_string(),
        "/run echo hi",
        "Befehl bleibt stehen"
    );
    // `/compact` während des Streams: bleibt ebenfalls gesperrt.
    a.sessions[0].editor.set_text("/compact");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::WaitingForLLM);
    assert!(!s.compacting, "keine Kompaktierung während des Streams");
    assert_eq!(s.editor.text_string(), "/compact", "Befehl bleibt stehen");
}

// ── Terminal-Titel (`aidev · <status> · <tab>`) ──────────────────────

#[test]
fn terminal_title_idle_einzelne_session() {
    let mut a = app();
    a.refresh_tab_labels();
    // Ohne Kanal ist der Tab-Titel der Modellname der Session.
    assert_eq!(a.busy_status(), "idle");
    assert_eq!(a.terminal_title_string(), "aidev · idle · test/m");
}

#[test]
fn terminal_title_busy_einzelne_session_ohne_zaehler() {
    let mut a = app();
    a.sessions[0].editor.set_text("hallo");
    a.send_prompt(0); // phase → WaitingForLLM
    a.refresh_tab_labels();
    // Eine Session + busy → Zähler entfällt (nur „busy“).
    assert_eq!(a.busy_status(), "busy");
    assert_eq!(a.terminal_title_string(), "aidev · busy · test/m");
}

#[test]
fn terminal_title_busy_mehrere_sessions_mit_zaehler() {
    // 1 von 2 Sessions beschäftigt.
    let mut a = app();
    a.new_session();
    a.sessions[0].editor.set_text("hallo");
    a.send_prompt(0);
    a.refresh_tab_labels();
    assert_eq!(a.busy_status(), "1/2 busy");
    assert_eq!(a.terminal_title_string(), "aidev · 1/2 busy · test/m");

    // Beide Sessions beschäftigt.
    let mut a = app();
    a.new_session();
    a.sessions[0].editor.set_text("hallo");
    a.send_prompt(0);
    a.active = 1;
    a.sessions[1].editor.set_text("hallo2");
    a.send_prompt(1);
    a.refresh_tab_labels();
    assert_eq!(a.busy_status(), "2/2 busy");
}

#[test]
fn terminal_title_folgt_dem_aktiven_tab() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/m", None), ("test/fast", None)]);
    a.new_session();
    // Session 1 trägt einen abweichenden Modell-Alias → anderes Label.
    a.sessions[1].model_alias = Some("test/fast".to_string());
    a.active = 0;
    a.refresh_tab_labels();
    assert!(
        a.terminal_title_string().ends_with("test/m"),
        "Titel trägt Label der aktiven Session: {}",
        a.terminal_title_string()
    );

    // Tab-Wechsel auf Session 1 → Titel übernimmt deren Label.
    a.active = 1;
    a.refresh_tab_labels();
    assert!(
        a.terminal_title_string().ends_with("test/fast"),
        "nach Tab-Wechsel neues Label: {}",
        a.terminal_title_string()
    );
}

#[test]
fn shift_enter_fuegt_umbruch_ein_enter_sendet_mehrzeilig() {
    let mut a = app();
    let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);

    // "Zeile eins" tippen, dann Shift+Enter → Umbruch statt Senden.
    for c in "Zeile eins".chars() {
        a.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.handle_key(shift_enter);
    assert_eq!(
        a.sessions[0].editor.text_string(),
        "Zeile eins\n",
        "Shift+Enter fügt einen Umbruch ein"
    );
    assert_ne!(
        a.sessions[0].phase,
        Phase::WaitingForLLM,
        "Shift+Enter sendet nicht"
    );

    // "Zeile zwei" in der zweiten Zeile tippen und mit Enter absenden.
    for c in "Zeile zwei".chars() {
        a.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    assert_eq!(a.sessions[0].editor.text_string(), "Zeile eins\nZeile zwei");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(a.sessions[0].phase, Phase::WaitingForLLM);
    let ids: Vec<_> = a.sessions[0].chat.order().to_vec();
    match &a.sessions[0].chat.event(ids[0]).unwrap().kind {
        EventKind::UserPrompt { text, .. } => {
            assert_eq!(
                text, "Zeile eins\nZeile zwei",
                "mehrzeiliger Text wird gesendet"
            )
        }
        other => panic!("erwartet UserPrompt, bin {other:?}"),
    }
}

#[test]
fn paste_fuegt_mehrzeiligen_text_ein_statt_zu_senden() {
    let mut a = app();
    // Vorhandene Eingabe, dann mehrzeiligen Text in die Mitte einfügen.
    for c in "Foo".chars() {
        a.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.handle_paste("Bar\nBaz".to_string());
    assert_eq!(
        a.sessions[0].editor.text_string(),
        "FooBar\nBaz",
        "Paste fügt Text mit Zeilenumbrüchen ein"
    );
    assert_ne!(
        a.sessions[0].phase,
        Phase::WaitingForLLM,
        "Paste-Umbruch darf nicht senden"
    );

    // Erneutes Einfügen ersetzt eine aktive Selektion.
    a.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    for _ in 0..3 {
        a.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
    }
    assert_eq!(a.sessions[0].editor.selected_range(), Some(0..3)); // "Foo"
    a.handle_paste("neu".to_string());
    assert_eq!(a.sessions[0].editor.text_string(), "neuBar\nBaz");

    // Enter leert das Feld und sendet den gesamten (mehrzeiligen) Text.
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(a.sessions[0].phase, Phase::WaitingForLLM);
    let ids: Vec<_> = a.sessions[0].chat.order().to_vec();
    match &a.sessions[0].chat.event(ids[0]).unwrap().kind {
        EventKind::UserPrompt { text, .. } => assert_eq!(text, "neuBar\nBaz"),
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

    a.tx.send(llm::WorkerEvent::ToolStart {
        session: 0,
        tool_call_id: "call_1".into(),
        function_name: "read".into(),
        arguments: "{\"path\":\"x\"}".into(),
        label: "read x".into(),
    })
    .unwrap();
    a.tx.send(llm::WorkerEvent::ToolOutput(0, "inhalt".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity()))
        .unwrap();
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
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());

    // Zwischenrunde (assistant mit tool_calls): Text, dann DAS USAGE DIESER
    // RUNDE, dann Tool-Aufruf – Reihenfolge wie im Worker nach §4.3-Fix.
    a.tx.send(llm::WorkerEvent::Chunk(0, "Ich schaue nach.".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Usage(
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
    a.tx.send(llm::WorkerEvent::ToolStart {
        session: 0,
        tool_call_id: "call_1".into(),
        function_name: "read".into(),
        arguments: "{\"path\":\"x\"}".into(),
        label: "read x".into(),
    })
    .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity()))
        .unwrap();
    // Finale Runde (ohne tool_calls), danach Done.
    a.tx.send(llm::WorkerEvent::Chunk(0, "Antwort.".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Usage(
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
        EventKind::Assistant {
            num_tokens_text, ..
        } => *num_tokens_text,
        _ => unreachable!(),
    };
    // Beide leiten ihre Text-Tokens aus dem Usage IHRER Runde ab (kein ~0).
    assert!(
        mid > 0,
        "Zwischenantwort liefert abgeleitete Tokens, war {mid}"
    );
    assert!(
        fin > 0,
        "finale Antwort liefert abgeleitete Tokens, war {fin}"
    );
}

#[test]
fn round_end_schliesst_tool_runde_sofort_ab() {
    use crate::llm::Usage;
    // Neue Worker-Reihenfolge: Usage → ToolStart → ToolEnd → RoundEnd, noch VOR
    // der Folge-Anfrage. Die Zwischenrunde muss danach sofort finalisiert sein
    // (`reported_usage` + `time_end`), ohne auf den ersten Chunk zu warten.
    let mut a = app();
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());

    a.tx.send(llm::WorkerEvent::Usage(
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
    a.tx.send(llm::WorkerEvent::ToolStart {
        session: 0,
        tool_call_id: "call_1".into(),
        function_name: "read".into(),
        arguments: "{\"path\":\"x\"}".into(),
        label: "read x".into(),
    })
    .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::RoundEnd(0)).unwrap();
    assert!(a.drain_events());

    {
        let s = &a.sessions[0];
        assert!(s.open_assistant_id.is_none(), "Runde sofort geschlossen");
        assert!(s.pending_usage.is_none(), "geparkte Usage verbraucht");
        let last = s.last_usage_current().expect("reported_usage steht sofort");
        assert_eq!(last.total_tokens, 540, "verified tokens ohne Folge-Chunk");
    }
    // Idempotent: doppeltes RoundEnd (z. B. nach Cancel/Error) ist ein No-Op.
    a.tx.send(llm::WorkerEvent::RoundEnd(0)).unwrap();
    assert!(a.drain_events());
    assert_eq!(
        a.sessions[0]
            .chat
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Assistant { .. }))
            .count(),
        1,
        "keine Phantom-Runde durch doppeltes RoundEnd"
    );

    // Der nächste Chunk öffnet eine NEUE Runde (kein Anhängen an die alte).
    a.tx.send(llm::WorkerEvent::Chunk(0, "Antwort.".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Done(0)).unwrap();
    assert!(a.drain_events());
    assert_eq!(
        a.sessions[0]
            .chat
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Assistant { .. }))
            .count(),
        2,
        "Folge-Chunk öffnet neue Runde"
    );
}

#[test]
fn end_to_end_gemessene_parts_ueberleben_bis_zum_tool_event() {
    use crate::llm::Usage;
    // Abbild des `response.txt`-Mitschnitts: Reasoning + 2 Tool-Calls in einer
    // Runde, danach finale Antwort. Die beim Streaming gemessenen Parts müssen
    // bis in die Tool-Events durchkommen (und den zweiten derive-Lauf der
    // finalen Runde überleben).
    let mut a = app();
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());

    a.tx.send(llm::WorkerEvent::Reasoning(0, "Gedanken.".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Usage(
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
    a.tx.send(llm::WorkerEvent::ToolStart {
        session: 0,
        tool_call_id: "call_8f1dd6088e2d4f46a434135b".into(),
        function_name: "glob".into(),
        arguments: "{\"pattern\": \"*\"}".into(),
        label: "glob *".into(),
    })
    .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::ToolStart {
        session: 0,
        tool_call_id: "call_04276ce441d9441786fe240d".into(),
        function_name: "glob".into(),
        arguments: "{\"pattern\": \"**/*\"}".into(),
        label: "glob **/*".into(),
    })
    .unwrap();
    a.tx.send(llm::WorkerEvent::ToolEnd(0, tool_activity()))
        .unwrap();
    // Finale Antwort + Done (dritter derive-Lauf über den ganzen Turn).
    a.tx.send(llm::WorkerEvent::Chunk(0, "Zusammenfassung.".into()))
        .unwrap();
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
        EventKind::Assistant {
            num_tokens_reasoning: 38,
            ..
        }
    ));
}

#[test]
fn abbruch_erzeugt_abort_event_und_phase_idle() {
    let mut a = app();
    a.sessions[0].push_user_message("frage".into(), Some(Permission::Read), "m".into());
    a.tx.send(llm::WorkerEvent::Chunk(0, "halb".into()))
        .unwrap();
    a.tx.send(llm::WorkerEvent::Cancelled(0)).unwrap();
    assert!(a.drain_events());

    let s = &a.sessions[0];
    assert_eq!(s.phase, Phase::Idle);
    assert!(s.aborted, "Abbruch markiert");
    assert!(
        s.chat.iter().any(|e| matches!(e.kind, EventKind::Abort)),
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

#[test]
fn prompt_tokens_nach_compaction_zaehlt_nur_aktuellen_kontext() {
    let mut s = Session::new(0);
    // Alter Turn (wird kompaktiert, bleibt im Chat erhalten, zählt nicht mehr).
    s.push_user_message("alte frage".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("gedanken alt".into(), "antwort alt".into());
    s.chat
        .finalize_assistant(a1, std::time::Instant::now(), zero_usage(), 0, 0, false);
    // Summary mit exakter (vom Compaction-Aufruf gelieferter) Token-Zahl.
    let summary_tokens = 5;
    s.chat.compact(
        2,
        "[Compressed history - 1 earlier messages]\n\nzusammenfassung".into(),
        summary_tokens,
    );
    // Neuer Turn NACH der Summary.
    s.push_user_message("neue frage".into(), Some(Permission::Read), "m".into());
    let a2 = s.open_assistant("gedanken neu".into(), "antwort neu".into());
    s.chat
        .finalize_assistant(a2, std::time::Instant::now(), zero_usage(), 0, 0, false);

    let got = super::prompt_tokens(&s);
    // Nur Kontext ab der letzten Summary: exakte Summary-Tokens + Heuristik
    // der nachfolgenden Events. Die archivierten Nachrichten zählen NICHT.
    let expected = summary_tokens
        + llm::estimate_tokens("neue frage")
        + llm::estimate_tokens("gedanken neu")
        + llm::estimate_tokens("antwort neu");
    assert_eq!(got, expected, "archivierte Events fließen nicht mehr ein");
    assert!(
        got < 40,
        "ohne Aufräumen würde der alte Kontext (22T) mit reinzählen"
    );
}

#[test]
fn should_compact_verwendet_nach_compaction_nicht_altes_usage() {
    use crate::config::ResolvedEndpoint;
    // Konfiguration: Schwelle = 160_000 (context_window 200k · compact_at 0.8).
    let cfg = base_config();
    let ep = ResolvedEndpoint {
        model: "test/m".into(),
        api_model: "m".into(),
        base_url: "http://127.0.0.1:1".into(),
        api_key: "x".into(),
        user_agent: String::new(),
        context_window: 200_000,
    };
    let mut s = Session::new(0);
    let usage = |p: u64, c: u64| llm::Usage {
        prompt_tokens: p,
        completion_tokens: c,
        total_tokens: p + c,
        cached_tokens: None,
    };
    // Turn 1 (wird archiviert): Kontext bis 100_000 – unter der Schwelle.
    s.push_user_message("frage eins".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("gedanken eins".into(), "antwort eins".into());
    s.chat.finalize_assistant(
        a1,
        std::time::Instant::now(),
        usage(90_000, 10_000),
        0,
        0,
        false,
    );
    // Turn 2 (gerade über der Schwelle): sein Usage entscheidet über `should_compact`.
    s.push_user_message("frage zwei".into(), Some(Permission::Read), "m".into());
    let a2 = s.open_assistant("gedanken zwei".into(), "antwort zwei".into());
    s.chat.finalize_assistant(
        a2,
        std::time::Instant::now(),
        usage(150_000, 20_000),
        0,
        0,
        false,
    );

    // Vor der Kompaktierung: der letzte Usage (170_000) liegt über der Schwelle → kompaktieren.
    assert!(
        should_compact(&s, &cfg, &ep),
        "letzter Usage über der Schwelle → Kompaktierung"
    );

    // Kompaktierung (via Session): Turn 1 wird archiviert, Turn 2 überlebt.
    s.apply_compaction("zusammenfassung".into(), 1_000, 0);

    // UNMITTELBAR danach: Der echte aktuelle Kontext ist klein (Summary + Survivor-Shift)
    // → KEINE weitere Kompaktierung nötig, auch wenn das rohe `reported_usage`
    // der überlebenden Events noch den alten, gegen die gelöschte (größere)
    // Historie gemessenen Wert trägt.
    let live = super::prompt_tokens(&s);
    assert!(
        live < 160_000,
        "aktueller Kontext nach Kompaktierung liegt unter der Schwelle (war {live})"
    );
    assert!(
        !should_compact(&s, &cfg, &ep),
        "Doppelkompaktierung: nach dem Einbau der Summary gilt der ALTE Usage nicht mehr"
    );
}

#[test]
fn last_usage_current_verwirft_usage_vor_der_compaction() {
    let mut s = Session::new(0);
    // Alter Turn mit (serverbestätigtem) Usage – VOR der Summary.
    s.push_user_message("alte frage".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("gedanken alt".into(), "antwort alt".into());
    s.chat.finalize_assistant(
        a1,
        std::time::Instant::now(),
        llm::Usage {
            prompt_tokens: 900,
            completion_tokens: 100,
            total_tokens: 1000,
            cached_tokens: None,
        },
        0,
        0,
        false,
    );
    assert!(
        s.last_usage_current().is_some(),
        "ohne Archive ist der Usage aktuell"
    );
    // Compaction: Archive NACH dem alten Turn einfügen – dessen Usage ist
    // jetzt veraltet (stammt aus der größeren Historie).
    s.chat.compact(2, "zusammenfassung".into(), 5);
    assert!(
        s.last_usage_current().is_none(),
        "Usage vor dem Archive wird verworfen"
    );
    // `prompt_tokens` (die gespeicherte `context_len`-Mechanik) bleibt klein:
    // keine Doppelkompaktierung direkt nach dem Einbau der Summary.
    assert!(
        super::prompt_tokens(&s) <= 50,
        "gespeicherte Kontextlänge nach Summary bleibt klein"
    );
    // Neuer Turn NACH der Summary: dessen Usage reflektiert den neuen Kontext.
    s.push_user_message("neue frage".into(), Some(Permission::Read), "m".into());
    let a2 = s.open_assistant("gedanken neu".into(), "antwort neu".into());
    s.chat.finalize_assistant(
        a2,
        std::time::Instant::now(),
        llm::Usage {
            prompt_tokens: 500,
            completion_tokens: 20,
            total_tokens: 520,
            cached_tokens: None,
        },
        0,
        0,
        false,
    );
    assert_eq!(
        s.last_usage_current().map(|u| u.total_tokens),
        Some(520),
        "Turn nach der Compaction liefert wieder aktuellen Usage"
    );
}

fn zero_usage() -> llm::Usage {
    llm::Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: None,
    }
}

// ── /run und Key-Handling ─────────────────────────────────────────────────

#[test]
fn parse_run_line_extrahiert_ausdruck() {
    assert_eq!(
        parse_run_line("/run cargo build && git status").as_deref(),
        Some("cargo build && git status")
    );
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
fn slash_new_erzeugt_neue_session_wie_ctrl_n() {
    // `/new` nutzt exakt dieselbe Funktion wie Ctrl+N: neue Session anhängen
    // und aktivieren, mit leerem Editor und Default-Kanal.
    let mut a = app();
    assert_eq!(a.sessions.len(), 1);

    a.sessions[0].editor.set_text("/new");
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(a.sessions.len(), 2, "/new erzeugt eine neue Session/Tab");
    assert_eq!(a.active, 1, "die neue Session ist aktiv");
    assert!(
        a.sessions[1].editor.text_string().is_empty(),
        "neue Session startet mit leerer Eingabe"
    );
    assert_eq!(
        a.sessions[0].phase,
        Phase::Idle,
        "alte Session bleibt unangetastet"
    );

    // Vergleich mit Ctrl+N: gleicher Sessions-Endzustand (2 Sessions, aktive = 1).
    let mut b = app();
    b.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(
        b.sessions.len(),
        2,
        "Ctrl+N erzeugt ebenfalls eine neue Session"
    );
    assert_eq!(b.active, 1);
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
    a.tx.send(llm::WorkerEvent::HttpHeaders(
        0,
        vec![
            ("content-type".into(), "text/event-stream".into()),
            ("date".into(), "Tue, 01 Jan 2025 00:00:00 GMT".into()),
        ],
    ))
    .unwrap();
    assert!(a.drain_events());
    let h = a.sessions[0]
        .last_http_headers
        .as_ref()
        .expect("Header gespeichert");
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
    assert!(
        a.http_headers_dialog,
        "andere Tasten behalten den Dialog offen"
    );
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!a.http_headers_dialog, "Esc schließt den Header-Dialog");
}

// ── Options-Dialog (Ctrl+O) auf Basis der ListNav-Abstraktion ───────────────

#[test]
fn ctrl_o_oeffnet_options_dialog_mit_cursor_0() {
    let mut a = app();
    assert!(a.options_dialog.is_none(), "kein Options-Dialog zu Beginn");
    a.handle_key(ctrl_o());
    let d = a
        .options_dialog
        .as_ref()
        .expect("Ctrl+O öffnet den Options-Dialog");
    assert_eq!(d.nav.cursor(), 0);
    assert_eq!(
        d.nav.len(),
        3,
        "drei Optionen: Version (Info), Maus, Modell"
    );
    // Der Dialog ist modal: auch ohne weitere Zustände bleibt er offen,
    // bis eine Schließ-Taste kommt.
    a.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(
        a.options_dialog.is_some(),
        "fremde Tasten lassen den Dialog offen"
    );
}

#[test]
fn options_dialog_navigation_bewegt_cursor_geklemmt() {
    let mut a = app();
    a.handle_key(ctrl_o());
    // Runter: 0 → 1; weiter runter klemmt bei drei Optionen (max 2).
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(a.options_dialog.as_ref().unwrap().nav.cursor(), 1);
    a.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        a.options_dialog.as_ref().unwrap().nav.cursor(),
        2,
        "am Ende klemmt 'j'"
    );
    // Hoch: 2 → 1; weiter hoch klemmt am Anfang.
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(a.options_dialog.as_ref().unwrap().nav.cursor(), 1);
    a.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(
        a.options_dialog.as_ref().unwrap().nav.cursor(),
        0,
        "am Anfang klemmt 'k'"
    );
}

#[test]
fn options_dialog_esc_und_ctrl_o_schliessen() {
    let mut a = app();
    a.handle_key(ctrl_o());
    assert!(a.options_dialog.is_some());
    a.handle_key(ctrl_o());
    assert!(
        a.options_dialog.is_none(),
        "Ctrl+O schließt den Dialog wieder"
    );

    a.handle_key(ctrl_o());
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.options_dialog.is_none(), "Esc schließt den Dialog");
}

#[test]
fn options_dialog_enter_toggelt_maus_bei_cursor_1() {
    let mut a = app();
    assert!(!a.mouse_enabled);
    a.handle_key(ctrl_o());
    // Cursor 1 (Maus-Option) + Enter → Maus umschalten; Dialog bleibt offen
    // (Status sichtbar). Cursor 0 ist die reine Versions-Info.
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let before = a.mouse_enabled;
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_ne!(
        a.mouse_enabled, before,
        "Enter auf Maus-Option toggelt das Reporting"
    );
    assert!(a.options_dialog.is_some(), "Dialog bleibt offen");
}

#[test]
fn options_dialog_enter_bei_cursor_2_aendert_nicht_die_maus() {
    let mut a = app();
    a.handle_key(ctrl_o());
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → Maus (1)
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → Modell (2)
    let before = a.mouse_enabled;
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        a.mouse_enabled, before,
        "Enter auf der Modell-Option toggelt nichts"
    );
    assert!(a.options_dialog.is_some(), "Dialog bleibt offen");
}

// ── Statuszeile: keine Haupt-Tastenkürzel, solange ein Dialog offen ist ────

/// Text der linken Statuszeilen-Hälfte (ohne Styling) für Assertions.
fn status_text(a: &App) -> String {
    let line = crate::ui::status_left(a, 160);
    line.spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn statuszeile_idle_ohne_dialog_zeigt_kurzeln() {
    let a = app();
    let text = status_text(&a);
    assert!(
        text.contains("new session") && text.contains("options"),
        "Idle ohne Dialog: Tastenkürzel sichtbar – war: {text:?}"
    );
}

#[test]
fn statuszeile_leer_wenn_dialog_offen_und_idle() {
    let mut a = app();
    // Options-Dialog öffnen (Ctrl+O) → Idle + Dialog offen.
    a.handle_key(ctrl_o());
    assert!(a.any_dialog_open());
    let text = status_text(&a);
    assert!(
        text.trim().is_empty(),
        "Dialog offen + Session idlet → Statuszeile leer – war: {text:?}"
    );
}

#[test]
fn statuszeile_zeigt_thinking_trotz_dialog_bei_beschaeftigt() {
    let mut a = app();
    // Session beschäftigt (WaitingForLLM), dann Dialog öffnen.
    a.sessions[0].phase = Phase::WaitingForLLM;
    a.handle_key(ctrl_o());
    assert!(a.any_dialog_open());
    let text = status_text(&a);
    assert!(
        text.contains("thinking"),
        "beschäftigt + Dialog offen → 'thinking…' bleibt – war: {text:?}"
    );
}

// ── Bestätigungsdialoge (PreSend/Exec/Stop/ChannelClose) auf ListNav ────────

#[test]
fn exec_confirm_enter_sendet_ja_nein_ueber_reply() {
    let (tx, rx) = mpsc::channel();
    let mut a = app();
    a.exec_confirm = Some(ExecConfirm {
        label: "run cargo test".into(),
        command: "cargo test".into(),
        nav: ListNav::new(2), // Cursor 0 = „Yes, run"
        reply: tx,
    });
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.recv(), Ok(true), "Cursor 0 bestätigt die Ausführung");
    assert!(
        a.exec_confirm.is_none(),
        "Dialog schließt nach der Entscheidung"
    );
}

#[test]
fn exec_confirm_navigation_und_esc_lehnen_ab() {
    // Navigation auf „No, decline" → Enter sendet false.
    let (tx, rx) = mpsc::channel();
    let mut a = app();
    a.exec_confirm = Some(ExecConfirm {
        label: "run cargo test".into(),
        command: "cargo test".into(),
        nav: ListNav::new(2),
        reply: tx,
    });
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(a.exec_confirm.as_ref().unwrap().nav.cursor(), 1);
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(rx.recv(), Ok(false), "Option „No, decline“ lehnt ab");

    // Esc lehnt ebenfalls ab (ohne Cursor-Bewegung).
    let (tx2, rx2) = mpsc::channel();
    let mut a = app();
    a.exec_confirm = Some(ExecConfirm {
        label: "x".into(),
        command: "x".into(),
        nav: ListNav::new(2),
        reply: tx2,
    });
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(rx2.recv(), Ok(false), "Esc lehnt ab");
}

#[test]
fn stop_confirm_default_abbrechen_und_quitt_per_enter() {
    // Default-Cursor 1 (Abbrechen, sicher): Enter beendet NICHT.
    let mut a = app();
    a.stop_confirm = Some(StopConfirm {
        entries: vec![],
        more: 0,
        nav: ListNav::new_at(2, 1),
    });
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!a.quit, "Abbrechen-Option quittet nicht");
    assert!(a.stop_confirm.is_none(), "Dialog schließt nach Enter");

    // Cursor auf „Yes, quit & stop containers" schieben → Enter quittet.
    let mut a = app();
    a.stop_confirm = Some(StopConfirm {
        entries: vec![],
        more: 0,
        nav: ListNav::new_at(2, 1),
    });
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)); // 1 → 0
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.quit, "Cursor 0 + Enter beendet das Programm");
}

#[test]
fn pre_send_confirm_default_abbrechen_und_bewegung_klemmt() {
    let mut a = app();
    a.pre_send_confirm = Some(PreSendConfirm {
        session: 0,
        nav: ListNav::new_at(3, 2), // Default: Abbrechen
    });
    // Runter klemmt am Ende (Option 2 = Abbrechen).
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(a.pre_send_confirm.as_ref().unwrap().nav.cursor(), 2);
    // Hoch: 2 → 1 → 0 → klemmt am Anfang.
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(a.pre_send_confirm.as_ref().unwrap().nav.cursor(), 1);
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(a.pre_send_confirm.as_ref().unwrap().nav.cursor(), 0);
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        a.pre_send_confirm.as_ref().unwrap().nav.cursor(),
        0,
        "am Anfang klemmt hoch"
    );
    // Esc schließt den Dialog, ohne etwas abzuschicken.
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.pre_send_confirm.is_none());
}

// ── Model-Picker (Selection<ModelPick>, model_pick_list, Cursor) ──────────────

fn apply_refresh(a: &mut App, ids: &[(&str, Option<u64>)]) {
    a.model_registry.apply_refresh(
        ids.iter()
            .map(|(s, d)| (s.to_string(), *d))
            .collect::<Vec<_>>()
            .as_slice(),
    );
}

#[test]
fn model_pick_list_default_wird_eingefuegt_wenn_fehlend() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/fast", None), ("test/strong", None)]);
    // Default-Modell „test/m" (aus base_config) fehlt → (Standard) an Pos 0.
    let list = a.model_pick_list();
    assert_eq!(list.len(), 3, "(Standard) + 2 Modelle");
    assert!(
        matches!(list[0], ModelPick::Default),
        "erster Eintrag ist (Standard)"
    );
    let keys: Vec<_> = list
        .iter()
        .filter_map(|it| match it {
            ModelPick::Model { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec!["test/fast", "test/strong"]);
}

#[test]
fn model_pick_list_kein_default_wenn_vorhanden() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/m", None), ("test/fast", None)]);
    let list = a.model_pick_list();
    assert_eq!(list.len(), 2, "kein (Standard), da Default vorhanden");
    assert!(
        matches!(&list[0], ModelPick::Model { key, .. } if key == "test/m"),
        "erster Eintrag ist das Default-Modell"
    );
}

#[test]
fn open_model_picker_positioniert_cursor_auf_aktuelles_modell() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/fast", None), ("test/strong", None)]);
    // Session ohne Alias → Cursor auf (Standard) (Pos 0).
    a.open_model_picker();
    let p = a.model_picker.as_ref().expect("Picker offen");
    assert_eq!(
        p.items.nav.cursor(),
        0,
        "ohne Alias → Cursor auf (Standard)"
    );
    assert!(matches!(p.items.selected(), Some(ModelPick::Default)));
    // Alias setzen → Cursor zeigt auf dieses Modell.
    a.sessions[0].model_alias = Some("test/fast".into());
    a.open_model_picker();
    let p = a.model_picker.as_ref().expect("Picker offen");
    assert!(
        matches!(p.items.selected(), Some(ModelPick::Model { key, .. }) if key == "test/fast"),
        "Cursor liegt auf der Konfiguration"
    );
}

#[test]
fn model_picker_select_setzt_alias_und_esc_verwirft() {
    let mut a = app();
    apply_refresh(&mut a, &[("test/fast", None), ("test/strong", None)]);
    a.open_model_picker();
    // Down → erstes echtes Modell (Pos 1 = test/fast nach (Standard)).
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.model_picker.is_none(), "Picker schließt nach Enter");
    assert_eq!(
        a.sessions[0].model_alias.as_deref(),
        Some("test/fast"),
        "Alias gesetzt"
    );
    // Picker erneut öffnen; Cursor liegt auf dem zuletzt gesetzten Alias.
    a.open_model_picker();
    // Einmal hoch → (Standard) an Position 0, dann Enter → Alias wird None.
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        a.sessions[0].model_alias.is_none(),
        "Alias zurückgesetzt auf (Standard)"
    );
    // Esc schließt ohne Änderung.
    a.open_model_picker();
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.model_picker.is_none());
    // Session-Alias ist noch der letzte Wert (nicht None, da Esc nicht Select war).
    assert_eq!(a.sessions[0].model_alias.as_deref(), None);
}

// ── Kanal-Picker (Selection<ChannelPick>) ────────────────────────────────────

#[test]
fn default_model_alias_vorrang_in_resolve_und_display() {
    // Config: model = "prov/mod", [models.mod] id = "prov/bla" → der Default
    // meint den Alias "mod", gesendet wird dessen Servername "bla".
    let cfg: Config = toml::from_str(
        r#"
        model = "prov/mod"

        [provider.prov]
        base_url = "http://127.0.0.1:1"
        api_key = "x"

        [models.mod]
        id = "prov/bla"
        context_window = 4096

        [models.other]
        id = "prov/mod"
        "#,
    )
    .expect("TOML lesbar");

    let (tx, rx) = mpsc::channel();
    let a = App::new(
        cfg,
        ChannelRegistry::new_with_warnings(&base_config()).0,
        tx,
        rx,
    );

    // Default-Session (kein Session-Alias): resolve_endpoint löst das Modellfeld
    // ZUERST als Alias auf → Anzeige "prov/mod", gesendet "bla".
    let ep = a.resolve_endpoint(0).expect("Default auflösbar");
    assert_eq!(ep.model, "prov/mod", "Anzeige-Form provider/alias");
    assert_eq!(ep.api_model, "bla", "Servername aus dem Alias");
    assert_eq!(ep.context_window, 4096, "context_window aus dem Alias-Eintrag");

    // display_model zeigt dieselbe Alias-Form.
    assert_eq!(a.display_model(0), "prov/mod");

    // Picker: Default-Key = Alias-Key "prov/mod" ist in der Liste enthalten,
    // es wird kein zusätzlicher "(Standard)"-Eintrag eingefügt.
    let list = a.model_pick_list();
    assert!(
        list.iter().any(|p| matches!(p, ModelPick::Model { key, .. } if key == "prov/mod")),
        "Alias-Eintrag im Picker gelistet"
    );
    assert!(
        !list.iter().any(|p| matches!(p, ModelPick::Default)),
        "Default-Modell ist selbst gelistet → kein (Standard)-Eintrag"
    );
}

#[test]
fn open_channel_picker_baut_eintraege() {
    let mut a = app();
    a.open_channel_picker();
    let p = a.channel_picker.as_ref().expect("Picker offen");
    assert_eq!(
        p.items.nav.len(),
        2,
        "NoChannel + NewChannel (keine Channels konfiguriert)"
    );
    assert!(
        matches!(p.items.selected(), Some(ChannelPick::NoChannel)),
        "Cursor startet bei (no channel)"
    );
    // Down → NewChannel.
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let p = a.channel_picker.as_ref().expect("Picker noch offen");
    assert!(matches!(p.items.selected(), Some(ChannelPick::NewChannel)));
}

#[test]
fn channel_select_verbindet_oder_trennt() {
    let mut a = app();
    // Enter auf (no channel) → Session getrennt.
    a.open_channel_picker();
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.channel_picker.is_none());
    assert!(
        a.sessions[0].channel.is_none(),
        "Session hat keinen Kanal mehr"
    );
    // Esc schließt den Picker ohne Änderung.
    a.open_channel_picker();
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.channel_picker.is_none());
}

#[test]
fn channel_picker_bewegung_klemmt() {
    let mut a = app();
    a.open_channel_picker();
    // Hoch am Anfang klemmt; Runter am Ende klemmt.
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert!(matches!(
        a.channel_picker.as_ref().unwrap().items.selected(),
        Some(ChannelPick::NoChannel)
    ));
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → NewChannel
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // klemmt
    assert!(matches!(
        a.channel_picker.as_ref().unwrap().items.selected(),
        Some(ChannelPick::NewChannel)
    ));
}

fn active_confirm_cursor(a: &App) -> usize {
    match &a
        .channel_close
        .as_ref()
        .expect("Kanal-Schließ-Dialog offen")
        .phase
    {
        ChannelClosePhase::ActiveConfirm { nav } => nav.cursor(),
        _ => panic!("erwartet ActiveConfirm-Phase"),
    }
}

// ── Channel Builder (drei Selection-Spalten mit Umlauf) ──────────────────────

/// Direkt gebauter Builder-State mit Local-Tunnel: Navigation löst keine
/// Podman-/Git-Subprozesse aus (`update_builder_container` kehrt bei Local
/// früh zurück; die Pfade existieren nicht → kein git).
fn builder_state() -> ChannelBuilderState {
    ChannelBuilderState {
        tunnels: Selection::wrap_at(
            vec![
                crate::channel::builder::Tunnel::Local,
                crate::channel::builder::Tunnel::Image {
                    name: "img".into(),
                    working_dir: None,
                },
            ],
            0,
        ),
        host_paths: Selection::wrap_at(
            vec![
                crate::channel::builder::HostPath { path: "/a".into() },
                crate::channel::builder::HostPath { path: "/b".into() },
            ],
            0,
        ),
        worktrees: Selection::wrap_at(Vec::new(), 0),
        col: 1, // Host-Spalte aktiv
        container_info: None,
        images_loaded: true,
        current_is_git: false,
        argv_path: None,
        edit: None,
        edit_error: None,
    }
}

/// Builder-State mit sichtbarer Worktree-Spalte (Repo gewählt): zwei
/// Branches, der zweite („feat“) markiert, ohne Worktree – so lässt sich die
/// `b`-Branch-Eingabe ohne echte Git-Subprozesse testen.
fn builder_state_with_repo() -> ChannelBuilderState {
    let mut b = builder_state();
    b.col = 2;
    b.current_is_git = true;
    b.worktrees = Selection::wrap_at(
        vec![
            crate::channel::builder::WorktreeEntry {
                label: "/a (main)".into(),
                path: "/a".into(),
                branch: "main".into(),
                is_main: true,
                has_worktree: true,
            },
            crate::channel::builder::WorktreeEntry {
                label: "feat (Kein Worktree)".into(),
                path: "".into(),
                branch: "feat".into(),
                is_main: false,
                has_worktree: false,
            },
        ],
        1,
    );
    b
}

#[test]
fn builder_host_spalte_wrapt_und_spaltenwechsel_klemmt() {
    let mut a = app();
    a.channel_builder = Some(builder_state());
    // Down: 0 → 1; weiter: 1 → 0 (Umlauf); Hoch: 0 → 1 (Umlauf zurück).
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        a.channel_builder.as_ref().unwrap().host_paths.nav.cursor(),
        1
    );
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        a.channel_builder.as_ref().unwrap().host_paths.nav.cursor(),
        0
    );
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        a.channel_builder.as_ref().unwrap().host_paths.nav.cursor(),
        1
    );
    // Links → Tunnel-Spalte; Rechte → Ordnung: bei Nicht-Repo nicht weiter
    // nach rechts als bis Host (links nicht unter 0).
    a.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(a.channel_builder.as_ref().unwrap().col, 0);
    a.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(
        a.channel_builder.as_ref().unwrap().col,
        0,
        "links klemmt bei 0"
    );
    a.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(a.channel_builder.as_ref().unwrap().col, 1);
    a.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(
        a.channel_builder.as_ref().unwrap().col,
        1,
        "bei Nicht-Repo klemmt rechts bei 1"
    );
}

#[test]
fn builder_b_branch_field_nur_in_worktree_spalte_und_esc_schliesst() {
    let mut a = app();
    a.channel_builder = Some(builder_state_with_repo());
    // `b` in der Host-Spalte öffnet KEIN Feld.
    a.channel_builder.as_mut().unwrap().col = 1;
    a.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert!(
        a.channel_builder.as_ref().unwrap().edit.is_none(),
        "b in der Host-Spalte darf kein Feld öffnen"
    );
    // `b` in der Worktree-Spalte öffnet das Branch-Feld, vorbelegt mit dem
    // Namen des markierten Branchs („feat“).
    a.channel_builder.as_mut().unwrap().col = 2;
    a.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    match &a.channel_builder.as_ref().unwrap().edit {
        Some(BuilderEdit::Branch(ed)) => assert_eq!(ed.text_string(), "feat"),
        other => panic!("erwartet Branch-Editor, war {:?}", other.is_some()),
    }
    // Esc verwirft das Feld wieder.
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.channel_builder.as_ref().unwrap().edit.is_none());
}

#[test]
fn builder_branch_enter_leer_zeigt_fehler_und_hält_offen() {
    let mut a = app();
    a.channel_builder = Some(builder_state_with_repo());
    a.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    // Feld leeren („feat“ entfernen) und Enter: leerer Name → Fehler, Feld bleibt.
    if let Some(BuilderEdit::Branch(ed)) = &mut a.channel_builder.as_mut().unwrap().edit {
        ed.set_text("");
    }
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let b = a.channel_builder.as_ref().unwrap();
    assert!(
        matches!(b.edit, Some(BuilderEdit::Branch(_))),
        "Feld bleibt bei Fehler offen"
    );
    assert!(
        b.edit_error
            .as_deref()
            .is_some_and(|e| e.contains("branch")),
        "Fehlermeldung gesetzt, war {:?}",
        b.edit_error
    );
    // Korrigieren: jetzt einen gültigen Namen setzen – ein echter git-Aufruf
    // würde nur mit realem Repo laufen; hier reicht der Esc-Abbruch.
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.channel_builder.as_ref().unwrap().edit.is_none());
}

#[test]
fn builder_pfad_existiert_nicht_oeffnet_bestaetigungsdialog() {
    let mut a = app();
    a.channel_builder = Some(builder_state());
    // „a“ öffnet das Pfad-Eingabefeld.
    a.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
    let missing = std::env::temp_dir()
        .join(format!("aidev-nx-{}", std::process::id()))
        .join("neu");
    if let Some(BuilderEdit::HostPath(ed)) = &mut a.channel_builder.as_mut().unwrap().edit {
        ed.set_text(&missing.to_string_lossy());
    }
    // Enter auf nicht existierenden Pfad → Bestätigungsdialog statt Übernahme.
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        a.path_confirm.is_some(),
        "nicht existierender Pfad muss den Bestätigungsdialog öffnen"
    );
    assert_eq!(
        a.path_confirm.as_ref().unwrap().path,
        missing,
        "Dialog trägt den eingegebenen Pfad"
    );
    assert!(
        a.channel_builder.as_ref().unwrap().edit.is_none(),
        "Eingabefeld ist nach Enter geschlossen"
    );
}

#[test]
fn builder_pfad_confirm_esc_kehrt_zur_eingabe_zurueck() {
    let mut a = app();
    a.channel_builder = Some(builder_state());
    let missing = std::env::temp_dir()
        .join(format!("aidev-pec-{}", std::process::id()))
        .join("neu");
    a.path_confirm = Some(PathConfirm {
        path: missing.clone(),
    });
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.path_confirm.is_none(), "Dialog geschlossen");
    let b = a.channel_builder.as_ref().unwrap();
    match &b.edit {
        Some(BuilderEdit::HostPath(ed)) => {
            assert_eq!(
                ed.text_string(),
                missing.to_string_lossy(),
                "Eingabe behält den Pfad"
            )
        }
        other => panic!("erwartet Pfad-Editor, war offen={}", other.is_some()),
    }
    assert!(b.edit_error.is_none(), "Abbruch ohne Fehlermeldung");
}

#[test]
fn builder_pfad_confirm_enter_legt_an_und_registriert() {
    let mut a = app();
    a.channel_builder = Some(builder_state());
    let base = std::env::temp_dir().join(format!("aidev-mkdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let missing = base.join("a/b/c");
    a.path_confirm = Some(PathConfirm {
        path: missing.clone(),
    });
    a.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.path_confirm.is_none(), "Dialog geschlossen");
    assert!(missing.is_dir(), "Verzeichnis per mkdir -p angelegt");
    let b = a.channel_builder.as_ref().unwrap();
    assert!(
        b.host_paths.items.iter().any(|hp| hp.path == missing),
        "Pfad in der Host-Liste registriert"
    );
    assert_eq!(b.host_paths.nav.cursor(), b.host_paths.items.len() - 1);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn channel_close_active_confirm_bewegt_und_esc_oeffnet_picker_wieder() {
    let mut a = app();
    a.channel_close = Some(ChannelClose {
        name: "test-channel".into(),
        kind: CloseKind::Picker,
        phase: ChannelClosePhase::ActiveConfirm {
            nav: ListNav::new_at(2, 1), // Default: Abbrechen (sicher)
        },
    });
    // Runter klemmt bei 1 (letzte Option), Hoch 1 → 0 → klemmt bei 0.
    a.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(active_confirm_cursor(&a), 1);
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(active_confirm_cursor(&a), 0);
    a.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(active_confirm_cursor(&a), 0, "am Anfang klemmt hoch");
    // Esc bei Picker-Ursprung → Dialog zu, Picker wieder offen.
    a.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(a.channel_close.is_none());
    assert!(
        a.channel_picker.is_some(),
        "Picker-Ursprung öffnet den Picker wieder"
    );
}

#[test]
fn reload_liefert_neue_config_und_baut_registries_um() {
    let mut a = app();
    // Ausgangszustand: keine Models, keine Kanäle.
    assert_eq!(a.model_registry.len(), 0);
    assert!(a.channels.names().is_empty());

    // Frische Config mit Modell + Local-Kanal.
    let mut models = indexmap::IndexMap::new();
    models.insert(
        "fast".to_string(),
        crate::config::ModelConfig::Plain("test/neu".into()),
    );
    let mut channels = std::collections::HashMap::new();
    channels.insert(
        "testchannel".to_string(),
        crate::config::ChannelConfig {
            kind: "local".into(),
            image: None,
            container: None,
            run_container: None,
            workdir: "/app".into(),
            host_root: Some(std::env::temp_dir().display().to_string()),
            home: None,
        },
    );
    let fresh = crate::config::Config {
        model: "test/neu".into(),
        default_channel: Some("testchannel".into()),
        channels,
        models,
        theme: "light".into(),
        mouse: true,
        ..base_config()
    };

    // `apply_reloaded_config` schaltet über `set_theme` das GLOBALE Theme um
    // (hier auf "light"). Im parallelen Testlauf würde das andere UI-Tests
    // stören, die `theme().ok` als erwartete Farbe lesen (flaky „grün nicht
    // gefunden" in der Kompaktierungs-Verifikation). Globalen Zustand daher
    // nach dem Aufruf wiederherstellen.
    let old_theme = crate::ui::theme();
    a.apply_reloaded_config(fresh, Vec::new());
    crate::ui::set_theme(old_theme);

    assert_eq!(a.config.model, "test/neu");
    assert_eq!(a.config.theme, "light");
    assert!(a.mouse_enabled, "Maus aus neuer Config übernommen");
    assert_eq!(
        a.model_registry.get("test/fast").map(|e| e.display_key()),
        Some("test/fast".to_string()),
        "Modell-Registry neu aufgebaut"
    );
    assert!(
        a.channels.get("testchannel").is_some(),
        "Kanal neu registriert"
    );
    assert_eq!(a.channels.default_channel_name(), Some("testchannel"));
    assert_eq!(
        a.sessions[0].error.as_deref(),
        Some("Config neu geladen."),
        "Statusmeldung gesetzt"
    );
}
