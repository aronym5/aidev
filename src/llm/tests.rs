//! Tests für die LLM-Schicht: Tool-Definitionen, Delta-Akkumulation,
//! Wire-Verträge, Hilfe-Funktionen und Kompaktierungs-Grenzen.
//!
//! Nach der Migration auf das Event-Log (`chat`) testet dieses Modul nur noch
//! die Architektur-unabhängige Logik (Werkzeuge, Draht-Format, Helfer) – nicht
//! mehr das alte `ChatMessage`-Layout.

use super::*;
use crate::config::{Config, ProviderConfig};
use crate::llm;
use crate::perm::Permission;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

// ── Tool-Delta-Akkumulation (tools_def) ───────────────────────────────────

#[test]
fn tool_call_deltas_werden_akkumuliert() {
    let mut accs: Vec<ToolCallAcc> = Vec::new();
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"id":"call_1","type":"function",
                "function":{"name":"read","arguments":"{\"path\":"}}]}),
    );
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Cargo.toml\"}"}}]}),
    );
    assert_eq!(accs.len(), 1);
    assert_eq!(accs[0].id, "call_1");
    assert_eq!(accs[0].name, "read");
    assert_eq!(accs[0].arguments, "{\"path\":\"Cargo.toml\"}");
}

#[test]
fn mehrere_tool_indizes_werden_getrennt_gesammelt() {
    let mut accs: Vec<ToolCallAcc> = Vec::new();
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"grep","arguments":"{\"pattern\":\"x\"}"}}]}),
    );
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":1,"id":"b","function":{"name":"read","arguments":"{\"path\":\"y\"}"}}]}),
    );
    assert_eq!(accs.len(), 2);
}

// ── Tool-Definitionen (tools_def) ─────────────────────────────────────────

#[test]
fn tool_definitions_enthalten_alle_werkzeuge() {
    let defs = tool_definitions(Permission::Execute);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    for want in ["grep", "read", "glob", "webfetch", "write", "run", "edit"] {
        assert!(names.contains(&want), "Werkzeug fehlt: {want}");
    }
}

#[test]
fn tool_definitions_folgen_der_berechtigung() {
    let defs = |p: Permission| {
        tool_definitions(p)
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<String>>()
    };
    assert_eq!(defs(Permission::Read), ["grep", "read", "glob", "webfetch"]);
    assert_eq!(
        defs(Permission::Write),
        ["grep", "read", "glob", "webfetch", "edit", "write"]
    );
    assert_eq!(
        defs(Permission::Execute),
        ["grep", "read", "glob", "webfetch", "edit", "write", "run"]
    );
}

#[test]
fn sanitize_arguments_normalisiert_auf_gueltiges_json() {
    // Bereits gültiges JSON bleibt unverändert.
    assert_eq!(sanitize_arguments("{\"a\":1}"), "{\"a\":1}");
    // Ungültiges/leeres → {} (keine naive Quote-Umschreibung).
    assert_eq!(sanitize_arguments(""), "{}");
    assert_eq!(sanitize_arguments("{'path':'Cargo.toml'}"), "{}");
}

// ── Wire-Verträge (wire) ──────────────────────────────────────────────────

#[test]
fn ensure_reasoning_fuellt_leeres_feld_fuer_tool_calls_nach() {
    let assistant = WireMessage {
        role: "assistant".into(),
        content: Some("Antwort".into()),
        reasoning_content: None,
        tool_calls: Some(vec![WireToolCall {
            id: "call_1".into(),
            ty: "function".into(),
            function: WireFunction {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }]),
        tool_call_id: None,
    };
    let norm = ensure_reasoning_for_tool_calls(std::slice::from_ref(&assistant));
    assert_eq!(norm[0].reasoning_content.as_deref(), Some(""));
    // Nicht-Tool-Turns bleiben unangetastet.
    let mut plain = assistant;
    plain.tool_calls = None;
    plain.reasoning_content = None;
    let norm2 = ensure_reasoning_for_tool_calls(&[plain]);
    assert_eq!(norm2[0].reasoning_content, None);
}

// ── Token-Schätzung (mod.rs) ──────────────────────────────────────────────

#[test]
fn estimate_tokens_rundet_auf_vier_zeichen() {
    assert_eq!(llm::estimate_tokens(""), 1);
    assert_eq!(llm::estimate_tokens("abcd"), 2);
    assert_eq!(llm::estimate_tokens("abcdefgh"), 3);
}

// ── HTTP-Parsing / Retry (http) ───────────────────────────────────────────

#[test]
fn parse_usage_liest_tokens() {
    let j = json!({"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}});
    let u = parse_usage(&j).expect("Usage vorhanden");
    assert_eq!(u.prompt_tokens, 10);
    assert_eq!(u.completion_tokens, 4);
    assert_eq!(u.total_tokens, 14);
    assert_eq!(parse_usage(&json!({"x":1})), None);
}

#[test]
fn retry_backoff_waechst_und_jitter_bleibt_in_grenzen() {
    assert_eq!(retry_delay(0), Duration::from_secs(2));
    let d1 = retry_delay(1);
    let d2 = retry_delay(3);
    assert!(d2 > d1, "Backoff muss wachsen");
}

// ── Kompaktierungs-Grenzen (compact) ──────────────────────────────────────

#[test]
fn wire_compact_boundary_zaehlt_turns_an_user_nachrichten() {
    let w = |role: &str| WireMessage {
        role: role.into(),
        content: Some("x".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    };
    // Turn 1 mit Werkzeug-Runde: user → assistant(tool_calls) → tool → tool
    let mut tool = w("assistant");
    tool.tool_calls = Some(vec![WireToolCall {
        id: "c".into(),
        ty: "function".into(),
        function: WireFunction {
            name: "read".into(),
            arguments: "{}".into(),
        },
    }]);
    let msgs = vec![
        w("user"),
        tool,
        w("tool"),
        w("tool"),
        w("user"),
        w("assistant"),
        w("user"),
    ];
    // Die Zählung läuft über `user`-Nachrichten (identisch zu
    // `compact_boundary` auf der Session-Seite): eine mehrteilige Tool-Runde
    // ist Teil ihres Turns und wird komplett mit archiviert.
    // keep=1 → es bleiben die letzten 2 User (turn2 + Pending user3) übrig;
    // Turn 1 (Indizes 0..4, samt Werkzeug-Runde) wird archiviert.
    assert_eq!(wire_compact_boundary(&msgs, 1), 4);
    // keep=2 → auch der vorletzte Turn würde gebraucht; mit nur 3 Usern gibt
    // es nichts hinter der Grenze zu archivieren.
    assert_eq!(wire_compact_boundary(&msgs, 2), 0);
    // keep größer als die Turn-Zahl → nichts zu archivieren.
    assert_eq!(wire_compact_boundary(&msgs, 9), 0);
}

#[test]
fn context_length_fehler_wird_erkannt() {
    assert!(looks_like_context_error("Request too large: maximum context length"));
    assert!(looks_like_context_error("prompt is too long for the model"));
    assert!(!looks_like_context_error("rate limit exceeded"));
}

#[test]
fn kompaktierung_zu_kurze_historie_meldet_abbruch() {
    let client = shared_client();
    let cfg = test_config("http://127.0.0.1:1");
    let ep = cfg.resolve(None).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Ohne Turn-Grenzen gibt es nichts zu kompaktieren.
    let msgs = vec![WireMessage {
        role: "user".into(),
        content: Some("nur eine".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];
    let err = compact_chat_messages(0, client, &cfg, &ep, &msgs, &cancel).unwrap_err();
    assert!(!err.is_empty());
}

// ── Helfer (helpers) ──────────────────────────────────────────────────────

#[test]
fn truncate_bleibt_kurz_unveraendert() {
    assert_eq!(truncate("", 10), "");
    assert_eq!(truncate("kurz", 10), "kurz");
}

#[test]
fn with_debug_haengt_pfad_als_eigene_zeile_an() {
    assert_eq!(with_debug("Kurz".into(), None), "Kurz");
    assert!(with_debug("Kurz".into(), Some("/tmp/x.json".into())).contains("/tmp/x.json"));
}

#[test]
fn server_error_summary_extrahiert_kurze_einzeilige_meldung() {
    let raw = r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#;
    let s = server_error_summary(raw, 100);
    assert!(s.contains("bad request"));
}

#[test]
fn reasoning_contract_hint_erklaert_nur_die_einschlaegige_meldung() {
    let hint = reasoning_contract_hint("error: reasoning_content must be passed back");
    assert!(hint.contains("Note:"), "sollte den Hinweis anhängen");
    assert_eq!(
        reasoning_contract_hint("ganz anderes problem"),
        "ganz anderes problem",
        "andere Meldung bleibt unverändert"
    );
}

#[test]
fn civil_from_days_liefert_lesbares_datum() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(31), (1970, 2, 1));
}

// ── Completion-Verteilung beim Streaming (http) ────────────────────────────

#[test]
fn distribute_weights_summiert_exakt_mit_rest_im_letzten() {
    // Die Summe ist IMMER exakt `total` – Rundungsreste schluckt der letzte
    // positive Anteil. Das gilt auch bei ungleichen Gewichten (0/1/…) und 0.
    for (total, weights) in [
        (100, vec![30, 30, 40]),
        (1, vec![1, 1, 1]),
        (7, vec![2, 2, 2]),
        (5, vec![1, 0, 1]),
        (0, vec![10]),
        (9, vec![7]),
    ] {
        let out = distribute_weights(total, &weights);
        assert_eq!(out.len(), weights.len());
        assert_eq!(
            out.iter().copied().sum::<u64>(),
            total,
            "Summe exakt für {total} / {weights:?}: {out:?}"
        );
    }
}

#[test]
fn accumulator_verteilt_inkrement_proportional_und_setzt_zurueck() {
    let mut acc = RoundPartsAccumulator::default();
    // Bereich 1: viel reasoning.
    acc.track_reasoning(100);
    // usage mit Inkrement 20 → geht komplett ins reasoning.
    acc.apply_usage(20);
    // Bereich 2: reasoning nochmal + content.
    acc.track_reasoning(25);
    acc.track_content(75);
    // usage mit Inkrement 40 → 25:75 ⇒ 10|30.
    acc.apply_usage(60);
    let parts = acc.parts();
    assert_eq!((parts.reasoning, parts.content), (30, 30));
}

#[test]
fn accumulator_reset_erlaubt_mehrere_usage_events() {
    let mut acc = RoundPartsAccumulator::default();
    // Erst nur reasoning, dann nur content – je eigenes usage.
    acc.track_reasoning(50);
    acc.apply_usage(10); // 10 → reasoning
    acc.track_content(50);
    acc.apply_usage(30); // 20 → content (inc = 30-10)
    let parts = acc.parts();
    assert_eq!(parts.reasoning, 10, "reasoning bleibt exakt");
    assert_eq!(parts.content, 20, "content wird danach gemessen");
}

#[test]
fn accumulator_tool_calls_werden_pro_index_verteilt() {
    let mut acc = RoundPartsAccumulator::default();
    acc.track_content(100);
    acc.track_tool(0, 25);
    acc.track_tool(1, 75);
    // usage-Inkrement 40 über content 100 + call0 25 + call1 75 (=200):
    // content 20, call0 5, call1 15.
    acc.apply_usage(40);
    let parts = acc.parts();
    assert_eq!(parts.content, 20);
    assert_eq!(parts.tool_calls, vec![5, 15]);
    // Summe exakt.
    assert_eq!(
        parts.reasoning + parts.content + parts.tool_calls.iter().copied().sum::<u64>(),
        40
    );
}

#[test]
fn accumulator_nur_finales_usage_verteilt_ueber_ganze_runde() {
    // Server liefert nur EIN usage am Ende → alles wird über die gesamt-
    // gemessenen Bytes verteilt (≈ bisherige proportionale Aufteilung).
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(60);
    acc.track_content(40);
    acc.track_tool(0, 100);
    acc.apply_usage(200);
    let parts = acc.parts();
    // 60:40:100 über 200 ⇒ 60|40|100.
    assert_eq!((parts.reasoning, parts.content, parts.tool_calls.as_slice()), (60, 40, &[100][..]));
}

#[test]
fn accumulator_null_inkrement_verteilt_nichts() {
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(10);
    acc.track_content(90);
    acc.apply_usage(0);
    let parts = acc.parts();
    assert_eq!((parts.reasoning, parts.content), (0, 0));
    // parts-Objekt ist Default (leer).
    assert!(parts.tool_calls.is_empty());
}

#[test]
fn accumulator_tool_kopf_mit_null_bytes_geht_an_aktive_sektion() {
    // Tool-Kopf-Delta (id/name, leere Argumente) erzeugt 0 messbare Bytes,
    // aber der Server zählt dafür Completion-Tokens. Über die aktive Sektion
    // landet das Inkrement trotzdem beim Tool-Call statt verloren zu gehen.
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(100);
    acc.apply_usage(20); // reasoning + 20
    // active = Reasoning; das Tool-Kopf-Delta überschreibt es auf Slot 0.
    acc.track_tool(0, 0);
    acc.apply_usage(38); // inc = 18, keine Bytes → komplett an Tool 0
    let parts = acc.parts();
    assert_eq!(parts.reasoning, 20);
    assert_eq!(parts.tool_calls, vec![18]);
}

// ── Test-Helper ───────────────────────────────────────────────────────────

/// Test-Config mit einem einzigen Provider für gegebene URL.
fn test_config(base_url: &str) -> Config {
    let mut provider = HashMap::new();
    provider.insert(
        "test".to_string(),
        ProviderConfig {
            base_url: base_url.to_string(),
            api_key: Some("test".into()),
            user_agent: None,
        },
    );
    Config {
        model: "test/m".into(),
        provider,
        ..Config::default()
    }
}
