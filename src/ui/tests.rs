//! UI-Tests: nur ein schlanker Smoke-Test der neuen Event-basierten Render-
//! Pipeline (`build_history_cache`, `build_live_blocks`, Layout). Die UI wird
//! bewusst nicht mehr eingehend getestet – sie ändert sich noch (siehe Plan).

use super::*;
use crate::app::Session;
use crate::chat::EventKind;
use crate::config::SymbolMode;
use crate::llm::Usage;
use crate::perm::Permission;

fn usage_zero() -> Usage {
    Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: None,
    }
}

/// Session mit einem abgeschlossenen User/Assistant-Turn.
fn session_with_history() -> Session {
    let mut s = Session::new(0);
    s.push_user_message("Hallo Modell".into(), Some(Permission::Read), "model-x".into());
    let aid = s.open_assistant("Gedanken".into(), "Hallo!".into());
    s.chat
        .finalize_assistant(aid, std::time::Instant::now(), usage_zero(), 1, 1, false);
    s
}

#[test]
fn history_cache_baut_bloecke_fuer_user_und_assistant() {
    let mut s = session_with_history();
    s.view = crate::app::ViewLevel::Overview;
    let (blocks, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "model-x", 8192);

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
    let (live, _live_used) = build_live_blocks(&s, 100, SymbolMode::Glyph, 8192, &ctx);
    assert!(live.is_empty(), "keine offenen Events → kein Live-Tail");
}

#[test]
fn live_blocks_zeigen_offene_assistant_rune() {
    let mut s = Session::new(0);
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    s.ensure_open_assistant();
    s.append_text("laufende Antwort");

    let (_, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let (live, live_used) = build_live_blocks(&s, 100, SymbolMode::Glyph, 8192, &ctx);
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
fn beendete_tools_bleiben_unter_offener_runde_sichtbar() {
    use crate::chat::ToolKind;
    let mut s = Session::new(0);
    s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), String::new());
    // Mittel-Zustand einer Mehr-Tool-Runde (verzögerte Runden-Schließung):
    // Tool 1 ist schon beendet (`ToolEnd` verarbeitet), Tool 2 läuft noch –
    // die Assistant-Runde selbst ist aber noch OFFEN.
    let t1 = s.open_tool(
        Some(aid),
        "c1".into(),
        "glob".into(),
        r#"{"pattern":"*"}"#.into(),
        ToolKind::Glob { pattern: "*".into(), num_results: 2 },
    );
    s.chat.finish_tool(t1, std::time::Instant::now());
    s.open_tool_ids.retain(|&t| t != t1); // wie `finalize_tool_event`
    s.open_tool(
        Some(aid),
        "c2".into(),
        "read".into(),
        r#"{"path":"a.rs"}"#.into(),
        ToolKind::Read { path: "a.rs".into(), range: String::new() },
    );

    let (hist, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let (live, _live_used) = build_live_blocks(&s, 100, SymbolMode::Glyph, 8192, &ctx);
    let hist_text: Vec<String> = hist.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string())).collect();
    let live_text: Vec<String> = live.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string())).collect();
    // Das beendete Tool 1 gehört zur noch OFFENEN Runde → noch NICHT in der
    // Historie, aber der Live-Tail muss es weiterhin zeigen (statisch), sonst
    // verschwände es zwischen `ToolEnd` und Runden-Schluss aus der UI.
    assert!(!hist_text.iter().any(|l| l.contains("glob")), "Runde offen → Tool 1 noch nicht in Historie: {hist_text:?}");
    assert!(live_text.iter().any(|l| l.contains("glob")), "beendetes Tool bleibt unter offener Runde sichtbar: {live_text:?}");
    assert!(live_text.iter().any(|l| l.contains("read") || l.contains("a.rs")), "laufendes Tool sichtbar: {live_text:?}");

    // Sobald die Runde geschlossen wird (nächstes Chunk/Reasoning/Usage/Done),
    // wandert das Paar (Runde + Tool-Kinder) in die Historie; der Live-Tail
    // enthält danach nichts mehr davon.
    s.finish_assistant(false, None);
    let (hist2, ctx2) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let (live2, _live_used) = build_live_blocks(&s, 100, SymbolMode::Glyph, 8192, &ctx2);
    let hist2_text: Vec<String> = hist2.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string())).collect();
    assert!(hist2_text.iter().any(|l| l.contains("glob")), "geschlossen → Tool 1 in Historie: {hist2_text:?}");
    assert!(live2.is_empty(), "kein offenes Event mehr → Live-Tail leer");
}

#[test]
fn assistant_sync_ctx_beruecksichtigt_tool_call_anteil() {
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
    let assistant_ev = |s: &Session| {
        s.chat
            .iter()
            .find(|e| matches!(e.kind, EventKind::Assistant { .. }))
            .expect("assistant")
            .clone()
    };

    // 1) Abschlussantwort ohne Tools → `total_tokens`.
    let mut s = Session::new(0);
    s.push_user_message("f1".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), "Antwort".into());
    s.chat.finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
    assert_eq!(assistant_sync_ctx(&assistant_ev(&s), &s), Some(1098));

    // 2) Tool-Runde mit gemessenen completion_parts → prompt + reasoning + content.
    let mut s = Session::new(0);
    s.push_user_message("f2".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), String::new());
    let t1 = s.open_tool(
        Some(aid), "c1".into(), "glob".into(), "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob { pattern: "*".into(), num_results: 2 },
    );
    let t2 = s.open_tool(
        Some(aid), "c2".into(), "glob".into(), "{\"pattern\":\"**/*\"}".into(),
        ToolKind::Glob { pattern: "**/*".into(), num_results: 2 },
    );
    s.chat.finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
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
    assert_eq!(assistant_sync_ctx(&assistant_ev(&s), &s), Some(987 + 38 + 0), "parts: prompt+reasoning+content");

    // 3) Tool-Runde OHNE parts → `total_tokens` − ∑ tool num_tokens_input.
    let mut s = Session::new(0);
    s.push_user_message("f3".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), String::new());
    let t1 = s.open_tool(
        Some(aid), "c1".into(), "glob".into(), "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob { pattern: "*".into(), num_results: 2 },
    );
    let t2 = s.open_tool(
        Some(aid), "c2".into(), "glob".into(), "{\"pattern\":\"**/*\"}".into(),
        ToolKind::Glob { pattern: "**/*".into(), num_results: 2 },
    );
    s.chat.finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);
    s.chat.set_tool_tokens(t1, 36, 100);
    s.chat.set_tool_tokens(t2, 37, 200);
    assert_eq!(assistant_sync_ctx(&assistant_ev(&s), &s), Some(1098 - 73), "ohne parts: total − tool-calls");

    // 4) Ohne bestätigte total_tokens → None.
    let mut s = Session::new(0);
    s.push_user_message("f4".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant("".into(), "x".into());
    s.chat.finalize_assistant(aid, std::time::Instant::now(), zero, 0, 0, false);
    assert_eq!(assistant_sync_ctx(&assistant_ev(&s), &s), None);
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
    s.chat.finalize_assistant(aid, std::time::Instant::now(), usage, 0, 0, false);

    let (blocks, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let texts: Vec<String> = blocks.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string())).collect();
    // In der Übersicht wird die Gedanken-Runde als Zeile übersprungen …
    assert!(!texts.iter().any(|l| l.contains("Gedanken")), "Reasoning ohne Text erscheint nicht als Zeile: {texts:?}");
    // … aber `used` springt trotzdem auf die serverbestätigte Zahl (resync_to
    // läuft für jede geschlossene Runde, nicht nur für Text-Runden).
    assert_eq!(ctx.used, 500, "verified tokens fließen trotz leerem Text in die Schätzung");
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
    s.chat.finalize_assistant(aid, std::time::Instant::now(), usage_zero(), 0, 0, false);

    let (_, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let est_user = crate::llm::estimate_tokens("kurz");
    let est_reasoning = crate::llm::estimate_tokens("nur Gedanken");
    assert_eq!(
        ctx.used,
        est_user + est_reasoning,
        "Reasoning-Schätzung bleibt ohne Usage in `used`"
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
        Some(a1), "c1".into(), "glob".into(), "{\"pattern\":\"*\"}".into(),
        ToolKind::Glob { pattern: "*".into(), num_results: 2 },
    );
    s.chat.finish_tool(t1, t0);
    let a2 = s.open_assistant("fertig".into(), "Antwort".into());
    s.chat.finalize_assistant(a2, t0, usage(910), 0, 0, false);

    let user_id = s
        .chat
        .order()
        .iter()
        .find(|id| matches!(s.chat.event(**id).map(|e| &e.kind), Some(EventKind::UserPrompt { .. })))
        .copied()
        .expect("user");

    // Helper-Map: A1 (content-los) verifiziert den User, A2 (Content) das Tool.
    let m = assistant_verified_before(&s);
    assert_eq!(m.get(&user_id).copied(), Some(420), "content-lose Runde verifiziert Vorgänger");
    assert_eq!(m.get(&t1).copied(), Some(910), "content-behaftete Runde verifiziert Vorgänger");

    // Sichtbar: grüne Zahlen auf der User-Zeile (420T) und der Tool-Zeile (910T);
    // die content-lose Runde selbst rendert KEINE Zeile.
    let (blocks, _ctx) = build_history_cache(&s, 120, SymbolMode::Glyph, "m", 8192);
    let has_green = |blk: &ChatBlock, needle: &str| {
        blk.lines.iter().any(|l| {
            l.spans.iter().any(|sp| {
                sp.style.fg == Some(SYM_OK)
                    && sp.content.to_string().contains(needle)
            })
        })
    };
    let texts: Vec<String> = blocks.iter().flat_map(|b| b.lines.iter().map(|l| l.to_string())).collect();
    assert!(!texts.iter().any(|l| l.contains("Gedanken")), "Content-lose Runde bleibt in der Übersicht unsichtbar: {texts:?}");
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
fn tool_verifikation_resynct_used_absolut_ohne_doppelzaehlung() {
    // Reihenfolge der Tool-Zeile: `overview_tool_line_chat` addiert erst die
    // Tool-Schätzung (add_tool), DANN springt der verifizierte `prompt_tokens`-
    // Wert (`resync_after`, enthält das Tool-Ergebnis bereits) auf den absoluten
    // Stand zurück – die Tool-Schätzung wird NICHT doppelt gezählt.
    let mut ctx = ContextEstimate::base();
    ctx.add_tool("glob", 136);
    assert_eq!(ctx.used, 136);
    ctx.resync_to(910);
    assert_eq!(ctx.used, 910, "verifizierter Wert ersetzt die Schätzung absolut");

    // `resync_after` kapselt setzen→rendern→re-anchor: nimmt den grünen Wert
    // vom `verified_prompt`-Feld, resynct darauf und räumt das Feld wieder ab.
    let mut c2 = ContextEstimate::base();
    c2.add_tool("read", 200);
    c2.verified_prompt = Some(1200);
    resync_after(&mut c2);
    assert_eq!(c2.used, 1200, "resync_after verankert auf dem verifizierten Wert");
    assert_eq!(c2.verified_prompt, None, "resync_after räumt das Feld ab");
    // Ohne verifizierten Wert bleibt die Schätzung unverändert.
    let mut c3 = ContextEstimate::base();
    c3.add_tool("read", 200);
    resync_after(&mut c3);
    assert_eq!(c3.used, 200, "ohne Verifikation kein Sprung");
}

#[test]
fn nachtraegliche_bestaetigung_passt_usage_bar_an_neue_daten_an() {
    // Anforderung: Die Gesamtlänge der usage-bar ist die erste Spalte (der
    // serverbestätigte Kontextwert), sobald er eintrudelt. Die farbigen Blöcke
    // (Inhalts-Kategorien + Tools) bleiben die Summen der Event-Token. Sobald
    // ein Turn abgeschlossen wird und eine bestätigte `total_tokens` vorliegt,
    // springt `used` (Balkenlänge) auf diese Zahl – der Farbanteile-Anteil
    // (add_content/add_tool) bleibt davon unberührt.
    let mut ctx = ContextEstimate::base();
    // Zwei User/Agent-Events + ein Tool (Event-Summen wie in build_history_cache).
    ctx.add_content(ContentKind::User, 120); // User
    ctx.add_tool("glob", 136);
    ctx.add_content(ContentKind::Content, 80); // Assistant content
    assert_eq!(
        ctx.used,
        120 + 136 + 80,
        "Balkenlänge = Summe der Event-Token ohne Grundanteil"
    );

    // Nachträglich trifft die Server-Bestätigung ein: `used` (Gesamtlänge)
    // springt auf den Kontextwert der ersten Spalte.
    ctx.resync_to(900);
    assert_eq!(
        ctx.used, 900,
        "bestätigte Zahl steuert die Balkenlänge (erste Spalte)"
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
    ctx.resync_to(50); // used=50, window=100 → 50 % gefüllt

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
    assert_ne!(st.fg, st.bg, "die zwei Farben am Übergang unterscheiden sich");
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
            if t.starts_with('█') || t.starts_with('░') || t.starts_with('▏') || t.starts_with('▎') {
                break;
            }
            pos += disp_col(&t);
        }
        pos
    }

    let mut ctx = ContextEstimate::base();
    ctx.add_content(ContentKind::User, 120);
    ctx.resync_to(500);
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
    let (blocks, ctx) = build_history_cache(&s, 100, SymbolMode::Glyph, "m", 8192);
    let (live, _live_used) = build_live_blocks(&s, 100, SymbolMode::Glyph, 8192, &ctx);
    let logo = ChatBlock {
        lines: vec![Line::from("logo")],
        bg: None,
        gap: 0,
        is_tool: false,

    };
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
        s.chat.finalize_assistant(aid, t0, usage_zero(), 0, 0, false);
    }
    // Neuer Turn (Assistent übergibt wieder an den User).
    s.push_user_message("frage2".into(), Some(Permission::Read), "m".into());
    let aid = s.open_assistant(String::new(), "Antwort zwei".into());
    s.chat.finalize_assistant(aid, t0, usage_zero(), 0, 0, false);

    let (blocks, _ctx) = build_history_cache(&s, 200, SymbolMode::Glyph, "m", 8192);
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
        }),
        "⌕ grep \"fn main\""
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Grep {
            pattern: "TODO".into(),
            path: "src".into(),
            include: String::new(),
            num_results: 5,
        }),
        "⌕ grep \"TODO\" src - 5 results"
    );
    assert_eq!(
        tool_kind_detail(&ToolKind::Glob {
            pattern: "src/**/*.rs".into(),
            num_results: 0,
        }),
        "☰ glob \"src/**/*.rs\""
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
        "⚙ run cargo test"
    );
}
