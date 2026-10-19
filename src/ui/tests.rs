//! UI-Tests der Event-basierten Render-Pipeline (`build_history_cache`,
//! `build_live_blocks`, Layout, Block-Aufbereitung).

use super::*;
use crate::app::{Session, ViewLevel};
use crate::chat::EventKind;
use crate::llm::Usage;
use crate::perm::Permission;

/// Überlebende Events zwischen Summary und neuem Turn: Ihre gespeicherte
/// Kontextlänge (`context_len`) wird beim Einschieben der Summary um den Shift
/// reduziert (grau); vor der Summary bleibt alles unverändert (grün), neue Turns
/// danach sind wieder grün.

#[test]
fn ueberlebende_events_nach_summary_werden_verschoben() {
    let mut s = Session::new(0);
    // Turn 1 – wird archiviert.
    s.push_user_message("alte frage".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("gedanken alt".into(), "antwort alt".into());
    s.chat.finalize_assistant(
        a1,
        std::time::Instant::now(),
        Usage {
            prompt_tokens: 900,
            completion_tokens: 50,
            total_tokens: 950,
            cached_tokens: None,
        },
        3,
        4,
        false,
    );
    // Turn 2 – ÜBERLEBT (bleibt NACH der Summary stehen).
    s.push_user_message("mittlere frage".into(), Some(Permission::Read), "m".into());
    let a2 = s.open_assistant("gedanken mittel".into(), "antwort mittel".into());
    s.chat.finalize_assistant(
        a2,
        std::time::Instant::now(),
        Usage {
            prompt_tokens: 1400,
            completion_tokens: 60,
            total_tokens: 1460,
            cached_tokens: None,
        },
        5,
        6,
        false,
    );
    // Kompaktierung über die Session (macht den echten Shift): Turn 1 archiviert.
    // `keep=0` wird wie `keep=1` behandelt (wie im Wire-Pfad
    // `wire_compact_boundary`): der LETZTE Turn bleibt IMMER stehen – also
    // Turn 2 als überlebendes Event zwischen Summary und neuem Turn.
    s.apply_compaction(
        "[Compressed history - 1 earlier messages]\n\nzusammen".into(),
        10,
        0,
    );

    // Turn 3 – NEUER Turn NACH der Kompaktierung.
    s.push_user_message("neue frage".into(), Some(Permission::Read), "m".into());
    let a3 = s.open_assistant("gedanken neu".into(), "antwort neu".into());
    s.chat.finalize_assistant(
        a3,
        std::time::Instant::now(),
        Usage {
            prompt_tokens: 2000,
            completion_tokens: 70,
            total_tokens: 2070,
            cached_tokens: None,
        },
        7,
        8,
        false,
    );

    let ids: Vec<_> = s.chat.order().to_vec();
    let cl = |id: crate::chat::EventId| s.chat.event(id).and_then(|e| e.context_len);
    let is_green = |id: crate::chat::EventId| s.chat.context_is_green(id);

    let find = |kind: &str, needle: &str| -> crate::chat::EventId {
        ids.iter()
            .copied()
            .find(|&id| {
                let e = s.chat.event(id).unwrap();
                let subj = match &e.kind {
                    EventKind::UserPrompt { text, .. } if kind == "U" => text.clone(),
                    EventKind::Assistant { reasoning, .. } if kind == "A" => reasoning.clone(),
                    EventKind::Archive { summary, .. } if kind == "S" => summary.clone(),
                    _ => return false,
                };
                subj.contains(needle)
            })
            .expect(needle)
    };

    // VOR der Summary: unverändert (grün, aus Usage abgeleitet).
    let u1 = cl(find("U", "alte frage")).unwrap();
    let a1c = cl(find("A", "gedanken alt")).unwrap();
    assert_eq!(
        (u1, a1c),
        (900, 950),
        "archivierte Events behalten ihre bestätigten Zahlen"
    );
    assert!(is_green(find("U", "alte frage")) && is_green(find("A", "gedanken alt")));

    // Shift = Kontext des letzten Events vor der Summary (A1: 950) − Summary (10).
    let summary_arch = find("S", "zusammen");
    assert_eq!(
        cl(summary_arch),
        Some(10),
        "Summary ist ihr eigener (grüner) Anker"
    );

    // Überlebende NACH der Summary: verschoben (alt − shift) und GRAU.
    let u2 = cl(find("U", "mittlere frage")).unwrap();
    let a2c = cl(find("A", "gedanken mittel")).unwrap();
    assert_eq!(
        (u2, a2c),
        (1400 - 940, 1460 - 940),
        "überlebende Zahl = alt − shift"
    );
    assert!(
        !is_green(find("U", "mittlere frage")),
        "überlebender Prompt ist grau (verschoben)"
    );
    assert!(
        !is_green(find("A", "gedanken mittel")),
        "überlebende Antwort ist grau (verschoben)"
    );

    // Neuer Turn NACH der Summary: wieder grün (neue Koordinaten).
    let u3 = cl(find("U", "neue frage")).unwrap();
    let a3c = cl(find("A", "gedanken neu")).unwrap();
    assert_eq!(
        (u3, a3c),
        (2000, 2070),
        "neue Runde misst im verkleinerten Kontext"
    );
    assert!(is_green(find("U", "neue frage")) && is_green(find("A", "gedanken neu")));
}

/// Session mit einem abgeschlossenen User/Assistant-Turn.
fn session_with_history() -> Session {
    let mut s = Session::new(0);
    s.push_user_message(
        "Hallo Modell".into(),
        Some(Permission::Read),
        "model-x".into(),
    );
    let aid = s.open_assistant("Gedanken".into(), "Hallo!".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), Usage::default(), 1, 1, false);
    s
}

#[test]
fn history_cache_baut_bloecke_fuer_user_und_assistant() {
    let mut s = session_with_history();
    s.view = crate::app::ViewLevel::Overview;
    let (blocks, ctx) = build_history_cache(&s, 100, "model-x", 8192);

    assert!(
        blocks.len() >= 2,
        "Overview: User-Zeile + Assistant-Textzeile, bekam {}",
        blocks.len()
    );
    // Ohne künstlichen Grundanteil zählt der Schätzer nur die Event-Token; im
    // Overview-Modus werden User/Assistant über `add_content` verbucht, also
    // ist `used` > 0.
    assert!(ctx.used > 0, "Context-Estimator hält einen Wert");
    // Ohne offene Events ist der Live-Tail leer.
    let (live, _live_used) = build_live_blocks(&s, 100, 8192, &ctx);
    assert!(live.is_empty(), "keine offenen Events → kein Live-Tail");
}

#[test]
fn themewechsel_macht_den_history_cache_ungueltig() {
    // `/theme` soll bereits gezeichnete Texte SOFORT neu einfärben: `set_theme`
    // erhöht die Theme-Version, und `ensure_history_cache` baut den Cache beim
    // nächsten Frame neu auf – auch wenn Breite/Ansicht/Historie unverändert
    // sind (die Historie selbst bleibt unangetastet, kein history_version-Bump).
    let mut s = session_with_history();
    let (version_vorher, history_vorher) = {
        ensure_history_cache(&mut s, 100, "model-x", 8192);
        (
            s.history_cache
                .as_ref()
                .expect("Cache nach ensure")
                .theme_version,
            s.history_version,
        )
    };

    set_theme(resolve(ThemeChoice::Dark));
    assert!(
        theme_version() > version_vorher,
        "set_theme erhöht die Theme-Version"
    );

    // Nächster Frame ohne weitere Änderungen → Rebuild mit neuer Theme-Farbe.
    ensure_history_cache(&mut s, 100, "model-x", 8192);
    assert_eq!(
        s.history_cache.as_ref().unwrap().theme_version,
        theme_version(),
        "Theme-Wechsel löst sofort einen Cache-Rebuild aus"
    );
    // Die Historie selbst wurde nicht angefasst – der Theme-Schlüssel allein
    // ist für den Rebuild verantwortlich.
    assert_eq!(
        s.history_version, history_vorher,
        "s.history_version bleibt unverändert"
    );

    // Globalen Theme-Zustand für andere (parallele) Tests wieder herstellen.
    set_theme(resolve(ThemeChoice::Dark));
}

#[test]
fn live_blocks_zeigen_offene_assistant_rune() {
    let mut s = Session::new(0);
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    s.ensure_open_assistant();
    s.append_text("laufende Antwort");

    let (_, ctx) = build_history_cache(&s, 100, "m", 8192);
    let (live, live_used) = build_live_blocks(&s, 100, 8192, &ctx);
    assert!(!live.is_empty(), "offene Assistant-Runde wird gerendert");
    // Der Live-Tail erweitert den Kontext über das Historie-Ende hinaus – genau
    // dieser finale `used`-Wert fließt als `live_context` (Live-Tail der
    // Übersichts-Balken-Schätzung) in die nächsten Frames ein.
    assert!(
        live_used > ctx.used,
        "Live-Tail erhöht die Context-Größe über das Historie-Ende: {} → {}",
        ctx.used,
        live_used
    );
}

#[test]
fn waehrend_der_tool_phase_liegen_runde_und_fertige_tools_in_der_historie() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    s.open_assistant(String::new(), "Antwort.".into());
    // Der erste Tool-Start beendet das Streaming: die Runde wird geschlossen
    // (Tool-Phase), ihre Werkzeuge sind danach die einzigen offenen Events.
    let aid = s.tool_parent().expect("Runde für die Tool-Phase");
    let t1 = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        r#"{"pattern":"*"}"#.into(),
        ToolKind::Glob {
            pattern: "*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    s.chat.close_event(t1, std::time::Instant::now()); // Tool 1 fertig (`ToolEnd`)
    let t2 = s.open_tool(
        Some(aid),
        "c2".into(),
        "read".into(),
        r#"{"path":"a.rs"}"#.into(),
        ToolKind::Read {
            path: "a.rs".into(),
            range: String::new(),
        },
    );

    let collect = |blocks: Vec<ChatBlock>| -> Vec<String> {
        blocks
            .into_iter()
            .flat_map(|b| b.lines.into_iter().map(|l| l.to_string()))
            .collect()
    };
    let (hist, ctx) = build_history_cache(&s, 100, "m", 8192);
    let (live, _live_used) = build_live_blocks(&s, 100, 8192, &ctx);
    let hist_text = collect(hist);
    let live_text = collect(live);
    // Runde und fertiges Tool 1 sind abgeschlossen → Historie (in Reihenfolge,
    // also auch in der richtigen Zeilenfolge über dem laufenden Tool 2).
    assert!(
        hist_text.iter().any(|l| l.contains("Antwort.")),
        "geschlossene Runde in der Historie: {hist_text:?}"
    );
    assert!(
        hist_text.iter().any(|l| l.contains("glob")),
        "fertiges Tool 1 in der Historie: {hist_text:?}"
    );
    // Nur das laufende Tool 2 ist offen → Live-Tail, ohne Tool 1 zu wiederholen.
    assert!(
        live_text
            .iter()
            .any(|l| l.contains("read") || l.contains("a.rs")),
        "laufendes Tool 2 im Live-Tail: {live_text:?}"
    );
    assert!(
        !live_text.iter().any(|l| l.contains("glob")),
        "fertiges Tool 1 nicht mehr im Live-Tail: {live_text:?}"
    );

    // Tool 2 fertig → auch es wandert in die Historie; kein offenes Event mehr.
    s.chat.close_event(t2, std::time::Instant::now());
    let (hist2, ctx2) = build_history_cache(&s, 100, "m", 8192);
    let (live2, _live_used) = build_live_blocks(&s, 100, 8192, &ctx2);
    let hist2_text = collect(hist2);
    assert!(
        hist2_text.iter().any(|l| l.contains("glob")),
        "geschlossen → Tool 1 in Historie: {hist2_text:?}"
    );
    assert!(live2.is_empty(), "kein offenes Event mehr → Live-Tail leer");
}

#[test]
fn confirmed_context_len_beruecksichtigt_tool_call_anteil() {
    use crate::chat::ToolKind;
    use crate::llm::CompletionParts;
    let usage = Usage {
        prompt_tokens: 987,
        completion_tokens: 111,
        total_tokens: 1098,
        cached_tokens: None,
    };
    let zero = Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: None,
    };

    // 1) Abschlussantwort ohne Tools → `total_tokens`.
    let mut s = Session::new(0);
    s.push_user_message("f1".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), "Antwort".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
    assert_eq!(s.chat.confirmed_context_len(aid), Some(1098));

    // 2) Tool-Runde mit gemessenen completion_parts → prompt + reasoning + content.
    let mut s = Session::new(0);
    s.push_user_message("f2".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), String::new());
    let t1 = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob {
            pattern: "*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    let t2 = s.open_tool(
        Some(aid),
        "c2".into(),
        "glob".into(),
        "{\"pattern\":\"**/*\"}".into(),
        ToolKind::Glob {
            pattern: "**/*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
    s.chat.set_tool_tokens(t1, 36, 100);
    s.chat.set_tool_tokens(t2, 37, 200);
    s.chat.set_completion_parts(
        aid,
        Some(CompletionParts {
            reasoning: 38,
            content: 0,
            tool_calls: vec![36, 37],
        }),
    );
    assert_eq!(
        s.chat.confirmed_context_len(aid),
        Some(987 + 38),
        "parts: prompt+reasoning+content"
    );

    // 3) Tool-Runde OHNE parts → `total_tokens` − ∑ tool num_tokens_input.
    let mut s = Session::new(0);
    s.push_user_message("f3".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), String::new());
    let t1 = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob {
            pattern: "*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    let t2 = s.open_tool(
        Some(aid),
        "c2".into(),
        "glob".into(),
        "{\"pattern\":\"**/*\"}".into(),
        ToolKind::Glob {
            pattern: "**/*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
    s.chat.set_tool_tokens(t1, 36, 100);
    s.chat.set_tool_tokens(t2, 37, 200);
    assert_eq!(
        s.chat.confirmed_context_len(aid),
        Some(1098 - 73),
        "ohne parts: total − tool-calls"
    );

    // 4) Ohne bestätigte total_tokens → None.
    let mut s = Session::new(0);
    s.push_user_message("f4".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), "x".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), zero, 0, 0, false);
    assert_eq!(s.chat.confirmed_context_len(aid), None);
}

#[test]
fn contentlose_runde_mit_usage_synct_used_trotzdem() {
    let usage = Usage {
        prompt_tokens: 420,
        completion_tokens: 80,
        total_tokens: 500,
        cached_tokens: None,
    };
    // Reine Gedanken-Runde: kein Text (nur reasoning), aber serverbestätigte Usage.
    let mut s = Session::new(0);
    s.view = crate::app::ViewLevel::Overview; // Context-Balken/Schätzung nur in der Übersicht
    s.push_user_message("kurz".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("nur Gedanken, kein Text".into(), String::new());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);

    let (blocks, ctx) = build_history_cache(&s, 100, "m", 8192);
    let texts: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    // In der Übersicht wird die Gedanken-Runde als Zeile übersprungen …
    assert!(
        !texts.iter().any(|l| l.contains("Gedanken")),
        "Reasoning ohne Text erscheint nicht als Zeile: {texts:?}"
    );
    // … aber `used` springt trotzdem auf die serverbestätigte Zahl (der
    // gespeicherte `context_len`-Anker wird aus `reported_usage` abgeleitet)
    // läuft für jede geschlossene Runde, nicht nur für Text-Runden).
    assert_eq!(
        ctx.used, 500,
        "verified tokens fließen trotz leerem Text in die Schätzung"
    );
}

#[test]
fn contentlose_runde_ohne_usage_behaelt_reasoning_schaetzung() {
    // Keine Usage (Resync springt nicht): die reine Gedanken-Runde darf die
    // laufende Schätzung beim Schließen nicht wieder sinken lassen – sie
    // verhält sich wie der Live-Tail (Reasoning-Anteil wird mitgezählt).
    let mut s = Session::new(0);
    s.view = crate::app::ViewLevel::Overview;
    s.push_user_message("kurz".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("nur Gedanken".into(), String::new());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), Usage::default(), 0, 0, false);

    let (_, ctx) = build_history_cache(&s, 100, "m", 8192);
    let est_user = crate::llm::estimate_tokens("kurz");
    let est_reasoning = crate::llm::estimate_tokens("nur Gedanken");
    assert_eq!(
        ctx.used,
        est_user + est_reasoning,
        "Reasoning-Schätzung bleibt ohne Usage in `used`"
    );
    assert_eq!(
        ctx.block_sum,
        est_user + est_reasoning,
        "Reasoning trägt auch zur zweiten Zahl bei (Context-Schätzung, keine Zeile im Overview)"
    );
}

#[test]
fn assistant_zeile_annotiert_reasoning_und_text_getrennt() {
    fn assistant_line(s: &Session) -> String {
        let (blocks, _ctx) = build_history_cache(s, 100, "m", 8192);
        blocks
            .iter()
            .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
            .find(|l| l.contains("Antwort"))
            .expect("Assistant-Textzeile")
    }

    // Reasoning + Text: beide Tokenzahlen einzeln als „res+txt“ (analog Tool-
    // Call+Antwort) – nicht mehr nur die Textzahl.
    let mut s = Session::new(0);
    s.view = crate::app::ViewLevel::Overview;
    s.push_user_message("kurz".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("Gedanken".into(), "Antwort".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), Usage::default(), 5, 7, false);
    let line = assistant_line(&s);
    assert!(
        line.contains("5+7"),
        "Reasoning+Text erscheinen getrennt als `res+txt`, Zeile: {line:?}"
    );

    // Nur Text (kein Reasoning): unverändert die einzelne Tokenzahl.
    let mut s = Session::new(0);
    s.view = crate::app::ViewLevel::Overview;
    s.push_user_message("kurz".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), "Antwort".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), Usage::default(), 0, 7, false);
    let line = assistant_line(&s);
    assert!(
        line.contains("7") && !line.contains('+'),
        "ohne Reasoning bleibt die einzelne Textzahl, Zeile: {line:?}"
    );
}

#[test]
fn assistant_runden_verifizieren_das_event_davor() {
    use crate::chat::ToolKind;
    let t0 = std::time::Instant::now();
    let usage = |p: u64| Usage {
        prompt_tokens: p,
        completion_tokens: 100,
        total_tokens: p + 100,
        cached_tokens: None,
    };
    // Chronologie: User → A1 (CONTENT-LOS, nur Gedanken, Usage 420) → Tool T1
    // → A2 (Content, Usage 910). Nach dem Wunsch verifiziert JEDE abgeschlossene
    // Assistant-Runde das unmittelbar VOR ihr liegende Event mit ihren
    // `prompt_tokens` – besonders auch die content-losen.
    let mut s = Session::new(0);
    s.view = crate::app::ViewLevel::Overview;
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("nur Gedanken".into(), String::new());
    s.chat.finalize_assistant(a1, t0, usage(420), 0, 0, false);
    let t1 = s.open_tool(
        Some(a1),
        "c1".into(),
        "glob".into(),
        "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob {
            pattern: "*".into(),
            num_results: 2,
            num_results_reported: 2,
        },
    );
    s.chat.close_event(t1, t0);
    let a2 = s.open_assistant("fertig".into(), "Antwort".into());
    s.chat.finalize_assistant(a2, t0, usage(910), 0, 0, false);

    // Sichtbar: grüne Zahlen auf der User-Zeile (420T) und der Tool-Zeile (910T);
    // die content-lose Runde selbst rendert KEINE Zeile. Die Anker stammen aus
    // den gespeicherten `context_len`-Werten: der User-Prompt ist durch A1s
    // `prompt_tokens` (420) bestätigt, das Tool T1 durch A2s (910) – jeweils
    // noch unverschoben, also grün.
    let (blocks, _ctx) = build_history_cache(&s, 120, "m", 8192);
    let has_green = |blk: &ChatBlock, needle: &str| {
        blk.lines.iter().any(|l| {
            l.spans.iter().any(|sp| {
                sp.style.fg == Some(theme().ok) && sp.content.to_string().contains(needle)
            })
        })
    };
    let texts: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    assert!(
        !texts.iter().any(|l| l.contains("Gedanken")),
        "Content-lose Runde bleibt in der Übersicht unsichtbar: {texts:?}"
    );
    assert!(
        blocks.iter().any(|b| has_green(b, "420T")),
        "User-Zeile zeigt grüne 420T (von der content-losen Runde): {texts:?}"
    );
    assert!(
        blocks.iter().any(|b| has_green(b, "910T")),
        "Tool-Zeile zeigt grüne 910T (von der Content-Runde): {texts:?}"
    );
}

#[test]
fn tool_verifikation_verankert_used_absolut() {
    // Die Anzeige-Basis `used` hängt ausschließlich am gespeicherten
    // Kontext-Anker (`set_anchor` = context_len des Events); die Tool-Anteile
    // (`add_tool`) fließen nur in die Zusammensetzung (Balken-Segmente), nicht
    // in die erste Zahl – kein Doppelzählen einer Schätzung neben dem Anker.
    let mut ctx = ContextEstimate::base();
    ctx.add_tool("glob", 136);
    ctx.set_anchor(910, true);
    assert_eq!(
        ctx.used, 910,
        "gespeicherter Wert ersetzt die Schätzung absolut"
    );
    assert!(ctx.green, "unverschobene Bestätigung ist grün");
    assert_eq!(
        ctx.tools,
        vec![("glob".to_string(), 136)],
        "Zusammensetzung bleibt erhalten"
    );

    // Ohne Anker kein Sprung: `used` bleibt auf der Live-Basis (add_used).
    let mut c3 = ContextEstimate::base();
    c3.add_used(200);
    assert_eq!(c3.used, 200, "ohne Anker bleibt die Live-Basis");
}

#[test]
fn nachtraegliche_bestaetigung_passt_usage_bar_an_neue_daten_an() {
    // Die Gesamtlänge der usage-bar ist die erste Spalte (der gespeicherte
    // Kontextwert), sobald er vorliegt. Die farbigen Blöcke (Inhalts-Kategorien
    // + Tools) bleiben die Summen der Event-Token. Sobald ein Turn abgeschlossen
    // wird und eine bestätigte Kontextzahl vorliegt, springt `used`
    // (Balkenlänge) auf diese Zahl – der Farbanteile-Anteil bleibt unberührt.
    let mut ctx = ContextEstimate::base();
    // Zwei User/Agent-Events + ein Tool (Event-Summen wie in build_history_cache).
    ctx.add_content(ContentKind::User, 120); // User
    ctx.add_tool("glob", 136);
    ctx.add_content(ContentKind::Content, 80); // Assistant content
                                               // `add_*` bucht nur die Zusammensetzung; `used` wächst erst über die
                                               // gespeicherte `context_len` (Anker) bzw. die Live-Basis.
    assert_eq!(ctx.used, 0, "Zusammensetzung allein hebt `used` nicht an");

    // Die (gespeicherte) Bestätigung steuert die Balkenlänge (erste Spalte).
    ctx.set_anchor(900, true);
    assert_eq!(
        ctx.used, 900,
        "gespeicherte Zahl steuert die Balkenlänge (erste Spalte)"
    );
}

#[test]
fn kompaktierung_reset_und_shift_steuern_usage_bar_und_folgekontext() {
    // Simulation der Sequenz des Archive-Zweigs in `build_history_cache`:
    // Summary = eigener (grüner) Anker mit NUR Summary-Zusammensetzung; die
    // überlebenden Events danach zeigen ihre gespeicherte (in `apply_compaction`
    // um den Shift reduzierte) Kontextlänge als graue Zahl.
    let summary = 2_000;

    // ── Summary (Archive): eigene Token als grüner Anker; usage-bar besteht
    // ── nur aus der Summary in exakt dieser Länge (Rest auf null).
    let mut ctx = ContextEstimate::base();
    ctx.contents = [0; 5];
    ctx.contents[ContentKind::Summary as usize] = summary;
    ctx.tools = Vec::new();
    ctx.block_sum = summary;
    ctx.set_anchor(summary, true);
    assert_eq!(
        ctx.contents,
        [summary, 0, 0, 0, 0],
        "Zusammensetzung nur aus summary in exakt dieser Länge"
    );
    assert!(ctx.tools.is_empty(), "Tool-Anteile zurückgesetzt");
    assert_eq!(ctx.used, summary, "Summary ist der neue Kontext-Anker");
    assert!(ctx.green, "Summary-Anker ist grün (eigene exakte Zahl)");

    // ── Folge-Event 1 (überlebt, verschoben): die gespeicherte Kontextlänge
    // ── wurde in `apply_compaction` bereits um den Shift reduziert → grau.
    // `add_content` bucht nur die Zusammensetzung (Balken-Anteile), `used`
    // hängt am gespeicherten Anker (set_anchor).
    ctx.add_content(ContentKind::User, 300);
    ctx.set_anchor(52_100 - 48_000, false);
    assert_eq!(
        ctx.used, 4_100,
        "gespeicherte (verschobene) Kontextlänge steuert die erste Zahl"
    );
    assert!(!ctx.green, "verschobene Zahl ist grau");
    assert_eq!(
        ctx.contents,
        [summary, 300, 0, 0, 0],
        "Zusammensetzung akkumuliert weiter"
    );

    // ── Folge-Event 2: weiterer Tool-Anteil nur in der Zusammensetzung.
    ctx.add_tool("read", 120);
    assert_eq!(
        ctx.used, 4_100,
        "Tool-Anteil hebt `used` nicht an (Anker bleibt)"
    );
    assert_eq!(ctx.tools, vec![("read".to_string(), 120)]);

    // ── Ohne Kompaktierung bleibt ein bestätigter Wert grün.
    let mut plain = ContextEstimate::base();
    plain.add_tool("read", 200);
    plain.set_anchor(1200, true);
    assert_eq!(plain.used, 1200, "ohne Shift: Bestätigung unverändert");
    assert!(plain.green, "unverschobene Bestätigung ist grün");
}

#[test]
fn kompaktierung_verankert_summary_zeile_und_schiebt_folgeverifikationen() {
    let mut s = Session::new(0);
    // Alter Turn (wird kompaktiert, bleibt aber im Chat erhalten).
    s.push_user_message("alte frage".into(), Some(Permission::Read), "m".into());
    let a1 = s.open_assistant("gedanken alt".into(), "antwort alt".into());
    s.chat
        .finalize_assistant(a1, std::time::Instant::now(), Usage::default(), 0, 0, false);
    // Kontextlänge des letzten Events VOR der Summary (Overview-Schätzung des
    // alten Turns: User-Prompt + Reasoning, Text ohne Usage trägt 0).
    let old_before_summary =
        crate::llm::estimate_tokens("alte frage") + crate::llm::estimate_tokens("gedanken alt");
    // Summary bewusst deutlich KÜRZER als der ersetzte Kontext, damit der
    // Kompaktierungs-Shift echt reduziert (wie im echten Einsatz).
    let summary_tokens = 5;
    // Kompaktierung: Summary (Archive) an der Grenze einfügen.
    s.chat.compact(
        2,
        "[Compressed history - 1 earlier messages]\n\nzusammenfassung".into(),
        summary_tokens,
    );
    // Neuer Turn NACH der Summary (Tail) mit serverbestätigter Usage.
    s.push_user_message("neue frage".into(), Some(Permission::Read), "m".into());
    let a2 = s.open_assistant("gedanken neu".into(), "antwort neu".into());
    s.chat.finalize_assistant(
        a2,
        std::time::Instant::now(),
        Usage {
            prompt_tokens: 5200,
            completion_tokens: 40,
            total_tokens: 5240,
            cached_tokens: None,
        },
        1,
        2,
        false,
    );
    s.view = crate::app::ViewLevel::Overview;
    let (blocks, end_ctx) = build_history_cache(&s, 120, "m", 81920);

    // Kompaktierung: grüner Anker jetzt NUR auf der Summary-Zeile selbst
    // (eigene, kleine Tokenzahl). Alle Folge-Events NACH der Summary werden
    // GRAU und um den Shift reduziert angezeigt – bestätigte Rohwerte (grün)
    // stehen nach der Kompaktierung nicht mehr an den Zeilen.
    // shift = Kontext im letzten Event vor der Summary − Summary-Länge.
    let shift = old_before_summary - summary_tokens;
    assert!(shift > 0, "Testszenario braucht echten positiven Shift");

    let has_green = |blk: &ChatBlock, needle: &str| {
        blk.lines.iter().any(|l| {
            l.spans.iter().any(|sp| {
                sp.style.fg == Some(theme().ok) && sp.content.to_string().contains(needle)
            })
        })
    };
    // Summary-Zeile: eigene Tokenzahl als grüner Anker („5T“).
    let own = crate::ui::status::fmt_ctx(summary_tokens);
    assert!(
        blocks.iter().any(|b| has_green(b, &own)),
        "Summary-Zeile zeigt ihre eigene Tokenzahl als grünen Anker"
    );
    // NACH der Summary: Der neue Turn (prompt_tokens 5200) wurde gegen den
    // verkleinerten Kontext gemessen – er ist ein neuer Anker in NEUEN
    // Koordinaten (grün, unverschoben), und der Shift wird auf 0 zurückgesetzt.
    let green_val = crate::ui::status::fmt_ctx(5200);
    assert!(
        blocks.iter().any(|b| has_green(b, &green_val)),
        "Folgebestätigung nach Summary ist grün (neuer Koordinaten-Anker, prompt_tokens=5200)"
    );
    assert!(
        end_ctx.green,
        "Folge-Bestätigung in neuen Koordinaten ist grün (unverschoben)"
    );

    // usage-bar-Zusammensetzung am HISTORIE-ENDE = Daten darüber (nur die
    // Summary als Überbleibsel der alten Historie) + aktuelles Event:
    // Summary in exakt der Summary-Länge + Anteile des neuen Turns.
    let s_user = crate::llm::estimate_tokens("neue frage");
    let s_reasoning = 1; // num_tokens_reasoning des neuen Turns
    let s_text = 2; // num_tokens_text des neuen Turns
    assert_eq!(end_ctx.contents[0], summary_tokens, "Summary-Anteil exakt");
    assert_eq!(end_ctx.contents[1], s_user, "neuer User-Anteil");
    assert_eq!(end_ctx.contents[2], s_reasoning, "neuer Reasoning-Anteil");
    assert_eq!(end_ctx.contents[3], s_text, "neuer Content-Anteil");
    assert_eq!(end_ctx.contents[4], 0, "kein Other-Anteil");
    assert!(end_ctx.tools.is_empty(), "keine Tool-Anteile");

    // Der rundeigene verifizierte Fuß (total_tokens 5240) stammt aus dem
    // verkleinerten Kontext (neue Koordinaten) – kein Shift mehr.
    assert_eq!(
        end_ctx.used, 5240,
        "Folge-Verifikation in neuen Koordinaten (kein Shift)"
    );
}

#[test]
fn usage_bar_verwendet_partial_bloecke_fuer_subzeichengenaue_uebergaenge() {
    // Band + genau ein Tool: der Farbwechsel fällt mitten in eine Zelle, sodass
    // ein links-füllender Partial-Block (mit fg=Band, bg=Tool) erzeugt wird
    // statt eines harten Zeichen-Sprungs.
    let mut ctx = ContextEstimate::base();
    ctx.add_content(ContentKind::User, 4); // User
    ctx.add_tool("glob", 5); // Tool, größerer Anteil
    ctx.set_anchor(50, false); // used=50, window=100 → 50 % gefüllt

    let spans = context_bar(&ctx, 100, 20);
    // Gesamtzahl Zeichen = exakt `cells` Zellen (die Balkenbreite bleibt fix).
    let total_chars: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    assert_eq!(total_chars, 20, "Bar bleibt exakt `cells` Zellen breit");

    let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        combined.chars().any(|c| "▏▎▍▌▋▊▉".contains(c)),
        "sub-zeichengenauer Farbübergang erzeugt einen Partial-Block, bekam: {combined:?}"
    );
    // Der Partial-Block trägt zwei Farben: fg = erste Segmentfarbe, bg = nächste.
    let partial = spans
        .iter()
        .find(|s| s.content.chars().any(|c| "▏▎▍▌▋▊▉".contains(c)))
        .expect("Partial-Block vorhanden");
    let st = partial.style;
    assert!(
        st.fg.is_some() && st.bg.is_some(),
        "Partial-Block kombiniert Vorder- und Hintergrundfarbe"
    );
    assert_ne!(
        st.fg, st.bg,
        "die zwei Farben am Übergang unterscheiden sich"
    );
}

#[test]
fn overview_balken_bleibt_bei_zero_width_glyphen_ausgerichtet() {
    // Regression: `char_w` zählte Combining-/ZWJ-/Variation-Select-Zeichen
    // (echte Breite 0) dank `.max(1)` als 1 Zelle mit. Dadurch wurde ein
    // run-Befehl mit solchen Glyphen zu schmal vermessen → der rechtsbündige
    // Balken samt Token-Zahlen rutschte N Zellen nach links (und der Text
    // wurde „zu früh“ mit „…“ gekürzt). Breite-0-Zeichen müssen also 0 zählen.
    use crate::ui::blocks::overview_row;
    // Disp-Spalte wie das Terminal: Breite-0-Zeichen (Combining/ZWJ/VS) zählen
    // 0 – unabhängig davon, was `char_w` gerade tut. Nur so erkennt der Test,
    // ob die Vermessung in `overview_line` mit der echten Renderbreite überein-
    // stimmt, statt den (falschen) `char_w`-Wert inkonsistent zu spiegeln.
    fn disp_col(s: &str) -> usize {
        s.chars()
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }
    fn bar_at(line: &Line) -> usize {
        let mut pos = 0usize;
        for s in &line.spans {
            let t: String = s.content.chars().collect();
            if t.starts_with('█') || t.starts_with('░') || t.starts_with('▏') || t.starts_with('▎')
            {
                break;
            }
            pos += disp_col(&t);
        }
        pos
    }

    let mut ctx = ContextEstimate::base();
    ctx.add_content(ContentKind::User, 120);
    ctx.set_anchor(500, false);
    let window = 8192;
    let width = 120;
    let ann = Some(" 40+100".into());

    // Reiner ASCII-Befehl (Referenz, keine Breite-0-Zeichen); kurz genug,
    // dass er nicht gekürzt wird – so isoliert der Test die Zählung in der
    // Padding-Berechnung von etwaigen Truncation-Effekten.
    let ascii = "⚙ run cargo build --release";
    let zw = "⚙ run cargo b\u{0301}\u{200d}\u{fe0f}uild --release";

    let blk_ascii = overview_row(
        ascii.into(),
        Style::default(),
        width,
        &ctx,
        window,
        true,
        ann.clone(),
    );
    let blk_zw = overview_row(zw.into(), Style::default(), width, &ctx, window, true, ann);

    // Beide müssen den Balken an derselben Spalte beginnen lassen.
    assert_eq!(
        bar_at(&blk_ascii.lines[0]),
        bar_at(&blk_zw.lines[0]),
        "Breite-0-Glyphen dürfen den rechtsbündigen Balken nicht nach links verschieben"
    );
    // Und die Gesamtbreite beider Zeilen ist identisch (Balken rechtsbündig).
    let w_total = |b: &ChatBlock| {
        b.lines[0]
            .spans
            .iter()
            .map(|s| disp_width(&s.content))
            .sum::<usize>()
    };
    assert_eq!(w_total(&blk_ascii), w_total(&blk_zw));
}

#[test]
fn layout_blocks_reihen_logo_historie_live_auf() {
    let s = session_with_history();
    let (blocks, ctx) = build_history_cache(&s, 100, "m", 8192);
    let (live, _live_used) = build_live_blocks(&s, 100, 8192, &ctx);
    let logo = ChatBlock::plain(vec![Line::from("logo")], false);
    let (placed, total) = layout_blocks(&logo, &blocks, &live);
    assert_eq!(placed.len(), blocks.len() + 1, "Logo + Historie");
    assert!(total > 0);
    // Tops sind monoton steigend.
    for w in placed.windows(2) {
        assert!(w[1].top >= w[0].top);
    }
}

#[test]
fn anchor_roundtrip_bleibt_in_den_grenzen() {
    let tops = vec![0, 10, 20];
    let heights = vec![5, 5, 5];
    let a = line_to_anchor(&tops, &heights, 12);
    assert_eq!(anchor_to_line(&tops, &heights, a), 12);
    // Über das Ende hinaus wird geklemmt.
    let b = line_to_anchor(&tops, &heights, usize::MAX);
    assert!(b.block < tops.len());
}

#[test]
fn session_events_sind_chronologisch_geordnet() {
    let s = session_with_history();
    let kinds: Vec<&EventKind> = s.chat.iter().map(|e| &e.kind).collect();
    assert!(matches!(kinds[0], EventKind::UserPrompt { .. }));
    assert!(matches!(kinds[1], EventKind::Assistant { .. }));
}

#[test]
fn fusszeile_erscheint_nur_bei_antworten_ohne_tool_aufrufe() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    let t0 = std::time::Instant::now();
    s.push_user_message("frage1".into(), Some(Permission::Read), "m".into());
    // Mehrrundiger Tool-Usage: 3 Assistant-Sub-Runden im selben Turn.
    for i in 0..3 {
        let aid = s.open_assistant(String::new(), String::new());
        if i == 0 {
            s.open_tool(
                Some(aid),
                format!("c{i}"),
                "read".into(),
                r#"{"path":"a.c"}"#.into(),
                ToolKind::Read {
                    path: "a.c".into(),
                    range: String::new(),
                },
            );
        }
        s.chat
            .finalize_assistant(aid, t0, Usage::default(), 0, 0, false);
    }
    // Neuer Turn (Assistent übergibt wieder an den User).
    s.push_user_message("frage2".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), "Antwort zwei".into());
    s.chat
        .finalize_assistant(aid, t0, Usage::default(), 0, 0, false);

    let (blocks, _ctx) = build_history_cache(&s, 200, "m", 8192);
    let footers: Vec<String> = blocks
        .iter()
        .filter(|b| {
            // `wrap_block` zerlegt die Fußzeile in einzelne Spans ("—", " ", "m", …);
            // das Em-Dash ist eindeutig ein Fußzeilen-Marker.
            b.lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|sp| sp.content.contains('—'))
        })
        .map(|b| {
            b.lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();
    assert_eq!(
        footers.len(),
        3,
        "genau eine Fußzeile je Antwort ohne Tool-Aufrufe – nicht je Sub-Runde. Gefunden: {footers:#?}"
    );
}

#[test]
fn tool_kind_detail_baut_kompakte_kurzform_aus_strukturierten_feldern() {
    use crate::chat::ToolKind;
    // read mit Fenster → Pfad + Range, ohne Fenster → nur Pfad.
    assert_eq!(
        tool_kind_detail(&ToolKind::Read {
            path: "gemma4.c".into(),
            range: "200-460".into(),
        }),
        "← read gemma4.c 200-460"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Read {
            path: "Cargo.toml".into(),
            range: String::new(),
        }),
        "← read Cargo.toml"
    );
    // grep/glob → Muster (grep zusätzlich Pfad + Trefferzahl, wenn gesetzt).
    assert_eq!(
        tool_kind_detail(&ToolKind::Grep {
            pattern: "fn main".into(),
            path: String::new(),
            include: String::new(),
            num_results: 0,
            num_results_reported: 0,
        }),
        "⌕ grep \"fn main\""
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 5,
            num_results_reported: 5,
        }),
        "⌕ grep \"TODO\" src - 5 results"
    );
    // Genau ein Treffer → Singular „result“.
    assert_eq!(
        tool_kind_detail(&ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 1,
            num_results_reported: 1,
        }),
        "⌕ grep \"TODO\" src - 1 result"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Glob {
            pattern: "src/**/*.rs".into(),
            num_results: 0,
            num_results_reported: 0,
        }),
        "☰ glob \"src/**/*.rs\""
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Glob {
            pattern: "*.rs".into(),
            num_results: 1,
            num_results_reported: 1,
        }),
        "☰ glob \"*.rs\" - 1 result"
    );
    // Decke aktiv: berichtet weniger als gefunden → „R of N results“.
    assert_eq!(
        tool_kind_detail(&ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 486,
            num_results_reported: 200,
        }),
        "⌕ grep \"TODO\" src - 200 of 486 results"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Glob {
            pattern: "mod.rs".into(),
            num_results: 486,
            num_results_reported: 200,
        }),
        "☰ glob \"mod.rs\" - 200 of 486 results"
    );
    // webfetch → Host · Prompt, write/edit → Pfad, run → Kommando.
    assert_eq!(
        tool_kind_detail(&ToolKind::Webfetch {
            url: "https://example.com".into(),
            prompt: "x".into(),
        }),
        "↧ webfetch example.com · x"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Write {
            path: "src/main.rs".into(),
        }),
        "→ write src/main.rs"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Run {
            cwd: String::new(),
            command: "cargo test".into(),
            exit_code: None,
        }),
        "⚙ bash cargo test"
    );
}

/// Fortgesetzte Zeilen einer umgebrochenen Diff-Zelle müssen exakt `cell_w`
/// Zellen breit sein – inkl. des Vorspanns (`num_w` Leerraum + `│` + Marker).
/// Vorher war das Umbruchlimit für Folgezeilen `cell_w` statt `cell_w - (num_w+3)`,
/// wodurch umgebrochene Zeilen um `num_w + 3` Zellen überliefen.

#[test]
fn diff_cell_folgezeilen_bleiben_unter_zellbreite() {
    use crate::ui::blocks::{diff_cell, disp_width, CellStyle};
    use ratatui::style::Style;

    let style = CellStyle {
        marker: ' ',
        base: Style::default(),
        mark: Style::default(),
        num_style: Style::default(),
    };
    let num_w = 2;
    let cell_w = 20;
    // Zum Zellgrenzen: erster Umbruch bei ≥ first_w; ein langer Text zwingt
    // mindestens eine weitere Zeile auf.
    let text = "ui".repeat(50); // 100 Zeichen → garantiert mehrere Zeilen
    let rows = diff_cell(Some(7), &text, &[], num_w, cell_w, &style);

    assert!(
        rows.len() >= 3,
        "langer Text bricht mehrfach um, bekam {}",
        rows.len()
    );
    for (i, row) in rows.iter().enumerate() {
        let w = row.iter().map(|s| disp_width(&s.content)).sum::<usize>();
        assert!(
            w <= cell_w,
            "Zeile {i} überschreitet die Zellbreite: {w} > {cell_w}"
        );
        // Jede Folgezeile trägt außerdem den `│`-Trenner.
        let joined: String = row.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains('│'), "Zeile {i} bringt den `│`-Trenner");
        if i > 0 {
            let before_pipe = &joined[..joined.find('│').unwrap()];
            assert_eq!(
                disp_width(before_pipe),
                num_w,
                "Folgezeile {}: leerer Nummern-Gutter exakt {num_w} breit",
                i
            );
        }
    }
}

/// Eine leere Zelle (Ausgleich, wenn die Gegenseite mehr Umbruchzeilen hat)
/// muss dieselbe Gutter-Struktur wie eine Folgezeile tragen: `num_w` Leerraum +
/// `│` + Marker – also den `│`-Trenner auch dort, wo keine Zeilennummer und kein
/// Inhalt steht.

#[test]
fn empty_cell_zeigt_trenner_und_gutter_struktur() {
    use crate::ui::blocks::{disp_width, empty_cell, CellStyle};
    use ratatui::style::Style;

    let style = CellStyle {
        marker: '+',
        base: Style::default(),
        mark: Style::default(),
        num_style: Style::default(),
    };
    let cell_w = 20;
    let num_w = 2;
    let spans = empty_cell(cell_w, num_w, &style);
    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();

    assert!(joined.contains('│'), "leere Zelle trägt den `│`-Trenner");
    let total = spans.iter().map(|s| disp_width(&s.content)).sum::<usize>();
    assert_eq!(total, cell_w, "leere Zelle bleibt exakt `cell_w` breit");
    assert!(
        joined.starts_with("  │+"),
        "Gutter: `num_w` Leerraum + `│` + Marker; bekam {joined:?}"
    );
}

// ── Gedanken-Rendering (Zoom 2/3 kompakt vs. Detail) ───────────────────────

/// Zoom 2+3 (Compact/Dialog): das Gedanken-Element ist eine EINZIGE Zeile mit
/// dem Icon `U+1F5ED` in Spalte 2 und dem tatsächlichen Reasoning-Inhalt ab
/// Spalte 4; Zeilenumbrüche werden zu Leerzeichen (wie Overview-Texte).

#[test]
fn gedanken_kompakt_ikon_spalte2_inhalt_ab_spalte4() {
    let blk = thoughts_block("Zeile eins\nZeile zwei", false, 40);
    assert_eq!(
        blk.lines.len(),
        1,
        "Zoom 2/3: genau eine komprimierte Zeile"
    );
    let line = &blk.lines[0];
    let s = line.to_string();
    // 2 Leerzeichen → Icon bei Spalte 2; danach eine Zelle Abstand → Text ab
    // Spalte 4 (THOUGHT_INDENT).
    assert!(
        s.starts_with("  \u{1F5ED} "),
        "Icon in Spalte 2, Text dahinter: {s:?}"
    );
    // Zeilenumbruch → Leerzeichen, echter Inhalt sichtbar.
    assert!(
        s.contains("Zeile eins Zeile zwei"),
        "Inhalt wird komprimiert: {s:?}"
    );
    // Gedanken-Look: gedämpft, aber NICHT kursiv (Zoom 2+3 = normal).
    assert_eq!(line.style.fg, Some(theme().muted));
    assert!(
        !line.style.add_modifier.contains(Modifier::ITALIC),
        "kompakte Gedankenzeile ist nicht kursiv"
    );
}

/// Zoom 2+3: läuft der Inhalt über den verfügbaren Platz, wird er mit „…“
/// abgeschnitten (analog zur Overview) statt umgebrochen.

#[test]
fn gedanken_kompakt_kuerzt_ueberlauf_mit_auslassung() {
    // budget = width(10) − THOUGHT_INDENT(4) − PAD_R(2) = 4 Zellen
    // (davon 1 für „…“) → „ABC…“.
    let blk = thoughts_block("ABCDEFGHIJ", false, 10);
    assert_eq!(blk.lines.len(), 1, "bleibt eine Zeile, kein Umbruch");
    let s = blk.lines[0].to_string();
    assert!(s.ends_with("ABC…"), "Überlauf gekürzt + „…“: {s:?}");
}

/// Detail (Zoom 4): der ganze Text bleibt mehrzeilig ab Spalte 4 eingerückt;
/// nur in der ersten Zeile steht zusätzlich das Icon in Spalte 2.

#[test]
fn gedanken_detail_voller_text_mit_ikon_nur_in_erster_zeile() {
    let reason = "Ein sehr langer Gedankentext, der über die verfügbare Breite \
                  hinaus läuft und daher in der Detailansicht umbrochen werden muss.";
    let blk = thoughts_block(reason, true, 40);
    let repr: Vec<String> = blk.lines.iter().map(|l| l.to_string()).collect();
    assert!(
        repr.len() >= 2,
        "langer Text bricht mehrzeilig um: {repr:?}"
    );
    // Erste Zeile: Icon in Spalte 2, Text beginnt weiterhin in Spalte 4.
    assert!(
        repr[0].starts_with("  \u{1F5ED} Ein sehr langer"),
        "erste Zeile: Icon Spalte 2 + Text Spalte 4: {repr:?}"
    );
    // Folgezeile: ohne Icon, weiterhin ab Spalte 4 eingerückt.
    assert!(
        repr[1].starts_with("    "),
        "Folgezeile ab Spalte 4 (ohne Icon): {repr:?}"
    );
    assert!(
        !repr[1].contains('\u{1F5ED}'),
        "Icon steht nur in der ersten Zeile: {repr:?}"
    );
    // Ganzer Inhalt bleibt erhalten (keine Kürzung im Detaillevel).
    assert!(
        repr.iter().any(|l| l.contains("Detailansicht")),
        "voller Text bleibt: {repr:?}"
    );
}

// ── grep/glob Detail-Spaltenansicht ────────────────────────────────────────

/// Detail-Modus für grep: Vorkommen werden in kompakter Spaltenansicht
/// statt nur als "N results" angezeigt.

#[test]
fn grep_detail_zeigt_vorkommen_in_spalten() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    s.view = ViewLevel::Detailed;
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), String::new());
    let t = s.open_tool(
        Some(aid),
        "c1".into(),
        "grep".into(),
        r#"{"pattern":"TODO","path":"src","content":0}"#.into(),
        ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 3,
            num_results_reported: 3,
        },
    );
    s.chat.set_tool_final(
        t,
        "src/main.rs:1: // TODO: fix\nsrc/lib.rs:5: TODO: handle\nsrc/config.rs:10: // TODO\n"
            .into(),
        ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 3,
            num_results_reported: 3,
        },
        0,
        0,
        std::time::Instant::now(),
    );
    s.finish_assistant(false, None);

    let (blocks, _) = build_history_cache(&s, 120, "m", 8192);
    let text: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    // Label-Zeile ist vorhanden.
    assert!(
        text.iter()
            .any(|l| l.contains("grep") && l.contains("TODO")),
        "Label mit Tool-Name und Query: {text:?}"
    );
    // Alle drei Treffer sind sichtbar.
    assert!(
        text.iter().any(|l| l.contains("main.rs:1")),
        "Treffer 1: {text:?}"
    );
    assert!(
        text.iter().any(|l| l.contains("lib.rs:5")),
        "Treffer 2: {text:?}"
    );
    assert!(
        text.iter().any(|l| l.contains("config.rs:10")),
        "Treffer 3: {text:?}"
    );
}

/// Detail-Modus für glob: Dateipfade werden in Spalten angezeigt.

#[test]
fn glob_detail_zeigt_dateien_in_spalten() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    s.view = ViewLevel::Detailed;
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), String::new());
    let t = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        r#"{"pattern":"src/**/*.rs"}"#.into(),
        ToolKind::Glob {
            pattern: "src/**/*.rs".into(),
            num_results: 4,
            num_results_reported: 4,
        },
    );
    s.chat.set_tool_final(
        t,
        "src/main.rs\nsrc/lib.rs\nsrc/config.rs\nsrc/app.rs\n".into(),
        ToolKind::Glob {
            pattern: "src/**/*.rs".into(),
            num_results: 4,
            num_results_reported: 4,
        },
        0,
        0,
        std::time::Instant::now(),
    );
    s.finish_assistant(false, None);

    let (blocks, _) = build_history_cache(&s, 120, "m", 8192);
    let text: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    // Alle vier Pfade sind sichtbar.
    for path in &["src/main.rs", "src/lib.rs", "src/config.rs", "src/app.rs"] {
        assert!(
            text.iter().any(|l| l.contains(path)),
            "Pfad {path} muss sichtbar sein: {text:?}"
        );
    }
}

/// Ohne Detail-Modus (Compact/Dialog) bleibt grep/glob ein Einzeiler.

#[test]
fn grep_compact_bleibt_einzeiler() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    // ViewLevel::Compact (Default)
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), String::new());
    let t = s.open_tool(
        Some(aid),
        "c1".into(),
        "grep".into(),
        r#"{"pattern":"TODO","path":"src"}"#.into(),
        ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 3,
            num_results_reported: 3,
        },
    );
    s.chat.set_tool_final(
        t,
        "src/main.rs:1: // TODO\nsrc/lib.rs:5: TODO\nsrc/config.rs:10: // TODO\n".into(),
        ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 3,
            num_results_reported: 3,
        },
        0,
        0,
        std::time::Instant::now(),
    );
    s.finish_assistant(false, None);

    let (blocks, _) = build_history_cache(&s, 120, "m", 8192);
    let text: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    // Im Compact-Modus: Keine individuellen Trefferzeilen, nur das Label.
    assert!(
        !text.iter().any(|l| l.contains("main.rs:1")),
        "Compact: keine individuellen Treffer: {text:?}"
    );
    assert!(
        text.iter()
            .any(|l| l.contains("grep") && l.contains("TODO")),
        "Compact: Label bleibt: {text:?}"
    );
}

/// Detail-Ansicht mit vielen Treffern: Spaltenanzahl passt sich dem Platz an.

#[test]
fn grep_detail_spaltenanzahl_passt_sich_an() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    s.view = ViewLevel::Detailed;
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), String::new());
    // 20 Dateinamen, bei 200 Zellen Breite → viele Spalten möglich.
    let output: String = (0..20)
        .map(|i| format!("file_{i:02}.txt"))
        .collect::<Vec<_>>()
        .join("\n");
    let t = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        r#"{"pattern":"*.txt"}"#.into(),
        ToolKind::Glob {
            pattern: "*.txt".into(),
            num_results: 20,
            num_results_reported: 20,
        },
    );
    s.chat.set_tool_final(
        t,
        output,
        ToolKind::Glob {
            pattern: "*.txt".into(),
            num_results: 20,
            num_results_reported: 20,
        },
        0,
        0,
        std::time::Instant::now(),
    );
    s.finish_assistant(false, None);

    let (blocks, _) = build_history_cache(&s, 200, "m", 8192);
    let text: Vec<String> = blocks
        .iter()
        .flat_map(|b| b.lines.iter().map(|l| l.to_string()))
        .collect();
    // Alle 20 Pfade sichtbar.
    for i in 0..20 {
        let name = format!("file_{i:02}.txt");
        assert!(
            text.iter().any(|l| l.contains(&name)),
            "Datei {name} muss sichtbar sein: {text:?}"
        );
    }
    // Mehrere Dateien pro Zeile (Spalten) → weniger Zeilen als Dateien.
    // (Label + mindestens 1 Ergebniszeile, aber nicht 20 Einzelzeilen)
    assert!(
        text.len() < 22,
        "Mehrere Spalten → nicht jede Datei in eigener Zeile: {} Zeilen",
        text.len()
    );
}

#[test]
fn preserve_breaks_haelt_einfache_zeilenumbrueche() {
    // Kernfall aus der Fehlermeldung: mehrzeilige Eingabe soll im Chatdialog
    // ihre Zeilen behalten statt zu einer Zeile zu verschmelzen.
    let src = super::markdown::preserve_breaks("Zeile eins\nZeile zwei");
    let l = super::markdown::logical_lines(&src);
    let lines: Vec<String> = l.iter().map(|x| x.to_string()).collect();
    assert_eq!(lines, ["Zeile eins", "Zeile zwei"]);
}

#[test]
fn preserve_breaks_behaelt_absatzgrenzen_und_markdown() {
    let show = |s: &str| -> Vec<String> {
        super::markdown::logical_lines(&super::markdown::preserve_breaks(s))
            .into_iter()
            .map(|x| x.to_string())
            .collect()
    };
    // Inline-Formatierung bleibt erhalten, Zeilen bleiben trotzdem getrennt.
    assert_eq!(
        show("mit **bold**\nund code `x`\ndann weiter"),
        ["mit bold", "und code x", "dann weiter"]
    );
    // Doppelte Umbrüche (Absätze) bleiben Absätze.
    assert_eq!(show("Absatz 1\n\nAbsatz 2"), ["Absatz 1", "", "Absatz 2"]);
    // Listen bleiben Listen.
    assert_eq!(
        show("- item eins\n- item zwei"),
        ["- item eins", "- item zwei"]
    );
    // Zitate bleiben Zitate.
    assert_eq!(show("> q eins\n> q zwei"), ["> q eins", "> q zwei"]);
    // Bestehender harter Umbruch (zwei Spaces) bleibt unverändert.
    assert_eq!(
        show("Zeile eins  \nZeile zwei"),
        ["Zeile eins", "Zeile zwei"]
    );
}

#[test]
fn preserve_breaks_tabellen_und_code_bleiben_unangetastet() {
    let show = |s: &str| -> Vec<String> {
        super::markdown::logical_lines(&super::markdown::preserve_breaks(s))
            .into_iter()
            .map(|x| x.to_string())
            .collect()
    };
    // GFM-Tabelle (auch mit nackter Begrenzungszeile `---|---`) bleibt Tabelle.
    let t = show("| a | b |\n---|---\n| 1 | 2 |");
    assert_eq!(
        t,
        [
            "┌───┬───┐",
            "│ a │ b │",
            "├───┼───┤",
            "│ 1 │ 2 │",
            "└───┴───┘"
        ]
    );
    // Fenced-Codeblock bleibt zeichengetreu (keine eingefügten Spaces).
    let src = "```rs\nlet a = 1;\nlet b = 2;\n```";
    assert_eq!(show(src), ["```rs", "let a = 1;", "let b = 2;", "```"]);
}

#[test]
fn preserve_breaks_blockzitate_bleiben_zeilengetreu() {
    let show = |s: &str| -> Vec<String> {
        super::markdown::logical_lines(&super::markdown::preserve_breaks(s))
            .into_iter()
            .map(|x| x.to_string())
            .collect()
    };
    // Aufeinanderfolgende `>`-Zeilen sind Markdown-EIN Absatz und würden sonst
    // verschmelzen – preserve_breaks hält sie getrennt, wie im Eingabefeld.
    assert_eq!(show("> q eins\n> q zwei"), ["> q eins", "> q zwei"]);
    assert_eq!(
        show("> eins\n> zwei\nnormal text"),
        ["> eins", "> zwei", "> normal text"]
    );
}

// ── Channel-Builder-Overlay: Breite ─────────────────────────────────────────

/// 90 % der Fensterbreite – bzw. die volle Fensterbreite, wenn 90 % darunter
/// unter `BUILDER_MIN_WIDTH` (80 Spalten) fielen.

#[test]
fn builder_dialog_width_nimmt_90_prozent_oder_die_volle_breite() {
    // Breite Fenster: 90 %.
    assert_eq!(builder_dialog_width(200), 180);
    assert_eq!(builder_dialog_width(120), 108);
    assert_eq!(builder_dialog_width(100), 90);
    assert_eq!(builder_dialog_width(89), 80); // genau 80 ist nicht "unter 80"
                                              // Schmale Fenster: 90 % wären < 80 → volle Fensterbreite.
    assert_eq!(builder_dialog_width(88), 88); // vorher: 79
    assert_eq!(builder_dialog_width(80), 80); // vorher: 72
    assert_eq!(builder_dialog_width(40), 40); // vorher: 36
                                              // Der Dialog bleibt in jedem Fall innerhalb des Fensters.
    for w in (1..=200).step_by(7) {
        assert!(builder_dialog_width(w) <= w, "Breite {w} passt ins Fenster");
    }
}

/// Smoke-Test für die schmalen Fenster, in denen jetzt die volle Breite genutzt
/// wird: Das Rendern (inkl. Spalten-Layout, umbrechender Header, Inline-Felder)
/// läuft ohne Panik durch.

#[test]
fn builder_rendert_auch_auf_schmalem_fenster() {
    for (w, h) in [(40u16, 20u16), (80, 24), (24, 18)] {
        let mut app = builder_app();
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).expect("TestBackend");
        term.draw(|f| draw_channel_builder(f, &mut app))
            .expect("render");
    }
}

/// Minimaler App-Zustand mit geöffnetem Channel Builder: zwei Tunnel,
/// ein längerer Host-Pfad, ein Branch.
fn builder_app() -> App {
    use crate::app::{ChannelBuilderState, Selection};
    use crate::channel::builder::{HostPath, Tunnel, WorktreeEntry};
    use crate::channel::ChannelRegistry;
    use crate::config::Config;
    use std::path::PathBuf;

    let cfg = Config {
        model: "test/m".into(),
        mouse: false,
        ..Config::default()
    };
    let channels = ChannelRegistry::new_with_warnings(&cfg).0;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = App::new(cfg, channels, tx, rx);
    app.channel_builder = Some(ChannelBuilderState {
        tunnels: Selection::wrap_at(
            vec![
                Tunnel::Local,
                Tunnel::Image {
                    name: "rust-2024-builder".into(),
                    working_dir: None,
                },
            ],
            0,
        ),
        host_paths: Selection::wrap_at(
            vec![HostPath {
                path: PathBuf::from("/home/work/git/ein-laengerer-projektname"),
            }],
            0,
        ),
        worktrees: Selection::wrap_at(
            vec![WorktreeEntry {
                label: "main (worktree)".into(),
                path: PathBuf::from("/home/work/git/ein-laengerer-projektname"),
                branch: "main".into(),
                is_main: true,
                has_worktree: true,
            }],
            0,
        ),
        col: 1,
        container_info: None,
        images_loaded: true,
        current_is_git: true,
        argv_path: None,
        edit: None,
        edit_error: None,
    });
    app
}

// ── Channel Builder: Zeilenumbruch (Marker bleibt am Inhalt) ───────────────

/// `wrap_builder_cells`: bricht an Leerzeichen (das entfällt), nach
/// Trennerzeichen (bleiben stehen) bzw. hart an der Spaltengrenze um – wie die
/// Eingabe (`editor::wrap_ranges`), nur zellengenau.

#[test]
fn wrap_builder_cells_bricht_an_leerzeichen_und_trenner() {
    let lines = |s: &str, w: usize| {
        let chars: Vec<char> = s.chars().collect();
        wrap_builder_cells(&chars, w)
            .into_iter()
            .map(|r| chars[r.clone()].iter().collect::<String>())
            .collect::<Vec<_>>()
    };
    // Leerzeichen entfällt, Rest beginnt in der nächsten Zeile (Wortgrenze).
    assert_eq!(lines("main task", 8), ["main", "task"]);
    // Trenner bleiben am Zeilenende stehen, dahinter wird getrennt.
    assert_eq!(lines("ho/me/a", 3), ["ho/", "me/", "a"]);
    // Führendes Leerzeichen entfällt beim Umbruch (keine leere Zeile); der
    // Text füllt die Zeile trotzdem bis zur Spaltengrenze.
    assert_eq!(lines(" Tunnel", 5), ["Tunne", "l"]);
    // Lange Wörter: hart an der Spaltengrenze.
    assert_eq!(lines("abcdefgh", 3), ["abc", "def", "gh"]);
}

/// `split_column_entry`: Der Marker steht immer mit dem Inhalt in einer Zeile –
/// nie allein. Über alle Spaltenbreiten hinweg keine Zeile, die nur den Marker
/// zeigt; die erste Zeile beginnt mit dem Inhalt (mit oder ohne Marker).

#[test]
fn split_column_entry_haelt_marker_und_inhalt_zusammen() {
    for col_w in 1..=40u16 {
        let lines = split_column_entry(
            "▶ ",
            "/home/work/git/ein-laengerer-name",
            Style::default(),
            col_w,
        );
        assert!(
            !lines.is_empty(),
            "col_w {col_w}: Eintrag darf nie leer sein"
        );
        for l in &lines {
            let text = l.to_string();
            // Niemals eine Zeile, die nur den Marker enthält.
            assert_ne!(text.trim(), "▶", "col_w {col_w}: '{text}' = nur Marker");
            // Zeilen überschreiten die Spaltenbreite nicht.
            let cells = text
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(1))
                .sum::<usize>();
            assert!(
                cells <= col_w as usize,
                "col_w {col_w}: '{text}' = {cells} Zellen"
            );
        }
        // Sieht die erste Zeile den Marker, dann auch den Inhalt dazu; ist die
        // Spalte zu schmal für den Marker, beginnt die erste Zeile mit Inhalt.
        let first = lines[0].to_string();
        assert!(
            first.starts_with("▶ ") && first.trim().len() > 2 || !first.starts_with("▶ "),
            "col_w {col_w}: '{first}'"
        );
    }
}

/// Wohlgeformter Fall: Kurzer Text neben dem Marker, Rest an der Wortgrenze
/// in der Folgezeile – wie vorher vom `Paragraph`-Umbruch („● main“ /
/// „(worktree)“); die Folgezeile braucht dafür genug Breite (10 Zellen).

#[test]
fn split_column_entry_behaelt_komfortablen_umbruch() {
    let lines = split_column_entry("● ", "main (worktree)", Style::default(), 10);
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert_eq!(text, ["● main", "(worktree)"]);
}

/// End-to-End über die gerenderte Fläche: Auf schmalen Terminals darf keine
/// Zeile existieren, die nur einen Marker (`▶`, `●`, `○`) enthält.

#[test]
fn builder_rendert_keinen_marker_ohne_inhalt() {
    for (w, h) in [(24u16, 18u16), (30, 20), (40, 20), (60, 24)] {
        let mut app = builder_app();
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).expect("TestBackend");
        term.draw(|f| draw_channel_builder(f, &mut app))
            .expect("render");
        let buf = term.backend().buffer().clone();
        for y in 0..h {
            let row: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
            let t = row.trim();
            assert!(
                t != "▶" && t != "●" && t != "○",
                "{w}x{h} Zeile {y} zeigt nur den Marker: {row:?}"
            );
        }
    }
}

#[test]
fn tabelle_verliert_beim_umbruch_keine_zeichen() {
    use crate::ui::markdown::{logical_lines, wrap_markdown};
    // Drei breite Spalten: die Tabelle passt nicht in die Breite, also werden
    // mehrere Spalten gleichzeitig verkleinert und ihre Zellen mehrzeilig
    // umgebrochen.
    let src = "\
| Sp1 | Sp2 | Sp3 |
|---|---|---|
| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb | cccccccccccccccccccc |
| x | y | z |";
    let logical = logical_lines(src);
    let width = 40usize;
    let out = wrap_markdown(&logical, width, PAD);

    // Keine physische Zeile darf die verfügbare Textbreite überschreiten.
    for l in &out {
        assert!(
            l.width() <= width - PAD_R,
            "Zeile zu breit ({} > {}): {:?}",
            l.width(),
            width - PAD_R,
            l.to_string()
        );
    }
    // Beim Zellumbruch darf kein Zeichen verloren gehen: die Kleinbuchstaben
    // a/b/c kommen ausschließlich im jeweiligen Zellinhalt vor.
    let text: String = out.iter().map(|l| l.to_string()).collect();
    for (c, n) in [('a', 20), ('b', 20), ('c', 20)] {
        assert_eq!(text.matches(c).count(), n, "Zeichen '{c}' verloren: {text}");
    }
}
