//! Chat-Block-Rendering: Block-Aufbereitung, Cache, Scroll-Layout, Werkzeug-
//! Boxen (run/diff), Übersichts-Balken und Text-/Gedanken-Elemente.
//!
//! Die hier gebauten `ChatBlock`s werden von `draw_chat` (in `mod.rs`)
//! platziert; der `HistoryCache` bündelt die umgebrochene Historie, damit die
//! abgeschlossene Konversation nicht bei jedem Frame neu umgebrochen wird.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{ChatAnchor, Session, ViewLevel};
use crate::chat::{ChatEvent, EventKind, ToolKind};
use crate::config::SymbolMode;
use crate::llm::estimate_tokens;
use crate::perm::Permission;

use super::markdown::*;
use super::*;

/// Ein Chat-Abschnitt als eigener Paragraph: eigener Hintergrund (volles Band)
/// und bereits umbrochene, eingerückte Zeilen. Die Zeilen sind besitzend
/// (`Line<'static>`), damit Blöcke über Frames hinweg gecacht werden können.
#[derive(Clone)]
pub(crate) struct ChatBlock {
    pub(crate) lines: Vec<Line<'static>>,
    /// Hintergrund über die gesamte Breite (z. B. Eingabe-Band für User).
    pub(crate) bg: Option<Color>,
    /// Leerzeilen vor diesem Block.
    pub(crate) gap: u16,
    /// Kompakter Einzeiler für einen Werkzeug-Aufruf? Zwei aufeinanderfolgende
    /// solche Einzeiler rücken ohne Leerzeile aneinander; Konsolen-Boxen
    /// (Umrandungen) zählen hier nicht und erhalten wieder ihren Abstand.
    pub(crate) is_tool: bool,
}

/// Gecachte, umgebrochene Blöcke der abgeschlossenen Chat-Historie (ohne Logo
/// und ohne laufenden Turn). Gültig, solange `width`, die Ansichtsebene
/// (`view`) und die Historie (`version`) unverändert sind – so wird nicht bei
/// jedem Frame der gesamte (auch nicht sichtbare) Verlauf neu umgebrochen.
pub(crate) struct HistoryCache {
    pub(crate) width: usize,
    pub(crate) view: ViewLevel,
    pub(crate) version: u64,
    pub(crate) blocks: Vec<ChatBlock>,
    /// Context-Schätzer am Ende der Historie (für den laufenden Turn im
    /// Übersichts-Balken zum Weiterschreiben).
    pub(crate) end_ctx: ContextEstimate,
}

/// Baut den Historie-Cache neu, falls Breite, Ansichtsebene oder Historie
/// sich geändert haben.
pub(crate) fn ensure_history_cache(
    s: &mut Session,
    width: usize,
    mode: SymbolMode,
    model: &str,
    window: u64,
) {
    let rebuild = s
        .history_cache
        .as_ref()
        .is_none_or(|c| c.width != width || c.view != s.view || c.version != s.history_version);
    if rebuild {
        let (blocks, end_ctx) = build_history_cache(s, width, mode, model, window);
        s.history_cache = Some(HistoryCache {
            width,
            view: s.view,
            version: s.history_version,
            blocks,
            end_ctx,
        });
    }
}

/// Umbricht alle Blöcke der abgeschlossenen Historie (Messages samt ihrer
/// chronologischen Ereignisse und Fußzeilen). Ohne Logo, ohne laufenden Turn.
/// Liefert außerdem den am Ende erreichten Context-Schätzer (für den Live-Tail
/// der Übersicht). Ohne Logo, ohne laufenden Turn.
pub(crate) fn build_history_cache(
    s: &Session,
    width: usize,
    mode: SymbolMode,
    model: &str,
    window: u64,
) -> (Vec<ChatBlock>, ContextEstimate) {
    let view = s.view;
    let mut blocks: Vec<ChatBlock> = Vec::new();
    let mut ctx = ContextEstimate::base();
    let ids: Vec<crate::chat::EventId> = s.chat.order().to_vec();
    // Vorab: welche Events bekommen die grüne Verification von der Assistant-
    // Runde DIREKT danach (deren `prompt_tokens`)? Die vorangehende Zeile wird
    // vor der Runde gerendert, also muss der Wert hier schon stehen.
    let verified_before = assistant_verified_before(s);

    // Modell-Stempel und Absende-Zeitpunkt des aktuellen Turns (kommen vom
    // UserPrompt, gelten für alle nachfolgenden Assistant-Runden bis zum
    // nächsten UserPrompt). `turn_begin` dient der Gesamt-Turn-Dauer in der
    // Fußzeile am Turn-Ende.
    let mut turn_model = model.to_string();
    let mut turn_begin: Option<std::time::Instant> = None;

    for (pos, id) in ids.iter().enumerate() {
        let Some(ev) = s.chat.event(*id) else {
            continue;
        };
        // Offene (wachsende) Events gehören zum Live-Tail, nicht zur Historie.
        if ev.time_end.is_none() {
            continue;
        }
        match &ev.kind {
            EventKind::UserPrompt {
                text,
                permission,
                model: m,
                num_tokens,
                ..
            } => {
                if !m.is_empty() {
                    turn_model = m.clone();
                }
                turn_begin = ev.time_begin;
                // Grün: bestätigte Usage des zugehörigen Turns (nächste
                // abgeschlossene Assistant-Runde). Hier zählt die PROMPT-
                // Zahl (Kontext vor der Antwort), nicht `total_tokens`.
                let verified = ids[pos + 1..]
                    .iter()
                    .filter_map(|nid| s.chat.event(*nid))
                    .find(|e| {
                        e.time_end.is_some() && matches!(e.kind, EventKind::Assistant { .. })
                    })
                    .and_then(|e| match &e.kind {
                        EventKind::Assistant { reported_usage, .. } => {
                            (reported_usage.prompt_tokens > 0).then_some(reported_usage.prompt_tokens)
                        }
                        _ => None,
                    });
                if view.is_overview() {
                    // Abgeleitete Turn-Usage (`num_tokens`) vorziehen, solange
                    // sie vorliegt (>0); während des Live-Turns bzw. ohne Usage
                    // bleibt die Schätzung – so bleibt die Konto-Verbuchung
                    // (Band + block_sum) im Live-Tail stabil und deckt sich
                    // nach Turn-Ende mit der angezeigten Zahl.
                    let tokens = (*num_tokens > 0)
                        .then_some(*num_tokens)
                        .unwrap_or_else(|| estimate_tokens(text));
                    ctx.add_content(ContentKind::User, tokens);
                    ctx.block_sum += tokens;
                    ctx.verified_prompt = verified_before.get(id).copied().or(verified);
                    blocks.push(overview_user_line(
                        text,
                        width,
                        &ctx,
                        window,
                        Some(*permission),
                        (tokens > 0).then_some(tokens),
                    ));
                    resync_after(&mut ctx);
                } else if text.trim().is_empty() {
                    // leere Eingabe → nichts
                } else {
                    blocks.push(user_input_block_chat(text, *permission, width, mode));
                }
            }
            EventKind::Assistant {
                reasoning,
                text,
                tool_event_ids,
                num_tokens_reasoning,
                num_tokens_text,
                reported_usage: _,
                completion_parts: _,
            } => {
                if !reasoning.trim().is_empty() && !view.is_overview() {
                    blocks.push(thoughts_block(
                        reasoning,
                        view.thoughts_open(),
                        None,
                        width,
                        false,
                        mode,
                        None,
                    ));
                }
                // Reasoning-Anteil mit der genauen Zahl (`num_tokens_reasoning`)
                // zählen; nur wenn noch kein Usage vorliegt (0) auf die
                // Zeichen-Schätzung zurückfallen. Konsistenz zum Live-Tail: dort
                // wächst die Schätzung während des Streamings mit den Gedanken;
                // sobald die Runde abgeschlossen ist (Usage da), übernimmt hier
                // die exakte Zahl. Runden mit Usage überschreibt der Resync
                // ohnehin zusätzlich mit dem Serverwert.
                if !reasoning.trim().is_empty() && view.is_overview() {
                    let r_tokens = (*num_tokens_reasoning > 0)
                        .then_some(*num_tokens_reasoning)
                        .unwrap_or_else(|| estimate_tokens(reasoning));
                    ctx.add_content(ContentKind::Reasoning, r_tokens);
                }
                if !text.trim().is_empty() {
                    if view.is_overview() {
                        let est = *num_tokens_text;
                        ctx.add_content(ContentKind::Content, est);
                        ctx.block_sum += est;
                        ctx.verified_prompt = verified_before
                            .get(id)
                            .copied()
                            .or_else(|| assistant_sync_ctx(ev, s));
                        blocks.push(overview_text_line(text, width, &ctx, window, Some(est)));
                        // KEIN resync hier: Der rundeigene Fuß (assistant_sync_ctx
                        // unten) übernimmt direkt danach die Basis – ein Zwischen-
                        // resync der Textzeile wäre sofort wieder überschrieben.
                        ctx.verified_prompt = None;
                    } else {
                        blocks.push(text_block(text, width, mode));
                    }
                }
                // Kontext-Fuß dieser Runde (serverbestätigt) VOR den
                // Tool-Kindern setzen: Eine spätere Tool-Verifikation (die
                // `prompt_tokens` der Folgerunde, inkl. aller Tool-Ergebnisse)
                // ist der jüngere und vollständigere Kontextstand und darf den
                // Fuß danach verdrängen. Läge der Fuß HINTER den Tools, würde er
                // die Tool-Verifikation sofort wieder überschreiben.
                if view.is_overview() {
                    if let Some(v) = assistant_sync_ctx(ev, s) {
                        ctx.resync_to(v);
                    }
                }
                // Werkzeuge dieser Runde (maßgebliche Reihenfolge).
                for tid in tool_event_ids {
                    if let Some(tev) = s.chat.event(*tid) {
                        if tev.time_end.is_some() {
                            if view.is_overview() {
                                // Grüne Verification der DIRECT NACHFOLGENDEN
                                // Assistant-Runde (deren prompt_tokens) als neuer
                                // `used`-Fuß (`resync_after` läuft NACH dem
                                // add_tool in der Tool-Zeile, s. o.).
                                ctx.verified_prompt = verified_before.get(tid).copied();
                                blocks.push(overview_tool_line_chat(tev, width, &mut ctx, window));
                                resync_after(&mut ctx);
                            } else {
                                blocks.push(tool_block_chat(tev, width, view.boxes_open()));
                            }
                        }
                    }
                }
                // Fußzeile: Modell · Dauer – nur bei einer reinen Antwort ohne
                // Tool-Aufrufe. Runden mit Tool-Calls (zu denen die Ausgabe gehört)
                // bekommen keine Signatur, ebenso wenig der Overview-Modus.
                if tool_event_ids.is_empty() && !view.is_overview() {
                    blocks.push(chat_footer_block(&turn_model, ev, turn_begin, width));
                }
            }
            EventKind::Tool { .. } if ev.parent_id.is_none() => {
                // Manuelles `/run`-Tool (parent None, kein Turn zugehörig).
                if view.is_overview() {
                    ctx.verified_prompt = verified_before.get(id).copied();
                    blocks.push(overview_tool_line_chat(ev, width, &mut ctx, window));
                    resync_after(&mut ctx);
                } else {
                    blocks.push(tool_block_chat(ev, width, view.boxes_open()));
                }
            }
            EventKind::Archive {
                summary,
                num_tokens,
                ..
            } => {
                // Kompaktierung: Die Summary wird zum neuen Kontext-Anker.
                // - `verified_prompt` = Tokenzahl des Summary-Events selbst
                //   (eigener, exakt verifizierter Anker statt der sonst üblichen
                //   Verifikation durch die Folgerunde):
                // - usage-bar-Zusammensetzung = NUR Summary in exakt dieser
                //   Länge; alle bisherigen Beiträge (User/Reasoning/Content,
                //   Tools) werden auf null zurückgesetzt – der ersetzte Teil
                //   der Historie ist aus dem Kontext.
                // - `shift` = Kontextlänge im letzten Event VOR der Summary
                //   (`ctx.used`) minus der Länge der Summary. Ab hier gemessene
                //   serverbestätigte absolute Zahlen beziehen sich auf die ALTE
                //   Historie und werden in `resync_to` um diesen Betrag
                //   reduziert; der Schätz-Pfad läuft auf der Summary-Basis
                //   weiter (ergibt rechnerisch „alte absolute Zahl − shift“).
                let summary_tokens = *num_tokens;
                let shift = ctx.used.saturating_sub(summary_tokens);
                ctx.contents = [0; 5];
                ctx.contents[ContentKind::Summary as usize] = summary_tokens;
                ctx.tools = Vec::new();
                ctx.block_sum = summary_tokens;
                ctx.verified_prompt = Some(summary_tokens);
                if view.is_overview() {
                    blocks.push(overview_summary_line(
                        summary,
                        width,
                        &ctx,
                        window,
                        summary_tokens,
                    ));
                } else {
                    blocks.push(summary_block(summary, width, mode));
                }
                // Anker exakt auf die Summary-Länge setzen (Shift gilt erst
                // für die NACHFOLGENDEN Events, sonst würde er doppelt wirken).
                resync_after(&mut ctx);
                ctx.compact_shift = shift;
            }
            EventKind::Abort => {
                // Markierung eines abgebrochenen Turns (wird nicht als Inhalt
                // angezeigt, nur als dezente Fußzeile).
                blocks.push(muted_footer_line("abgebrochen", width));
            }
            EventKind::Tool { .. } => {}
        }
    }
    (blocks, ctx)
}

// ── Chat-basierte Block-Helfer (neues Event-Log) ───────────────────────────

/// User-Eingabe im Detail-Modus: volles Eingabe-Band mit Berechtigungsfarbe.
fn user_input_block_chat(text: &str, p: Permission, width: usize, mode: SymbolMode) -> ChatBlock {
    let mut lines = wrap_markdown(
        &decorate_emphasis(decorate_symbols(logical_lines(text), mode)),
        width,
        PAD,
    );
    let color = permission_color(p);
    lines = lines
        .into_iter()
        .map(|l| l.style(Style::default().fg(color)))
        .collect();
    ChatBlock {
        lines,
        bg: Some(INPUT_BG),
        gap: 0,
        is_tool: false,

    }
}

/// Dezente Fußzeile (Modell · Dauer) nach einem beendeten Turn. Die Dauer
/// reicht vom Absende-Zeitpunkt des UserPrompts (`turn_begin`, Fallback:
/// Beginn der ersten Assistant-Runde) bis zum Ende der letzten Runde – also
/// über den ganzen Turn, nicht nur die letzte Sub-Runde.
fn chat_footer_block(
    model: &str,
    ev: &ChatEvent,
    turn_begin: Option<std::time::Instant>,
    width: usize,
) -> ChatBlock {
    let mut parts = vec![model.to_string()];
    if let (Some(b), Some(e)) = (turn_begin.or(ev.time_begin), ev.time_end) {
        let ms = e.duration_since(b).as_millis() as u64;
        parts.push(fmt_duration(ms));
    }
    muted_footer_line(&format!("— {}", parts.join(" · ")), width)
}

/// Gedeckte, einzeilige Fuß-/Hinweiszeile.
fn muted_footer_line(text: &str, width: usize) -> ChatBlock {
    let line = Line::from(Span::styled(text.to_string(), Style::default().fg(MUTED)));
    ChatBlock {
        lines: wrap_block(&[line], width, PAD),
        bg: None,
        gap: 0,
        is_tool: false
    }
}

fn def_tool_line(width: usize) -> ChatBlock {
    let line = Line::from(Span::styled("⛭ tool", Style::default().fg(MUTED)));
    ChatBlock {
        lines: wrap_block(&[line], width, PAD),
        bg: None,
        gap: 0,
        is_tool: true
    }
}

/// Tool-Name eines `ToolKind` (für `tool_icon`/`tool_color`).
fn tool_kind_name(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Run { .. } => "run",
        ToolKind::Edit { .. } => "edit",
        ToolKind::Read { .. } => "read",
        ToolKind::Write { .. } => "write",
        ToolKind::Grep { .. } => "grep",
        ToolKind::Glob { .. } => "glob",
        ToolKind::Webfetch { .. } => "webfetch",
    }
}

/// Rohes Detail eines Tools aus den strukturierten `ToolKind`-Feldern (ohne
/// Icon/Name): `run` → Kommando, `read` → Pfad + Range, `grep`/`glob` → Muster
/// (+Pfad, +Trefferzahl), `webfetch` → Host · Prompt, `write`/`edit` → Pfad.
fn tool_kind_body(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Run { command, .. } => command.trim().to_string(),
        ToolKind::Edit { path, rows } => {
            let info = crate::diff::DiffInfo {
                path: path.clone(),
                rows: rows.clone(),
            };
            let (added, removed) = info.summary();
            let mut d = path
                .trim_start_matches("✎ ")
                .trim_start_matches("edit ")
                .to_string();
            if added > 0 {
                d.push_str(&format!(" +{added}"));
            }
            if removed > 0 {
                d.push_str(&format!(" -{removed}"));
            }
            d
        }
        ToolKind::Read { path, range } => {
            if range.is_empty() {
                path.clone()
            } else {
                format!("{path} {range}")
            }
        }
        ToolKind::Write { path } => path.clone(),
        ToolKind::Grep { pattern, path, num_results, .. } => {
            let mut d = format!("\"{}\"",  pattern);
            if !path.is_empty() && path != "." {
                d.push(' ');
                d.push_str(path);
            }
            if *num_results > 0 {
                d.push_str(&format!(" - {num_results} results"));
            }
            d
        }
        ToolKind::Glob { pattern, num_results } => {
            let mut d = format!("\"{}\"",  pattern);
            if *num_results > 0 {
                d.push_str(&format!(" - {num_results} results"));
            }
            d
        }
        ToolKind::Webfetch { url, prompt } => {
            // Host (+ Pfadanfang) der URL plus gekürzter Prompt – so sieht der
            // User im Verlauf, WOHIN und WOZU abgerufen wurde.
            let target: String = url
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(url)
                .chars()
                .take(60)
                .collect();
            let short: String = prompt.chars().take(60).collect();
            if short.is_empty() {
                target
            } else {
                format!("{target} · {short}")
            }
        }
    }
}

/// Toolname + Detail ohne Icon (z. B. `run cargo test`) – Kopf der Konsolen-Box
/// (die hängt ihr eigenes `⚙` an) und Live-Label.
fn tool_kind_label(kind: &ToolKind) -> String {
    format!("{} {}", tool_kind_name(kind), tool_kind_body(kind))
        .trim_end()
        .to_string()
}

/// Komplette Kurzform mit Icon + Toolname + Detail (z. B. `⌕ grep TODO src - 5
/// results`) – gemeinsame Zeile von Detail-Ansicht und Overview, ohne
/// Ausgabe-Beitrag.
pub(crate) fn tool_kind_detail(kind: &ToolKind) -> String {
    format!("{} {}", tool_icon(tool_kind_name(kind)), tool_kind_label(kind))
        .trim_end()
        .to_string()
}

/// Einzeiliger, kind-basierter Tool-Block in Kategoriefarbe (Detail-Ansicht).
fn tool_kind_row(kind: &ToolKind, width: usize) -> ChatBlock {
    let line = Line::from(Span::styled(
        tool_kind_detail(kind),
        Style::default().fg(tool_color(tool_kind_name(kind))),
    ));
    ChatBlock {
        lines: wrap_block(&[line], width, PAD),
        bg: None,
        gap: 0,
        is_tool: true,
    }
}

/// Zeigt ein abgeschlossenes Tool-Event im Detail-Modus. `run` mit Ausgabe
/// bekommt eine Konsolen-Box (Kopf: `⚙ run <kommando>`); ab dem Dialog-Level
/// (Detailed/Dialog) `edit` die Diff-Box aus `diff_block`; alle anderen
/// Kategorien einen Log-Einzeiler aus `tool_kind_detail`.
fn tool_block_chat(ev: &ChatEvent, width: usize, run_open: bool) -> ChatBlock {
    let EventKind::Tool { output, kind, .. } = &ev.kind else {
        return def_tool_line(width);
    };
    match kind {
        ToolKind::Run { .. } if !output.trim().is_empty() => {
            live_run_block(&tool_kind_label(kind), output, width, run_open, false)
        }
        // Ab dem Dialog-Level (Detailed/Dialog) → die Diff-Box aus `diff_block`;
        // Compact bleibt die Einzeiler-Zusammenfassung (`✎ edit <pfad> +N -M`).
        ToolKind::Edit { path, rows } if run_open => {
            let diff = crate::diff::DiffInfo {
                path: path.clone(),
                rows: rows.clone(),
            };
            // Im neuen Event-Log ist der Erfolg (`ok`) nicht persistiert; ein
            // Edit mit Diff-Zeilen ist in der Praxis erfolgreich ausgeführt.
            diff_block(&tool_kind_label(kind), true, &diff, width, run_open)
        }
        _ => tool_kind_row(kind, width),
    }
}

/// Tool-Einzeiler in der Übersicht mit Context-Balken.
fn overview_tool_line_chat(
    ev: &ChatEvent,
    width: usize,
    ctx: &mut ContextEstimate,
    window: u64,
) -> ChatBlock {
    let EventKind::Tool {
        function_name,
        kind,
        num_tokens_input,
        num_tokens_output,
        ..
    } = &ev.kind
    else {
        return overview_row("⛭ tool".into(), Style::default().fg(MUTED), width, ctx, window, true, None);
    };
    let tool = tool_name(function_name);
    let call = *num_tokens_input;
    let answer = *num_tokens_output;
    let total = call + answer;
    ctx.add_tool(tool, total);
    ctx.block_sum += total;
    let label = tool_kind_detail(kind);
    let style = Style::default().fg(tool_color(tool));
    // Zwei Token-Zahlen vor dem Balken: eigenständig der Tool-Call
    // (`num_tokens_input`) und die Antwort/Ergebnis (`num_tokens_output`),
    // z. B. „ 40+100“.
    let ann = (total > 0).then(|| match (call, answer) {
        (0, b) => b.to_string(),
        (c, 0) => c.to_string(),
        (c, b) => format!("{c}+{b}"),
    });
    overview_row(label, style, width, ctx, window, true, ann)
}

/// Gedämpfter, kursiver Hinweis für eine kontextkomprimierte Zusammenfassung.
pub(crate) fn summary_block(content: &str, width: usize, mode: SymbolMode) -> ChatBlock {
    let logical: Vec<Line> = decorate_symbols(logical_lines(content), mode);
    let lines: Vec<Line> = wrap_markdown(&logical, width, PAD)
        .into_iter()
        .map(|line| line.style(Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)))
        .collect();
    ChatBlock {
        lines,
        bg: None,
        gap: 0,
        is_tool: false,

    }
}

/// Umbricht den laufenden Turn (Streaming-Tail) und ggf. den Fehlerhinweis –
/// klein und wird pro Frame neu gebaut, während die Historie aus dem Cache
/// kommt. Für die Übersichts-Balken läuft der Context-Schätzer ab dem Ende der
/// Historie (`start_ctx`) weiter. Liefert die Blocks UND den finalen
/// Kontextstand (`live_ctx.used`) für die Statusleisten-Anzeige.
pub(crate) fn build_live_blocks(
    s: &Session,
    width: usize,
    mode: SymbolMode,
    window: u64,
    start_ctx: &ContextEstimate,
) -> (Vec<ChatBlock>, u64) {
    let view = s.view;
    let mut blocks: Vec<ChatBlock> = Vec::new();
    let mut live_ctx = start_ctx.clone();
    let verified_before = assistant_verified_before(s);
    // Offene (wachsende) Events chronologisch rendern: offene Assistant-Runden
    // (Reasoning + Text) und offene Tool-Events (Live-Ausgabe). Abgeschlossene
    // Events stehen bereits im Historie-Cache.
    for ev in s.chat.iter().filter(|e| e.time_end.is_none()) {
        match &ev.kind {
            EventKind::Assistant {
                reasoning,
                text,
                tool_event_ids,
                ..
            } => {
                if !reasoning.trim().is_empty() {
                    live_ctx.add_content(ContentKind::Reasoning, estimate_tokens(reasoning));
                    if !view.is_overview() {
                        blocks.push(thoughts_block(
                            reasoning,
                            view.thoughts_open(),
                            None,
                            width,
                            true,
                            mode,
                            None,
                        ));
                    }
                }
                if !text.trim().is_empty() {
                    let est = estimate_tokens(text);
                    live_ctx.add_content(ContentKind::Content, est);
                    live_ctx.block_sum += est;
                    if view.is_overview() {
                        blocks.push(overview_text_line(text, width, &live_ctx, window, None));
                    } else {
                        blocks.push(text_block(text, width, mode));
                    }
                }
                // Tool-Kinder dieser Runde: offene live (wachsend), beendete
                // statisch – die Runde selbst ist noch offen, also rendert die
                // Historie sie (noch) nicht. Ohne diesen Zweig würden fertige
                // Tools zwischen `ToolEnd` und Runden-Schluss (nächstes
                // Chunk/Reasoning/Usage bzw. Done) aus der UI verschwinden.
                for tid in tool_event_ids {
                    if let Some(tev) = s.chat.event(*tid) {
                        if tev.time_end.is_none() {
                            let label = ev_tool_label(tev, s);
                            let out = ev_tool_output(tev);
                            if view.is_overview() {
                                let t = tool_name(&label);
                                let est = estimate_tokens(&label) + estimate_tokens(out);
                                live_ctx.add_tool(t, est);
                                live_ctx.block_sum += est;
                                blocks.push(overview_active_tool_line(&label, width, &live_ctx, window));
                            } else if !out.is_empty() {
                                blocks.push(live_run_block(&label, out, width, view.boxes_open(), true));
                            } else {
                                blocks.push(active_tool_line(&label, width));
                            }
                        } else {
                            if view.is_overview() {
                                live_ctx.verified_prompt = verified_before.get(tid).copied();
                                blocks.push(overview_tool_line_chat(tev, width, &mut live_ctx, window));
                                resync_after(&mut live_ctx);
                            } else {
                                blocks.push(tool_block_chat(tev, width, view.boxes_open()));
                            }
                        }
                    }
                }
            }
            EventKind::Tool { .. } if ev.parent_id.is_none() => {
                // Manuelles `/run`-Tool (live).
                let label = ev_tool_label(ev, s);
                let out = ev_tool_output(ev);
                if view.is_overview() {
                    let est = estimate_tokens(&label) + estimate_tokens(out);
                    live_ctx.add_tool("run", est);
                    live_ctx.block_sum += est;
                    blocks.push(overview_active_tool_line(&label, width, &live_ctx, window));
                } else if !out.is_empty() {
                    blocks.push(live_run_block(&label, out, width, view.boxes_open(), true));
                } else {
                    blocks.push(active_tool_line(&label, width));
                }
            }
            _ => {}
        }
    }
    if let Some(err) = &s.error {
        let mut lines = vec![Line::from(Span::styled(
            err.clone(),
            Style::default().fg(ERROR_FG),
        ))];
        if let Some(path) = &s.error_debug {
            lines.push(Line::from(Span::styled(
                format!("Debug-Material: {path}"),
                Style::default().fg(MUTED),
            )));
        }
        blocks.push(ChatBlock {
            lines: wrap_block(&lines, width, PAD),
            bg: None,
            gap: 0,
            is_tool: false,

        });
    }
    (blocks, live_ctx.used)
}

/// Live/Status-Label eines Tool-Events: bevorzugt das aktive Tool der Session,
/// sonst eine Ableitung aus der `ToolKind`-Kurzform (Name + Detail, ohne Icon –
/// Konsolen-Box und aktive Zeile ergänzen ihr eigenes Symbol).
fn ev_tool_label(ev: &ChatEvent, s: &Session) -> String {
    if let Some(l) = &s.active_tool_label {
        return l.clone();
    }
    let EventKind::Tool { kind, .. } = &ev.kind else {
        return "run".into();
    };
    tool_kind_label(kind)
}

/// Aktueller (ggf. wachsender) Output eines Tool-Events.
fn ev_tool_output(ev: &ChatEvent) -> &str {
    let EventKind::Tool { output, .. } = &ev.kind else {
        return "";
    };
    output
}

/// Beschreibt einen platzierten Block im Scroll-Stapel: Referenz, Zeile der
/// ersten Blockzeile (nach seiner Lücke) und Zeilenzahl.
pub(crate) struct Placed<'a> {
    pub(crate) block: &'a ChatBlock,
    pub(crate) top: usize,
    pub(crate) height: usize,
}

/// Baut das gemeinsame Layout (Logo an Index 0, dann Historie, dann Live-Tail):
/// liefert die platzierten Blöcke samt Gesamthöhe. Lücken folgen der Push-Regel:
/// Logo-Margin oben, Leerzeile zwischen Logo und erstem Inhalt, sonst 1 (bzw. 0
/// zwischen zwei Werkzeug-Einzeilern).
pub(crate) fn layout_blocks<'a>(
    logo: &'a ChatBlock,
    history: &'a [ChatBlock],
    live: &'a [ChatBlock],
) -> (Vec<Placed<'a>>, usize) {
    let mut placed = Vec::with_capacity(1 + history.len() + live.len());
    placed.push(Placed {
        block: logo,
        top: logo.gap as usize,
        height: logo.lines.len(),
    });
    let mut total = logo.gap as usize + logo.lines.len();
    let mut prev_tool = false;
    let mut first_content = true;
    for b in history.iter().chain(live.iter()) {
        let gap = if first_content {
            1 // Leerzeile zwischen Logo und erstem Inhalt
        } else if prev_tool && b.is_tool {
            0
        } else {
            1
        };
        placed.push(Placed {
            block: b,
            top: total + gap,
            height: b.lines.len(),
        });
        total += gap + b.lines.len();
        prev_tool = b.is_tool;
        first_content = false;
    }
    (placed, total)
}

/// Wandelt einen Scroll-Anker `(Block, Offset)` in die absolute Zeile um
/// (Zeile der ersten Blockzeile plus Offset, geklemmt auf den Block).
pub(crate) fn anchor_to_line(tops: &[usize], heights: &[usize], anchor: ChatAnchor) -> usize {
    let n = tops.len();
    if n == 0 {
        return 0;
    }
    let b = anchor.block.min(n - 1);
    let off = anchor.offset.min(heights[b].saturating_sub(1));
    tops[b] + off
}

/// Wandelt eine absolute Zeile in einen Scroll-Anker um: es wird der Block
/// gewählt, der die Zeile enthält (sonst der erste), der Offset auf den Block
/// geklemmt.
pub(crate) fn line_to_anchor(tops: &[usize], heights: &[usize], line: usize) -> ChatAnchor {
    let n = tops.len();
    if n == 0 {
        return ChatAnchor::default();
    }
    let mut j = 0usize;
    for (i, &t) in tops.iter().enumerate() {
        if t <= line {
            j = i;
        } else {
            break;
        }
    }
    let off = line
        .saturating_sub(tops[j])
        .min(heights[j].saturating_sub(1));
    ChatAnchor {
        block: j,
        offset: off,
    }
}


/// Laufendes Werkzeug als Einzeiler („⚙ tool …“) – in der Übersicht statt der
/// Live-Konsolen-Box, sonst solange noch keine Ausgabe anfällt.
pub(crate) fn active_tool_line(tool: &str, width: usize) -> ChatBlock {
    let amber = Color::Rgb(245, 158, 11);
    ChatBlock {
        lines: wrap_block(
            &[Line::from(Span::styled(
                format!("  {} {tool} …", tool_icon(tool_name(tool))),
                Style::default().fg(amber).add_modifier(Modifier::ITALIC),
            ))],
            width,
            PAD,
        ),
        bg: None,
        gap: 0,
        is_tool: true,

    }
}

/// Laufendes Werkzeug als Einzeiler in der Übersicht – mit Context-Balken.
pub(crate) fn overview_active_tool_line(
    tool: &str,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
) -> ChatBlock {
    let amber = Color::Rgb(245, 158, 11);
    let label = format!("{} {tool} …", tool_icon(tool_name(tool)));
    overview_row(
        label,
        Style::default().fg(amber).add_modifier(Modifier::ITALIC),
        width,
        ctx,
        window,
        true,
        None,
    )
}

/// Werkzeug-Name aus dem Label (erster Token, z. B. `run` aus `run cargo test`).
pub(crate) fn tool_name(label: &str) -> &str {
    label.split_whitespace().next().unwrap_or("")
}

/// Symbol eines Werkzeug-Aufrufs nach Kategorie: Zahnrad für `run`, Pfeile in
/// Richtung für `read` (←) / `write` (→), Lupe für `grep`, Stern für `glob`,
/// Download-Pfeil für `webfetch`, Stift für `edit`. Unbekanntes fällt auf ⛭
/// zurück.
pub(crate) fn tool_icon(tool: &str) -> &'static str {
    match tool {
        "run" => "⚙",
        "read" => "←",
        "write" => "→",
        "edit" => "✎",
        "grep" => "⌕",
        "glob" => "☰",
        "webfetch" => "↧",
        _ => "⛭",
    }
}

/// Dezente Kategorie-Farbe je Werkzeug, angelehnt an die Berechtigungsfarben
/// (read=blau, write/edit=amber, run=rot) plus eigene Töne für grep (teal),
/// glob (grün) und webfetch (violett). Unbekanntes fällt auf MUTED zurück.
pub(crate) fn tool_color(tool: &str) -> Color {
    match tool {
        "run" => Color::Rgb(232, 138, 138),
        "read" => Color::Rgb(121, 182, 242),
        "write" | "edit" => Color::Rgb(238, 198, 93),
        "grep" => Color::Rgb(94, 200, 186),
        "glob" => Color::Rgb(129, 201, 149),
        "webfetch" => Color::Rgb(176, 148, 244),
        _ => MUTED,
    }
}

/// Feste Darstellungs-Reihenfolge der Tool-Kategorien in der usage-bar (nach
/// den Inhalts-Kategorien): webfetch, glob, grep, read, write/edit, run.
/// Unbekannte Tools (kein Standard-Name) erhalten Rang 6 und folgen damit am
/// Ende – untereinander in Auftretensreihenfolge.
fn tool_rank(tool: &str) -> usize {
    match tool {
        "webfetch" => 0,
        "glob" => 1,
        "grep" => 2,
        "read" => 3,
        "write" | "edit" => 4,
        "run" => 5,
        _ => 6,
    }
}

/// Inhalts-Kategorien der usage-bar (der frühere einheitliche „Band“, jetzt
/// aufgeschlüsselt). Reihenfolge der Darstellung von links nach rechts:
/// Summary, User, Assistant Reasoning, Assistant Content, Other – danach folgen
/// die Tool-Kategorien. `Other` sammelt Inhalte, die keiner der vier benannten
/// Kategorien zugeordnet werden können.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Summary,
    User,
    Reasoning,
    Content,
    Other,
}

impl ContentKind {
    /// Dezent getrennte Kategorie-Farbe, deutlich abgesetzt von den
    /// Tool-Farben. `Reasoning` und `Content` spiegeln die Chat-Farben ihrer
    /// Texte: die Gedanken sind zart grau (`MUTED`), der Antworttext ist weiß
    /// (Standard-Textfarbe) – dadurch bleiben sie voneinander unterscheidbar.
    pub(crate) fn color(self) -> Color {
        match self {
            ContentKind::Summary => Color::Rgb(198, 146, 108),
            ContentKind::User => Color::White,
            ContentKind::Reasoning => MUTED,
            ContentKind::Content => Color::Black,
            ContentKind::Other => Color::Rgb(90, 100, 120),
        }
    }
}

/// Tab-Breite beim Ersetzen fürs Rendering (wie Terminal-Tab-Stops). Ratatui
/// verwirft Steuerzeichen (inkl. Tab) beim Rendern komplett – ohne Expansion
/// wären Tabs in Tool-Labels, Konsolen- und Code-Output unsichtbar. Tabs
/// werden durch so viele Leerzeichen ersetzt, dass die nächste Tab-Stop-Spalte
/// erreicht wird (vgl. echtes Terminal). Messung (`char_w(' ') == 1`) und
/// Rendering stimmen dadurch exakt überein – die Zeichenzählung wird nicht
/// durcheinandergebracht, weil die Expansion stets VOR der Messung erfolgt.
pub(crate) const TAB_WIDTH: usize = 8;

/// Spielt eine einzelne Zeile ab und ersetzt jeden Tab durch Leerzeichen bis
/// zur nächsten Tab-Stop-Spalte, ausgehend von Spalte `col` (innerhalb der
/// Zeile). Gibt die expandierte Zeile und die neue Spaltenposition zurück.
///
/// `col` zählt dabei über die tatsächlich gerenderten Zellen (expandierte
/// Leerzeichen eingerechnet). `pad_to` und `wrap_preformatted` expandieren Tabs
/// aus eigener Kraft (sie müssen das mit Umbruch/`\r`-Semantik verzahnen);
/// `fit_label` nutzt diese Funktion als gemeinsame, konsistente Basis.
fn expand_line_tabs(line: &str, mut col: usize) -> (String, usize) {
    if !line.contains('\t') {
        return (line.to_string(), col + display_units(line));
    }
    let mut out = String::with_capacity(line.len() + 8);
    for c in line.chars() {
        if c == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..n {
                out.push(' ');
            }
            col += n;
        } else {
            let cw = char_w(c);
            out.push(c);
            col += cw;
        }
    }
    (out, col)
}

/// Anzeige-Breite eines Strings in Zellen (Messriemen).
fn display_units(s: &str) -> usize {
    s.chars().map(char_w).sum()
}

/// Kürzt einen Einzeiler auf `max` Zellen (Breite) und hängt „…“ an, wenn
/// abgeschnitten – so bleibt in der Übersicht jeder Werkzeug-Aufruf auf einer
/// Zeile (nie umbrechen).
pub(crate) fn fit_label(s: &str, max: usize) -> String {
    // Tabs zuerst Tab-Stop-bewusst expandieren (→ Leerzeichen), damit sie
    // sichtbar gerendert und korrekt vermessen werden statt von Ratatui
    // verworfen zu werden. Die Expansion läuft VOR der Vermessung, daher
    // stimmen Messung und Rendering (jeweils über die expandierte Breite).
    let (expanded, total) = expand_line_tabs(s, 0);
    if total <= max {
        return expanded;
    }
    // Zu lang: expandiert truncaten, dabei `used` über die Zellen führen.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        if c == '\t' {
            let n = TAB_WIDTH - (used % TAB_WIDTH);
            for _ in 0..n {
                if used >= budget {
                    break;
                }
                out.push(' ');
                used += 1;
            }
        } else {
            let cw = char_w(c);
            if used + cw > budget {
                break;
            }
            out.push(c);
            used += cw;
        }
    }
    out.push('…');
    out
}

/// Flacht Text für die Übersichtsansicht ab: Zeilenumbrüche werden zu
/// Leerzeichen, mehrfache Whitespaces werden zusammengefasst. Leitetexte
/// wie „Hier ist …" erscheinen so als flüssiger Satz statt gebrochen.
pub(crate) fn flatten_for_overview(text: &str) -> String {
    let mut out = String::new();
    let mut prev_space = true; // Leerzeichen am Anfang verschlucken
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    // Tailing whitespace entfernen
    out.trim_end().to_string()
}

/// Kürzt eine Folge stylisierter Spans auf `max` Display-Zellen und hängt
/// „…" an, wenn abgeschnitten wurde. Der Abschnitt endet immer an einer
/// sauberen Span-Grenze (kein mitten im Token abgeschnittener Text).
pub(crate) fn truncate_styled_spans(spans: Vec<Span<'_>>, max: usize) -> Vec<Span<'static>> {
    if max == 0 {
        return Vec::new();
    }
    let budget = max.saturating_sub(1); // 1 Zelle für „…"
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();

    for span in spans {
        let mut chunk = String::new();
        let mut chunk_used = 0usize;
        for c in span.content.chars() {
            let cw = char_w(c);
            if used + chunk_used + cw > budget {
                // Aktuellen Chunk abschließen
                if !chunk.is_empty() {
                    out.push(Span::styled(chunk, span.style));
                }
                // „…" anhängen wenn überhaupt etwas kommt
                if !out.is_empty() || used > 0 {
                    out.push(Span::styled("…".to_string(), Style::default()));
                }
                return out;
            }
            chunk.push(c);
            chunk_used += cw;
        }
        // Ganzer Span passt
        if !chunk.is_empty() {
            used += chunk_used;
            out.push(Span::styled(chunk, span.style));
        }
    }
    // Alles passte – kein „…" nötig
    out
}

/// Fester Platz (in Zellen) für die rechts angefügte Token-Annotation der
/// Context-Balken (` 99.9kT` = Leerzeichen + max. 6 Z. „99.9kT“). Wird IMMER
/// reserviert, damit der Balken unabhängig von der Annotation gleich lang
/// bleibt – fehlt eine genaue Token-Zahl, bleibt der Platz einfach leer.
pub(crate) const CONTEXT_ANN_CELLS: usize = 7;

/// Zusätzlicher fester Platz (in Zellen) für die zweite, direkt hinter der
/// Server-Annotation angefügte Zahl: die Summe der block_token aller Blocks bis
/// hierher (` 99.9kT`). Wird IMMER reserviert – der usage-Balken ist dadurch
/// konstant so viele Zeichen kürzer, damit beides in eine Zeile passt.
pub(crate) const CONTEXT_BLOCKSUM_CELLS: usize = 7;

/// Links-füllende Unicode-Partial-Blöcke (U+258F..U+2588): Index `n` = `n/8`
/// der Zellenbreite von links gefüllt, `8` = voller Block. Jedes Zeichen ist
/// 1 Zelle breit – in Kombination mit Vorder- und Hintergrundfarbe lassen sich
/// damit sub-zeichengenaue Farbübergänge in der usage-bar darstellen.
const PARTIAL_LEFT: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Sub-Zellen-Auflösung (= Anzahl der Partial-Blöcke) je Zelle.
const BAR_SUB: usize = 8;

/// Farbe des ungenutzten / nicht zuordenbaren Rest-Anteils der usage-bar.
/// Bewusst dunkel (in der Nähe von `BASE_BG`), damit der flächig gefüllte
/// Leer-Rest dezent bleibt und der sub-zeichengenaue Übergang vom letzten
/// Farbbereich stimmig (ohne hellen „Streifen“) an ihn anschließt.
const BAR_LEER: Color = Color::Rgb(29, 32, 40);


/// Laufende Context-Schätzung für die Übersichts-Balken: kumulierte Token und
/// ihre Aufteilung nach Kategorien (Inhalts-Anteile in `ContentKind`-Kategorien,
/// je Tool-Kategorie ein eigener, farbiger Anteil – kumuliert über alle Aufrufe).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ContextEstimate {
    /// Geschätzte Gesamt-`total_tokens` bis hierher: die Summe der Event-
    /// Schätzungen. Sobald eine serverbestätigte `total_tokens` vorliegt,
    /// springt `used` dorthin (`resync_to`) und die Event-Schätzungen laufen
    /// auf diesem echten Wert weiter – die erste Zahl der Balken (und damit die
    /// Gesamtlänge der Bar) ist also „letzte gemeldete total_tokens + Schätzungen
    /// seitdem“.
    pub(crate) used: u64,
    /// Kumulierte Token je Inhalts-Kategorie (Summary/User/Reasoning/Content/Other).
    /// Eine Kompaktierung setzt die Zusammensetzung auf „nur Summary“ zurück.
    pub(crate) contents: [u64; 5],
    /// Je Tool-Kategorie die kumulierten Token (falls dort etwas anfiel).
    pub(crate) tools: Vec<(String, u64)>,
    /// Vom Server bestätigte Gesamt-`total_tokens` DES GERADE RENDERTEN Turns
    /// (der abgeschlossenen Abschluss-Zeile mit Usage). Wird nur transitär an
    /// der Antwort-Zeile dieses Turns gesetzt und danach sofort wieder `None`
    /// gesetzt – Werkzeug-Zeilen, Zwischen-Kommentare und spätere Runden (die
    /// nur Schätzungen sind) tragen nie eine Token-Zahl.
    pub(crate) verified_prompt: Option<u64>,
    /// Kumulierte Summe der block_token aller bis hierher gezeichneten Blocks.
    /// Zweite Token-Zahl rechts neben der Server-Annotation; wächst mit jedem
    /// Block, der in der Übersicht eine Zeile bekommt. Ein `Archive`-Event
    /// (Kompaktierung) setzt sie auf die Summary-Schätzung zurück – danach
    /// summiert erst das nächste Event wieder normal auf.
    pub(crate) block_sum: u64,
    /// Verschiebung durch die letzte Kompaktierung: Kontextlänge im letzten
    /// Event VOR der Summary minus der Länge der Summary. Nachfolgende
    /// serverbestätigte absolute Zahlen (Verifikationen) sind gegen die ALTE
    /// (vorkompaktierte) Historie gemessen und werden in `resync_to` um genau
    /// diesen Betrag reduziert; die Estimator-Basis läuft ab der Summary auf
    /// dem verbleibenden Kontext. Vor einer Kompaktierung ist der Wert 0.
    pub(crate) compact_shift: u64,
}

impl ContextEstimate {
    /// Start mit leerem Kontext (bewusst OHNE Grundanteil: die Gesamtlänge und
    /// die Farbanteile entstehen ausschließlich aus den Event-Token).
    pub(crate) fn base() -> Self {
        ContextEstimate {
            used: 0,
            contents: [0; 5],
            tools: Vec::new(),
            verified_prompt: None,
            block_sum: 0,
            compact_shift: 0,
        }
    }
    /// Bucht Token auf eine Inhalts-Kategorie (und erhöht die Gesamt-`used`).
    pub(crate) fn add_content(&mut self, kind: ContentKind, tokens: u64) {
        self.used += tokens;
        self.contents[kind as usize] += tokens;
    }
    pub(crate) fn add_tool(&mut self, tool: &str, tokens: u64) {
        self.used += tokens;
        match self.tools.iter_mut().find(|(t, _)| t == tool) {
            Some((_, v)) => *v += tokens,
            None => self.tools.push((tool.to_string(), tokens)),
        }
    }
    /// Korrektur der laufenden Schätzung auf eine serverbestätigte
    /// `total_tokens`: `used` springt auf den echten Wert, von dem ab die
    /// Schätzwerte der nachfolgenden Events weiteraddieren („letzte reportete
    /// total_tokens darüber + Schätzungen seitdem“). `contents`/`tools` bleiben
    /// unverändert – sie dienen nur der proportionalen Balken-Aufteilung.
    ///
    /// Nach einer Kompaktierung (`compact_shift > 0`) ist die bestätigte Zahl
    /// gegen die ALTE (vorkompaktierte) Historie gemessen; `used` wird um den
    /// Shift reduziert, damit die Anzeige dem neuen, kürzeren Kontext folgt.
    /// Vor einer Kompaktierung (Shift 0) verhält sich die Funktion unverändert.
    pub(crate) fn resync_to(&mut self, total: u64) {
        self.used = total.saturating_sub(self.compact_shift);
    }
}

/// Verifikations-Leuchtfeuer: JEDE abgeschlossene Assistant-Runde mit
/// serverbestätigten `prompt_tokens > 0` verifiziert das unmittelbar VOR ihr
/// liegende Event – dessen Übersichts-Zeile bekommt die `prompt_tokens` als
/// grüne Zahl (Kontextstand beim Start dieser Antwort). Unabhängig davon, ob
/// die Runde Content hat; insbesondere wirken damit auch CONTENT-LOSE Runden
/// (reine Gedanken), die selbst keine Zeile rendern und darum sonst übersprungen
/// würden.
///
/// Warum Look-back: die vorangehende Zeile wird VOR der Assistant-Runde
/// gerendert – die Verifikation muss daher vor dem Haupt-Loop vorliegen.
pub(crate) fn assistant_verified_before(s: &Session) -> std::collections::HashMap<crate::chat::EventId, u64> {
    let ids: Vec<crate::chat::EventId> = s.chat.order().to_vec();
    let mut out = std::collections::HashMap::new();
    for j in 1..ids.len() {
        let Some(ev) = s.chat.event(ids[j]) else {
            continue;
        };
        let EventKind::Assistant { reported_usage, .. } = &ev.kind else {
            continue;
        };
        if ev.time_end.is_none() || reported_usage.prompt_tokens == 0 {
            continue;
        }
        out.insert(ids[j - 1], reported_usage.prompt_tokens);
    }
    out
}

/// Re-anchor-`used` auf den zuvor gesetzten verifizierten Wert einer
/// Übersichts-Zeile. Läuft IMMER NACH dem Rendern der Zeile, weil der Aufrufer
/// vorher die Event-Schätzung des Blocks addiert haben kann (z. B. `add_tool`)
/// – der absolute verifizierte Wert enthält diesen Beitrag bereits und ersetzt
/// die Schätzung so ohne Doppelzählung. Eine verifizierte Zeile wird damit zum
/// neuen `used`-Anker: alle Folgezeilen rechnen auf der bestätigten Basis weiter.
pub(crate) fn resync_after(ctx: &mut ContextEstimate) {
    if let Some(v) = ctx.verified_prompt.take() {
        ctx.resync_to(v);
    }
}

/// Serverbestätigte Kontext-Zahl einer abgeschlossenen Assistant-Runde, an der
/// die erste Spalte synchronisiert wird.
///
/// Abschlussantworten (ohne tool_calls) liefern `total_tokens` (unverändert –
/// das ist für die finale Antwort gewollt). Enthält die Runde Tool-Calls,
/// werden deren Aufruf-Tokens ausgenommen:
/// `total_tokens − ∑ tool_calls` = `prompt_tokens + reasoning + content` –
/// die Tool-Argumente zählen nicht zum persistenten Kontext der Spalte.
///
/// Bevorzugt wird die beim Streaming gemessene Aufschlüsselung
/// (`completion_parts`); fehlt sie, liefern die angehängten Tool-Events
/// (`num_tokens_input`) die Call-Token-Summe.
pub(crate) fn assistant_sync_ctx(ev: &ChatEvent, s: &Session) -> Option<u64> {
    let EventKind::Assistant {
        reported_usage,
        tool_event_ids,
        completion_parts,
        ..
    } = &ev.kind
    else {
        return None;
    };
    if reported_usage.total_tokens == 0 {
        return None;
    }
    // Gemessene Aufschlüsselung (exakt aus den usage-Inkrementen).
    if let Some(p) = completion_parts {
        if !p.tool_calls.is_empty() {
            return Some(reported_usage.prompt_tokens + p.reasoning + p.content);
        }
        return Some(reported_usage.total_tokens);
    }
    // Keine parts: Tool-Call-Länge aus den angehängten Tool-Events ableiten.
    let tool_len: u64 = tool_event_ids
        .iter()
        .filter_map(|tid| s.chat.event(*tid))
        .filter_map(|t| match &t.kind {
            EventKind::Tool { num_tokens_input, .. } => Some(*num_tokens_input),
            _ => None,
        })
        .sum();
    if tool_len > 0 {
        return Some(reported_usage.total_tokens.saturating_sub(tool_len));
    }
    Some(reported_usage.total_tokens)
}

/// Rechter Balken einer Übersichts-Zeile: volle Länge = Kontext-Window,
/// gefüllt = aktueller geschätzter Bedarf (`ctx.used`), aufgeteilt nach
/// Kategorien – Band (grau) zuerst, dann die Tool-Kategorien in `tool_color`,
/// jeweils proportional zu ihren kumulierten Token. Rest bleibt leer („░“).
pub(crate) fn context_bar(ctx: &ContextEstimate, window: u64, cells: usize) -> Vec<Span<'static>> {
    // Beide Zahlen stehen in einem FESTEN Zellen-Budget (`CONTEXT_ANN_CELLS`
    // bzw. `CONTEXT_BLOCKSUM_CELLS`): jede wird linksbündig auf ihre volle
    // Budget-Breite aufgefüllt, damit sie auf JEDER Zeile in derselben Spalte
    // beginnt (und die Zeile immer an derselben Spalte endet) – unabhängig
    // davon, wie viele Zeichen `fmt_ctx` liefert („1T“ bis „99.9kT“).
    fn pad_ann(s: String, cells: usize) -> String {
        let w = s.chars().map(char_w).sum::<usize>();
        format!("{s}{}", " ".repeat(cells.saturating_sub(w)))
    }
    // Erste Zahl neben dem Balken: die Gesamt-`total_tokens` an dieser Stelle.
    // Liegt eine Server-Bestätigung vor (`verified_prompt`), wird sie exakt
    // (grün) angezeigt; sonst die Schätzung (grau) = letzte gemeldete
    // `total_tokens` + die Event-Schätzungen seitdem – so steht neben JEDEM
    // Balken eine Kontext-Zahl, nicht nur auf bestätigten Zeilen.
    let ctx_ann = match ctx.verified_prompt {
        Some(vp) => (pad_ann(format!(" {}", fmt_ctx(vp)), CONTEXT_ANN_CELLS), SYM_OK),
        None => (pad_ann(format!(" {}", fmt_ctx(ctx.used)), CONTEXT_ANN_CELLS), MUTED),
    };
    // Zweite Zahl direkt dahinter: die Summe der block_token aller Blocks bis
    // hierher (` 99.9kT`). Wird IMMER reserviert (Balken entsprechend kürzer),
    // damit beides in eine Zeile passt; bei noch 0 Blocks bleibt das Feld auf
    // voller Breite leer, damit die Spalte auch dann stabil bleibt.
    let block_sum_ann = if ctx.block_sum > 0 {
        pad_ann(format!(" {}", fmt_ctx(ctx.block_sum)), CONTEXT_BLOCKSUM_CELLS)
    } else {
        " ".repeat(CONTEXT_BLOCKSUM_CELLS)
    };
    let bar_cells = cells
        .saturating_sub(CONTEXT_ANN_CELLS)
        .saturating_sub(CONTEXT_BLOCKSUM_CELLS);

    let used = ctx.used.min(window);
    // Gefüllte Länge in Sub-Zellen (8 je Zeichen) für sub-zeichengenaue
    // Farbübergänge – genauer als die bisherige Zeichen-Auflösung.
    let total_sub = (bar_cells as u64) * (BAR_SUB as u64);
    let filled_sub = total_sub
        .checked_mul(used)
        .and_then(|v| v.checked_div(window))
        .unwrap_or(0);
    let unfilled_sub = total_sub.saturating_sub(filled_sub);
    let mut segs: Vec<(Color, u64)> = Vec::new();
    // Inhalts-Kategorien in Darstellungs-Reihenfolge: Summary, User, Assistant
    // Reasoning, Assistant Content, Other – nur Segmente mit angefallem Token.
    for kind in [
        ContentKind::Summary,
        ContentKind::User,
        ContentKind::Reasoning,
        ContentKind::Content,
        ContentKind::Other,
    ] {
        let v = ctx.contents[kind as usize];
        if v > 0 {
            segs.push((kind.color(), v));
        }
    }
    // Danach die Tool-Kategorien in FESTER Reihenfolge (nicht Auftretens-
    // reihenfolge): webfetch, glob, grep, read, write/edit, run. Unbekannte
    // Tools folgen am Ende in ihrer Auftretensreihenfolge.
    let mut tools: Vec<&(String, u64)> = ctx.tools.iter().collect();
    tools.sort_by_key(|(name, _)| tool_rank(name));
    for (name, v) in tools {
        segs.push((tool_color(name), *v));
    }
    // Proportionale Basis für die Farb-Aufteilung ist die SUMME DER
    // EVENT-TOKEN (alle Inhalts-Kategorien + Tools), nicht `used`. `used`
    // (erste Spalte) steuert die Gesamtlänge/Füllung; die Farbblöcke sollen die
    // echten Event-Verhältnisse zeigen. Liegt eine bestätigte Kontextzahl vor
    // (Serverwert > Event-Summen, z. B. durch Systemprompt-Overhead), bleibt
    // der Rest der gefüllten Bar leer – er ist nicht einem Event zuzuordnen.
    let total = (ctx
        .contents
        .iter()
        .sum::<u64>()
        .saturating_add(ctx.tools.iter().map(|(_, v)| *v).sum::<u64>()))
    .max(1);
    // Sub-Zellen je Segment (der letzte Anteil schluckt Rundungsreste).
    let mut seg_subs: Vec<(Color, u64)> = Vec::with_capacity(segs.len());
    let mut remaining = filled_sub;
    for (i, (color, v)) in segs.iter().enumerate() {
        let s = if i + 1 == segs.len() {
            remaining
        } else {
            filled_sub * v / total
        };
        seg_subs.push((*color, s));
        remaining = remaining.saturating_sub(s);
    }
    // Ungenutzter/Overhead-Rest: eigener (grauer) Sub-Anteil.
    seg_subs.push((BAR_LEER, unfilled_sub));

    // Sub-Zellen-Farbfolge aufbauen und in Zellen rendern.
    let mut subs: Vec<Color> = Vec::with_capacity(total_sub as usize);
    for (color, s) in &seg_subs {
        for _ in 0..*s {
            subs.push(*color);
        }
    }
    // Füllfehler absichern (z. B. Rundung): falls die Sequenz kürzer ist als
    // die Bar, mit Leer-Farbe auffüllen.
    while subs.len() < (total_sub as usize) {
        subs.push(BAR_LEER);
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for c in 0..bar_cells {
        let start = c * BAR_SUB;
        let cell = &subs[start..start + BAR_SUB];
        let first = cell[0];
        let mut m = 1usize;
        while m < BAR_SUB && cell[m] == first {
            m += 1;
        }
        if m == BAR_SUB {
            // Zelle einheitlich gefüllt: voller Block (bzw. leerer Rest).
            if first == BAR_LEER {
                // Leere Zelle als flächige (dunkle) Hintergrundfläche – nicht
                // als „░“-Schraffur mit durchscheinendem Hintergrund. Nur so
                // stimmt die Farbe exakt mit dem `bg`-Anteil des Partial-Block
                // am Übergang überein und es entsteht kein Farbsprung zwischen
                // dem letzten Farbbereich und dem ungefüllten Balkenrest.
                spans.push(Span::styled(" ", Style::default().bg(BAR_LEER)));
            } else {
                spans.push(Span::styled("█", Style::default().fg(first)));
            }
        } else {
            // Farbübergang innerhalb der Zelle: `m/8` von links in der ersten
            // Farbe, der Rest der Zelle zeigt die zweite Farbe als Hintergrund.
            let second = cell[m];
            let ch = PARTIAL_LEFT[m];
            let st = Style::default().fg(first).bg(second);
            spans.push(Span::styled(ch.to_string(), st));
        }
    }
    // Erste Zahl: Kontextgröße (grün = exakt vom Server bestätigt, sonst graue
    // Schätzung) – immer sichtbar.
    spans.push(Span::styled(ctx_ann.0, Style::default().fg(ctx_ann.1)));
    // Zweite Zahl direkt dahinter: Summe der block_token aller Blocks bis
    // hierher (dezent, grau) – die laufende Summe der Schätzwerte je Block.
    // Immer mit voller Budget-Breite (auch leer), damit die Spalte fix bleibt.
    spans.push(Span::styled(block_sum_ann, Style::default().fg(MUTED)));
    spans
}

/// Gemeinsames Layout einer einzeiligen Übersichts-Zeile: links der bereits
/// gekürzte Text (`content`), dann rechtsbündig direkt vor dem Balken die
/// optionale Token-Annotation, dann der Context-Balken. Einzeilig, nie
/// umbrechend. Der Balken beginnt für alle Zeilen in derselben Spalte (rechtes
/// Ende immer `PAD_R` Zellen vor dem Rand).
fn overview_line(
    content: Vec<Span<'static>>,
    annotation: Option<String>,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
    is_tool: bool,
) -> ChatBlock {
    let total = width.saturating_sub(PAD + PAD_R).max(1);
    let bar_w = (total / 3).max(4);
    let left_w = total.saturating_sub(bar_w + 1);
    let ann_w = annotation.as_ref().map(|a| disp_width(a)).unwrap_or(0);
    let used: usize = content
        .iter()
        .map(|s| s.content.chars().map(char_w).sum::<usize>())
        .sum();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(PAD))];
    spans.extend(content);
    // Token-Annotation rechtsbündig direkt vor dem Balken – der Aufrufer hat
    // den Text dafür entsprechend früher gekürzt.
    spans.push(Span::raw(" ".repeat(left_w.saturating_sub(used + ann_w))));
    if let Some(ann) = annotation {
        spans.push(Span::styled(ann, Style::default().fg(MUTED)));
    }
    // Eine Leerzelle Abstand: der Balken beginnt dadurch auf allen Zeilen in
    // derselben Spalte.
    spans.push(Span::raw(" "));
    spans.extend(context_bar(ctx, window, bar_w));
    ChatBlock {
        lines: vec![Line::from(spans)],
        bg: None,
        gap: 0,
        is_tool,
    }
}

/// Baut eine Übersichts-Zeile aus Text (links) + Context-Balken (rechts).
/// Die Token-Annotation (falls vorhanden) steht nicht direkt hinter dem Text,
/// sondern rechtsbündig kurz vor dem Balken; zu langer Text wird dafür ein
/// bisschen früher mit „…“ gekürzt. `is_tool` markiert Werkzeug-Zeilen (dicht
/// an Werkzeug-Zeilen anliegend).
pub(crate) fn overview_row(
    label: String,
    style: Style,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
    is_tool: bool,
    block_tokens: Option<String>,
) -> ChatBlock {
    let total = width.saturating_sub(PAD + PAD_R).max(1);
    let bar_w = (total / 3).max(4);
    let left_w = total.saturating_sub(bar_w + 1);
    // Annotation inkl. führendem Leerzeichen; der Platz dafür wird rechts vor
    // dem Balken reserviert (→ Text etwas früher gekürzt).
    let ann = block_tokens.map(|a| format!(" {a}"));
    let ann_w = ann.as_ref().map(|a| disp_width(a)).unwrap_or(0);
    let text = fit_label(&label, left_w.saturating_sub(ann_w));
    overview_line(vec![Span::styled(text, style)], ann, width, ctx, window, is_tool)
}

/// Einzeilige Äußerungs-Zeile der Übersicht – flacht den Text (Zeilen →
/// Leerzeichen), rendert Markdown (fett/italic/code-span) und kürzt gestylt
/// auf den verfügbaren Platz links vom Context-Balken. Die Token-Annotation
/// steht rechtsbündig kurz vor dem Balken (nicht direkt hinter dem Text).
pub(crate) fn overview_text_line(
    text: &str,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
    block_tokens: Option<u64>,
) -> ChatBlock {
    let total = width.saturating_sub(PAD + PAD_R).max(1);
    let bar_w = (total / 3).max(4);
    let left_w = total.saturating_sub(bar_w + 1);

    // Text flachklopfen (Zeilen → Leerzeichen) und durch Markdown-Pipeline
    // jagen, damit fett/italic/code-span erhalten bleiben.
    let flat = flatten_for_overview(text);
    let logical = logical_lines(&flat);
    // Alle Spans aller logischen Zeilen in eine flache Folge joinen (Trennung
    // durch ein Leerzeichen zwischen den Zeilen).
    let mut flat_spans: Vec<Span<'static>> = Vec::new();
    for (i, line) in logical.iter().enumerate() {
        if i > 0 {
            flat_spans.push(Span::raw(" "));
        }
        for span in &line.spans {
            flat_spans.push(Span::styled(span.content.to_string(), span.style));
        }
    }

    // Annotation inkl. führendem Leerzeichen (` 123`); der Text wird dafür
    // früher gekürzt, damit die Zahl rechtsbündig vor dem Balken Platz findet.
    let ann = block_tokens.map(|t| format!(" {t}"));
    let ann_w = ann.as_ref().map(|a| disp_width(a)).unwrap_or(0);
    let styled = truncate_styled_spans(flat_spans, left_w.saturating_sub(ann_w));
    overview_line(styled, ann, width, ctx, window, false)
}

/// Einzeilige USER-Eingabe der Übersicht: exakt wie `overview_text_line` (1
/// Zeile + „ …“ + optionaler Token-Zahl direkt vor dem Balken), aber auf dem
/// `INPUT_BG`-Band und mit der Berechtigungsfarbe (falls vorhanden). Der
/// aufrufende `build_history_cache` liefert die abgeleitete Turn-Usage
/// (`num_tokens`, sonst Schätzung) und trägt über `ctx.verified_prompt` die
/// bestätigte Gesamt-Usage als grüne Zahl.
pub(crate) fn overview_user_line(
    text: &str,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
    permission: Option<Permission>,
    tokens: Option<u64>,
) -> ChatBlock {
    let mut blk = overview_text_line(text, width, ctx, window, tokens);
    blk.bg = Some(INPUT_BG);
    if let Some(p) = permission {
        let color = permission_color(p);
        for span in &mut blk.lines[0].spans {
            if span.style.fg.is_none() {
                span.style.fg = Some(color);
            }
        }
    }
    blk
}

/// Einzeilige Summary-Zeile der Übersicht (nach der Kompaktierung): gedämpfter,
/// kursiver Text links, rechts der Context-Balken. Die `block_sum` wurde bereits
/// auf die Summary-Tokenzahl gesetzt – die „letzte Zahl“ hinter dem Balken zeigt
/// dadurch exakt diesen (neuen) Wert. `tokens` wird zusätzlich als Annotation
/// direkt vor dem Balken angezeigt (analog zu anderen Nachrichten). `ctx`
/// trägt zu diesem Zeitpunkt `verified_prompt = tokens` (eigener Anker) und
/// eine Zusammensetzung aus NUR der Summary.
fn overview_summary_line(
    text: &str,
    width: usize,
    ctx: &ContextEstimate,
    window: u64,
    tokens: u64,
) -> ChatBlock {
    let flat = flatten_for_overview(text);
    let style = Style::default().fg(MUTED).add_modifier(Modifier::ITALIC);
    overview_row(
        flat,
        style,
        width,
        ctx,
        window,
        false,
        Some(tokens.to_string()),
    )
}


/// Konsolen-Box für ein `run`-Werkzeug: erscheint, sobald die erste Ausgabe
/// anfällt. Bei noch **laufendem** Kommando (`running`) wird die Box pro Frame
/// mit dem aktuellen Stand fortgeschrieben und unten ein „⌛ running…“-Vermerk
/// angezeigt; die abgeschlossene Box lässt den Vermerk weg. Zu lange Ausgabe
/// wird kompakt von oben gekürzt (die letzten `RUN_PREVIEW_LINES` Zeilen
/// bleiben stehen), bei `open` (Tab) vollständig angezeigt.
pub(crate) fn live_run_block(
    label: &str,
    output: &str,
    width: usize,
    open: bool,
    running: bool,
) -> ChatBlock {
    let box_width = width.saturating_sub(2 * BOX_MARGIN).max(6);
    let content_width = box_width.saturating_sub(2 + 2 * BOX_PAD).max(1);

    let border = Style::default().fg(BOX_BORDER).bg(BOX_BG);
    let header_style = Style::default().fg(MUTED).bg(BOX_BG);
    let content_style = Style::default().fg(BOX_TEXT).bg(BOX_BG);
    let muted = Style::default().fg(MUTED).bg(BOX_BG);

    let header = format!("⚙ {label}");
    let out_rows = wrap_preformatted(output.trim_end(), content_width);

    let mut parts: Vec<(String, Style)> = Vec::new();
    for h in wrap_preformatted(&header, content_width) {
        parts.push((h, header_style));
    }
    if !open && out_rows.len() > RUN_PREVIEW_LINES {
        parts.push(("…".to_string(), muted));
        for r in out_rows.iter().skip(out_rows.len() - RUN_PREVIEW_LINES) {
            parts.push((r.clone(), content_style));
        }
    } else {
        for r in &out_rows {
            parts.push((r.clone(), content_style));
        }
    }
    if running {
        parts.push(("⌛ running…".to_string(), muted));
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(parts.len() + 2);
    lines.push(box_edge(width, box_width, border, true));
    for (text, style) in &parts {
        lines.push(box_line(width, box_width, text, *style));
    }
    lines.push(box_edge(width, box_width, border, false));

    ChatBlock {
        lines,
        bg: None,
        gap: 0,
        is_tool: false,

    }
}

/// Obere/untere Umrandungszeile der Konsolen-Box, eingerückt und bis zur
pub(crate) fn box_edge(width: usize, box_width: usize, style: Style, top: bool) -> Line<'static> {
    let (l, r) = if top { ('┌', '┐') } else { ('└', '┘') };
    let inner = box_width.saturating_sub(2);
    let right = width.saturating_sub(BOX_MARGIN + box_width);
    let spans = vec![
        Span::styled(" ".repeat(BOX_MARGIN), Style::default().bg(BASE_BG)),
        Span::styled(l.to_string(), style),
        Span::styled("─".repeat(inner), style),
        Span::styled(r.to_string(), style),
        Span::styled(" ".repeat(right), Style::default().bg(BASE_BG)),
    ];
    Line::from(spans)
}

/// Eine Inhaltszeile der Konsolen-Box: Rand, Polster, Inhalt (auf
/// Inhaltsbreite aufgefüllt), Polster, Rand – bündig zur `width`.
pub(crate) fn box_line(width: usize, box_width: usize, text: &str, style: Style) -> Line<'static> {
    let content_width = box_width.saturating_sub(2 + 2 * BOX_PAD).max(1);
    let right = width.saturating_sub(BOX_MARGIN + box_width);
    let border = Style::default().fg(BOX_BORDER).bg(BOX_BG);
    let spans = vec![
        Span::styled(" ".repeat(BOX_MARGIN), Style::default().bg(BASE_BG)),
        Span::styled("│", border),
        Span::styled(" ".repeat(BOX_PAD), style),
        Span::styled(pad_to(text, content_width), style),
        Span::styled(" ".repeat(BOX_PAD), style),
        Span::styled("│", border),
        Span::styled(" ".repeat(right), Style::default().bg(BASE_BG)),
    ];
    Line::from(spans)
}

/// Anzeigebreite eines Strings in Zellen.
pub(crate) fn disp_width(s: &str) -> usize {
    s.chars().map(char_w).sum()
}

/// Text auf genau `width` Zellen bringen: Vordere Zeichen übernehmen (führende
/// Whitespaces bleiben erhalten), hinten auffüllen bzw. hart abschneiden.
pub(crate) fn pad_to(text: &str, width: usize) -> String {
    // Defensiv Tabs Tab-Stop-bewusst expandieren (Aufrufer liefern i. d. R.
    // bereits expandierten Text aus `wrap_preformatted`), damit sie sichtbar
    // gerendert und korrekt vermessen werden statt von Ratatui verworfen zu
    // werden. Expansion läuft VOR der Messung → Mess- und Renderbreite stimmen.
    let mut out = String::new();
    let mut w = 0usize;
    for c in text.chars() {
        if c == '\t' {
            let n = TAB_WIDTH - (w % TAB_WIDTH);
            for _ in 0..n {
                if w >= width {
                    break;
                }
                out.push(' ');
                w += 1;
            }
        } else {
            let cw = char_w(c);
            if w + cw > width {
                break;
            }
            out.push(c);
            w += cw;
        }
    }
    while w < width {
        out.push(' ');
        w += 1;
    }
    out
}

/// Bricht Text zeilengetreu (Konsolen-Ausgabe): Zeilen bleiben erhalten,
/// führende Whitespaces werden nicht entfernt; zu lange Zeilen werden hart an
/// Zeichen-Grenzen auf die Breite `width` umgebrochen.
///
/// Wagenrückläufe (`\r`) wirken wie auf einem Terminal: Der Schreib-Cursor
/// springt an den Zeilenanfang, folgende Zeichen überschreiben die Zeile.
/// Fortschritts-Zeilen („Build 1/3\rBuild 2/3\rBuild 3/3“) erscheinen so nur mit
/// ihrem aktuellen Stand – nicht als Reste aller Zwischenstände.
pub(crate) fn wrap_preformatted(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        // Terminal-Semantik: `\r` setzt Spalte 0, dann überschreiben; `\t`
        // springt auf die nächste Tab-Stop-Spalte (sichtbar, als Leerzeichen).
        let mut cells: Vec<char> = Vec::new();
        let mut col = 0usize;
        for c in line.chars() {
            if c == '\r' {
                col = 0;
            } else if c == '\t' {
                let n = TAB_WIDTH - (col % TAB_WIDTH);
                for _ in 0..n {
                    if col < cells.len() {
                        cells[col] = ' ';
                    } else {
                        cells.push(' ');
                    }
                    col += 1;
                }
            } else if col < cells.len() {
                cells[col] = c;
                col += 1;
            } else {
                cells.push(c);
                col += 1;
            }
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for c in cells {
            let cw = char_w(c);
            if cur_w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(c);
            cur_w += cw;
        }
        out.push(cur);
    }
    out
}

/// Eigenständiger Text-Block für eine chronologisch verortete Äußerung des
/// Modells (Zwischen-Statement vor einem Tool oder die finale Antwort).
pub(crate) fn text_block(text: &str, width: usize, mode: SymbolMode) -> ChatBlock {
    let logical = decorate_emphasis(decorate_symbols(logical_lines(text), mode));
    ChatBlock {
        lines: wrap_markdown(&logical, width, PAD),
        bg: None,
        gap: 0,
        is_tool: false,

    }
}

/// Aufklappbares Gedanken-Element, zart in Grau: „▸/▾ Gedanken“-Kopfzeile,
/// zusammengeklappt zusätzlich mit der Gedanken-Dauer als Hinweis.
pub(crate) fn thoughts_block(
    reasoning: &str,
    open: bool,
    duration: Option<u64>,
    width: usize,
    streaming: bool,
    mode: SymbolMode,
    block_tokens: Option<u64>,
) -> ChatBlock {
    let gray = Style::default().fg(MUTED);
    let header = match (open, duration, streaming) {
        (true, _, _) => "▾ Gedanken".to_string(),
        (false, Some(ms), _) => format!("▸ Gedanken · {}", fmt_duration(ms)),
        (false, None, true) => "▸ Gedanken…".to_string(),
        (false, None, false) => "▸ Gedanken".to_string(),
    };
    // Token-Annotation an den Header anhängen
    let header = if let Some(tokens) = block_tokens {
        format!("{header} · ~{tokens}T")
    } else {
        header
    };
    let mut logical = vec![Line::from(Span::styled(
        header,
        gray.add_modifier(Modifier::ITALIC),
    ))];
    if open {
        let mut body = Vec::new();
        render_markdown(reasoning, &mut body);
        for line in &mut body {
            for span in &mut line.spans {
                span.style = span.style.fg(MUTED);
            }
        }
        logical.extend(body);
    }
    ChatBlock {
        lines: wrap_markdown(&decorate_symbols(logical, mode), width, THOUGHT_INDENT),
        bg: None,
        gap: 0,
        is_tool: false,

    }
}

/// Dauer lesbar formatieren (z. B. „340 ms“, „52 s“, „2 m 05 s“).
pub(crate) fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0).replace('.', ",")
    } else {
        format!("{} m {:02} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}



/// Umrandete Box für einen `edit`-Aufruf: Kopfzeile `✎ <label>` (z. B.
/// `edit a.c +1 -1`) und darunter die Diff-Zeilenpaare zweispaltig (alte Zeilen
/// links, neue rechts). Gelöschte Zeilen rot, hinzugefügte grün; die geänderten
/// Zeichen innerhalb der Zeile kräftiger. Zugeklappt (`open`) wird auf
/// `DIFF_PREVIEW_ROWS` gekürzt.
pub(crate) fn diff_block(
    label: &str,
    ok: bool,
    diff: &crate::diff::DiffInfo,
    width: usize,
    open: bool,
) -> ChatBlock {
    let box_width = width.saturating_sub(2 * BOX_MARGIN).max(6);
    let border = Style::default().fg(BOX_BORDER).bg(BOX_BG);
    let header_style = if ok {
        Style::default().fg(MUTED).bg(BOX_BG)
    } else {
        Style::default().fg(ERROR_DIM).bg(BOX_BG)
    };
    let content_style = Style::default().fg(BOX_TEXT).bg(BOX_BG);

    let max_num = diff
        .rows
        .iter()
        .flat_map(|r| [r.old_num, r.new_num])
        .flatten()
        .max()
        .unwrap_or(0);
    let num_w = max_num.to_string().len().max(2);
    let two_col = box_width >= 44;

    let mut rows: Vec<&crate::diff::DiffRow> = diff.rows.iter().collect();
    let truncated = !open && rows.len() > DIFF_PREVIEW_ROWS;
    if truncated {
        rows.truncate(DIFF_PREVIEW_ROWS);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(box_edge(width, box_width, border, true));
    for h in wrap_preformatted(&format!("✎ {label}"), box_width.saturating_sub(2)) {
        lines.push(box_line(width, box_width, &h, header_style));
    }
    for row in &rows {
        lines.extend(diff_row_lines(
            width,
            box_width,
            num_w,
            row,
            two_col,
            content_style,
        ));
    }
    if truncated {
        lines.push(box_line(
            width,
            box_width,
            "…",
            Style::default().fg(MUTED).bg(BOX_BG),
        ));
    }
    lines.push(box_edge(width, box_width, border, false));

    ChatBlock {
        lines,
        bg: None,
        gap: 0,
        is_tool: false,
    }
}

/// Stil-Kombination für eine Diff-Zelle (eine Seite eines Zeilenpaars).
pub(crate) struct CellStyle {
    pub(crate) marker: char,
    pub(crate) base: Style,
    pub(crate) mark: Style,
    pub(crate) num_style: Style,
}

/// Baut die Zeilen für ein Diff-Zeilenpaar. Zweispaltig: eine Zeile mit beiden
/// Seiten; bei schmalen Boxen zwei einspaltige Zeilen mit `-`/`+`-Markern
/// (Kontext erscheint dann nur einmal).
pub(crate) fn diff_row_lines(
    width: usize,
    box_width: usize,
    num_w: usize,
    row: &crate::diff::DiffRow,
    two_col: bool,
    content_style: Style,
) -> Vec<Line<'static>> {
    let content_inner = box_width.saturating_sub(2 + 2 * BOX_PAD).max(2);
    let right = width.saturating_sub(BOX_MARGIN + box_width);
    let border = Style::default().fg(BOX_BORDER).bg(BOX_BG);

    let del = CellStyle {
        marker: '-',
        base: Style::default().fg(DIFF_DEL_FG).bg(DIFF_DEL_BG),
        mark: Style::default().fg(DIFF_DEL_FG).bg(DIFF_DEL_MARK_BG),
        num_style: Style::default().fg(DIFF_DEL_FG).bg(DIFF_DEL_BG),
    };
    let add = CellStyle {
        marker: '+',
        base: Style::default().fg(DIFF_ADD_FG).bg(DIFF_ADD_BG),
        mark: Style::default().fg(DIFF_ADD_FG).bg(DIFF_ADD_MARK_BG),
        num_style: Style::default().fg(DIFF_ADD_FG).bg(DIFF_ADD_BG),
    };
    let ctx = CellStyle {
        marker: ' ',
        base: content_style,
        mark: content_style,
        num_style: content_style,
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    let make_line = |cells: Vec<(Vec<Span<'static>>, usize)>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled(
            " ".repeat(BOX_MARGIN),
            Style::default().bg(BASE_BG),
        ));
        spans.push(Span::styled("│", border));
        spans.push(Span::styled(" ".repeat(BOX_PAD), content_style));
        let n_cells = cells.len();
        for (i, (cell_spans, _cell_w)) in cells.into_iter().enumerate() {
            spans.extend(cell_spans);
            if i + 1 < n_cells {
                spans.push(Span::styled("│", border));
            }
        }
        spans.push(Span::styled(" ".repeat(BOX_PAD), content_style));
        spans.push(Span::styled("│", border));
        spans.push(Span::styled(
            " ".repeat(right),
            Style::default().bg(BASE_BG),
        ));
        Line::from(spans)
    };

    if two_col {
        let gutter = 1;
        let left_w = (content_inner - gutter) / 2;
        let right_w = content_inner - gutter - left_w;
        let l_style = if row.old_mark.is_empty() { &ctx } else { &del };
        let r_style = if row.new_mark.is_empty() { &ctx } else { &add };
        // Jede Zelle kann sich über mehrere physische Zeilen erstrecken (lange
        // Zeilen brechen um statt abgeschnitten zu werden); nur ihre erste
        // Zeile trägt Gutter/Marker. Wir zeichnen so viele Line-Ebenen, wie die
        // längere Seite braucht, und füllen die kürzere mit leeren Zellen auf.
        let l_rows = diff_cell(
            row.old_num,
            &row.old_text,
            &row.old_mark,
            num_w,
            left_w,
            l_style,
        );
        let r_rows = diff_cell(
            row.new_num,
            &row.new_text,
            &row.new_mark,
            num_w,
            right_w,
            r_style,
        );
        let n = l_rows.len().max(r_rows.len());
        for i in 0..n {
            let l = l_rows
                .get(i)
                .cloned()
                .unwrap_or_else(|| empty_cell(left_w, num_w, l_style));
            let r = r_rows
                .get(i)
                .cloned()
                .unwrap_or_else(|| empty_cell(right_w, num_w, r_style));
            out.push(make_line(vec![(l, left_w), (r, right_w)]));
        }
    } else {
        let old_present = row.old_num.is_some();
        let new_present = row.new_num.is_some();
        let context = row.old_mark.is_empty() && row.new_mark.is_empty();
        if context {
            let rows = diff_cell(
                row.old_num.or(row.new_num),
                &row.old_text,
                &row.old_mark,
                num_w,
                content_inner,
                &ctx,
            );
            for r in rows {
                out.push(make_line(vec![(r, content_inner)]));
            }
        } else {
            if old_present {
                let rows = diff_cell(
                    row.old_num,
                    &row.old_text,
                    &row.old_mark,
                    num_w,
                    content_inner,
                    &del,
                );
                for r in rows {
                    out.push(make_line(vec![(r, content_inner)]));
                }
            }
            if new_present {
                let rows = diff_cell(
                    row.new_num,
                    &row.new_text,
                    &row.new_mark,
                    num_w,
                    content_inner,
                    &add,
                );
                for r in rows {
                    out.push(make_line(vec![(r, content_inner)]));
                }
            }
        }
    }
    out
}

/// Eine leere Diff-Zelle zum Ausgleich, wenn eine Seite weniger Umbruchzeilen
/// hat als die andere. Zeichnet dieselbe Gutter-Struktur wie eine Folgezeile
/// von [`diff_cell`] (`num_w` Leerraum + `│` + Marker + leere Füllung), damit
/// die Spalten auch an leeren Positionen optisch verbunden bleiben – mit
/// `│`-Trenner zwischen (leerer) Zeilennummer und (leerem) Inhalt.
pub(crate) fn empty_cell(cell_w: usize, num_w: usize, style: &CellStyle) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    spans.push(Span::styled(" ".repeat(num_w), style.num_style));
    spans.push(Span::styled("│", style.num_style));
    spans.push(Span::styled(format!("{} ", style.marker), style.base));
    let used = num_w + 3;
    spans.push(Span::styled(
        " ".repeat(cell_w.saturating_sub(used)),
        style.base,
    ));
    spans
}

/// Eine Zelle einer Diff-Zeile: Nummerngutter (`<nr>│`), `-`/`+`/` `-Marker und
/// der auf `cell_w` Zellen gefüllte Text mit hervorgehobenen Zeichenbereichen.
/// Lange Zeilen werden **umgebrochen** statt abgeschnitten: die erste Zeile
/// trägt Zeilennummer + `│` + Marker, Folgezeilen tragen Leerzeichen in der
/// Breite der Nummer + `│` + Marker (Inhalt beginnt danach).
pub(crate) fn diff_cell(
    num: Option<u64>,
    text: &str,
    marks: &[(usize, usize)],
    num_w: usize,
    cell_w: usize,
    style: &CellStyle,
) -> Vec<Vec<Span<'static>>> {
    // Der Vorspann (`num_w` Leerraum + `│` + Marker = `num_w + 3` Zellen) gilt
    // auf der ersten UND auf jeder Folgezeile. Das Umbruchlimit für den Inhalt
    // ist also auf allen Zeilen gleich `cell_w - (num_w + 3)` – sonst liefe eine
    // Folgezeile samt Vorspann um `num_w + 3` Zellen über die Zellbreite hinaus.
    let first_w = cell_w.saturating_sub(num_w + 3);
    let lines = marked_wrapped(text, marks, first_w, first_w);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::with_capacity(lines.len());
    for (idx, segs) in lines.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used;
        if idx == 0 {
            let num_str = num
                .map(|n| format!("{n:>num_w$}"))
                .unwrap_or_else(|| " ".repeat(num_w));
            spans.push(Span::styled(num_str, style.num_style));
        } else {
            // Fortgesetzte Zeile: gleiche Breite wie die Nummer, aber leer.
            spans.push(Span::styled(" ".repeat(num_w), style.num_style));
        }
        spans.push(Span::styled("│", style.num_style));
        spans.push(Span::styled(format!("{} ", style.marker), style.base));
        used = num_w + 3;
        for (seg, changed) in segs {
            let s = if changed { style.mark } else { style.base };
            spans.push(Span::styled(seg.clone(), s));
            used += disp_width(&seg);
        }
        spans.push(Span::styled(
            " ".repeat(cell_w.saturating_sub(used)),
            style.base,
        ));
        rows.push(spans);
    }
    rows
}

/// Bricht `text` in physische Diff-Zeilen: die erste bis `first_w`, jede
/// weitere bis `rest_w` Zellen (Anzeigebreite). Liefert pro Zeile die Segmente
/// mit gleicher Markierung. Die Markierungen sind **char-Indizes** (nicht
/// Bytes); ein Zeichen, das breiter als die Zeile ist, läuft über statt
/// gelöscht zu werden.
pub(crate) fn marked_wrapped(
    text: &str,
    marks: &[(usize, usize)],
    first_w: usize,
    rest_w: usize,
) -> Vec<Vec<(String, bool)>> {
    let mut lines: Vec<Vec<(String, bool)>> = Vec::new();
    let mut line: Vec<(String, bool)> = Vec::new();
    let mut w = 0usize;
    let mut first = true;
    for (ci, c) in text.chars().enumerate() {
        let cw = char_w(c);
        let cap = if first { first_w } else { rest_w };
        if w + cw > cap {
            lines.push(std::mem::take(&mut line));
            w = 0;
            first = false;
        }
        let changed = marks.iter().any(|&(s, e)| ci >= s && ci < e);
        match line.last_mut() {
            Some((seg, seg_changed)) if *seg_changed == changed => seg.push(c),
            _ => line.push((c.to_string(), changed)),
        }
        w += cw;
    }
    // Leerer Text bleibt eine (leere) Zeile; sonst die letzte angebrochene.
    if lines.is_empty() || !line.is_empty() {
        lines.push(line);
    }
    lines
}

