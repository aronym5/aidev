//! Markdown-Weiterverarbeitung für den Chat: Symbol-Dekoration,
//! wortweiser Umbruch und Tabellen-Layout. Die Funktionen bauen auf den
//! von `tui-markdown` gelieferten `Line`s auf und formen sie zu
//! bildschirmbreiten Zeilen.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{from_str_with_options, Options, StyleSheet};
use unicode_width::UnicodeWidthChar;

use super::{
    ACCENT, BASE_BG, BOX_BORDER, CODE_BG, CODE_FG, MUTED, PAD_R, SYM_ERR, SYM_MUTED, SYM_OK,
    SYM_WARN,
};
use crate::config::SymbolMode;

/// Stylesheet des Chat-Markdown-Renderings.
///
/// Überschriften werden im Akzentfarbton gerendert.
#[derive(Clone, Copy)]
struct ChatSheet;

impl StyleSheet for ChatSheet {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::new().fg(ACCENT).bold().underlined(),
            2 => Style::new().fg(ACCENT).bold(),
            3 => Style::new().fg(ACCENT).bold().italic(),
            _ => Style::new().fg(ACCENT).italic(),
        }
    }

    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    fn link(&self) -> Style {
        Style::new().fg(ACCENT).underlined()
    }

    fn code(&self) -> Style {
        Style::new().fg(Color::Rgb(129, 201, 149)).bg(CODE_BG) // Grün wie SYM_OK
    }
}

/// Hängt einen markdown-gerenderten Textinhalt an die Sammlung an.
pub(super) fn render_markdown<'a>(src: &'a str, out: &mut Vec<Line<'a>>) {
    let text = from_str_with_options(src, &Options::new(ChatSheet));
    out.extend(text.lines);
}

/// Markdown zu logischen Zeilen rendern.
pub(super) fn logical_lines<'a>(src: &'a str) -> Vec<Line<'a>> {
    let mut out = Vec::new();
    render_markdown(src, &mut out);
    out
}

/// Ersatzzeichen + semantische Farbe für Status-Emoji im Glyph-Modus.
///
/// `ℹ️` (U+2139) fehlt bewusst – das „i“ wird überall gut angezeigt und bleibt
/// unangetastet. Die Ersatzglyphen sind Breite 1 und in praktisch jeder
/// Terminal-Schriftart vorhanden, sodass Tabelle/Code konsistent layouten.
pub(super) fn symbol_replacement(c: char) -> Option<(String, Color)> {
    let colored = |glyph: &str, color: Color| Some((glyph.to_string(), color));
    match c {
        '\u{2705}' => colored("✓", SYM_OK),    // ✅
        '\u{2714}' => colored("✓", SYM_OK),    // ✔
        '\u{2611}' => colored("☑", SYM_OK),    // ☑️
        '\u{274C}' => colored("✗", SYM_ERR),   // ❌
        '\u{274E}' => colored("✗", SYM_ERR),   // ❎
        '\u{2B1C}' => colored("□", SYM_MUTED), // ⬜
        '\u{1F534}' => colored("●", SYM_ERR),  // 🔴
        '\u{1F7E2}' => colored("●", SYM_OK),   // 🟢
        '\u{1F7E1}' => colored("●", SYM_WARN), // 🟡
        '\u{26A0}' => colored("⚠", SYM_WARN),  // ⚠️
        '\u{2757}' => colored("!", SYM_ERR),   // ❗
        '\u{1F44D}' => colored("✓", SYM_OK),   // 👍
        '\u{1F44E}' => colored("✗", SYM_ERR),  // 👎
        '\u{2753}' => colored("?", SYM_WARN),  // ❓
        '\u{1F6A8}' => colored("▲", SYM_ERR),  // 🚨
        '\u{1F197}' => colored("OK", SYM_OK),  // 🆗
        '\u{1F4A1}' => colored("★", SYM_WARN), // 💡
        _ => None,
    }
}

/// Status-Emoji in den logischen Zeilen einfärben bzw. ersetzen – vor dem
/// Umbruch, damit Tabellen-/Code-Layout konsistente Breiten sieht.
/// Ein ggf. folgender Variationsselektor (U+FE0F) wird beim Ersetzen mit
/// entsorgt; vorhandene Modifier (bold/italic) bleiben erhalten.
pub(super) fn decorate_symbols<'a>(lines: Vec<Line<'a>>, mode: SymbolMode) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| decorate_line(line, mode))
        .collect()
}

pub(super) fn decorate_line<'a>(line: Line<'a>, mode: SymbolMode) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let style = span.style;
        let chars: Vec<char> = span.content.chars().collect();
        let mut run = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match symbol_replacement(c) {
                Some((repl, color)) => {
                    if !run.is_empty() {
                        out.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    if chars.get(i + 1).copied() == Some('\u{FE0F}') {
                        i += 1; // Variationsselektor mitfressen
                    }
                    let text = if mode == SymbolMode::Emoji {
                        c.to_string()
                    } else {
                        repl
                    };
                    out.push(Span::styled(text, style.fg(color)));
                }
                None => run.push(c),
            }
            i += 1;
        }
        if !run.is_empty() {
            out.push(Span::styled(run, style));
        }
    }
    Line::from(out)
}

/// Orange Farbe für fetten/kursiven Text (Hervorhebung).
const HIGHLIGHT_FG: Color = Color::Rgb(238, 198, 93); // Orange wie SYM_WARN

/// Fett- und kursiven Text in den logischen Zeilen zusätzlich orange einfärben.
pub(super) fn decorate_emphasis(lines: Vec<Line<'_>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let out: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| {
                    let is_emphasis = span.style.add_modifier.contains(
                        ratatui::style::Modifier::BOLD | ratatui::style::Modifier::ITALIC,
                    );
                    if is_emphasis {
                        Span::styled(span.content.to_string(), span.style.fg(HIGHLIGHT_FG))
                    } else {
                        Span::styled(span.content.to_string(), span.style)
                    }
                })
                .collect();
            Line::from(out)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Markdown-bewusster Umbruch: Tabellen/Code bleiben zeilengetreu (kein
// Wort-Umbruch), Listen/Zitate bekommen hängenden Einzug, Prosa wird wortweise
// umgebrochen.
// ---------------------------------------------------------------------------

/// Rendert logische Zeilen zu physischen Zeilen:
/// - Tabellen (tui-markdown zeichnet sie mit ┌ │ ├ └) bleiben zeilengetreu –
///   Rand- und Datenzeilen erhalten ihre Ausrichtung.
/// - Codeblöcke (` ``` `…` ``` `) werden als Band mit eigenem Hintergrund
///   dargestellt; auch ohne Syntax-Highlighting (z. B. `toml`) sind sie als
///   Codeblock erkennbar; Whitespace bleibt erhalten, Umbruch ist hart.
/// - Absätze mit Marker (`- `, `1. `, `    - `) oder Zitat (`> `) rücken beim
///   Umbruch unter den Text ein (hängender Einzug).
pub(super) fn wrap_markdown<'a>(
    logical: &[Line<'a>],
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    // Symmetrischer Rand: rechts endet der Inhalt `PAD_R` Zellen vor dem Rand.
    let text_w = width.saturating_sub(PAD_R).max(1);
    let budget = text_w.saturating_sub(indent).max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut i = 0;
    while i < logical.len() {
        let line = &logical[i];
        let first = line.spans.first().map(|s| s.content.as_ref()).unwrap_or("");
        let trimmed = first.trim_start();
        // Code-Fence (```lang / ```): reine Rendervorschrift wie `**` – wird
        // nicht angezeigt, öffnet/schließt aber den Code-Bereich.
        if trimmed.starts_with("```") {
            in_code = !in_code;
            i += 1;
            continue;
        }
        if in_code {
            for l in code_content_line(line, budget) {
                out.push(fill_band(l, text_w, indent, CODE_BG));
            }
            i += 1;
            continue;
        }
        // Thematischer Trenner (---, ***, ___): volle Breite, dezent.
        if is_thematic_break(line) {
            out.push(hr_line(text_w, indent));
            i += 1;
            continue;
        }
        if is_table_line(line) {
            let start = i;
            while i < logical.len() && is_table_line(&logical[i]) {
                i += 1;
            }
            out.extend(render_table(&logical[start..i], text_w, indent));
            continue;
        }
        out.extend(wrap_paragraph(line, text_w, indent));
        i += 1;
    }
    out
}

/// Thematischer Trenner (CommonMark: drei oder mehr `-`, `*` oder `_`).
/// tui-markdown lässt ihn als kurze „---"-Zeile stehen.
pub(super) fn is_thematic_break(line: &Line) -> bool {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let t = text.trim();
    t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_'))
}

/// Volle-Breite-Trennlinie (`─` über die verfügbare Breite, dezent grau).
pub(super) fn hr_line(width: usize, indent: usize) -> Line<'static> {
    let budget = width.saturating_sub(indent).max(1);
    let dashes: String = "─".repeat(budget);
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    spans.push(Span::styled(dashes, Style::default().fg(MUTED)));
    Line::from(spans)
}

/// Tabellenzeile? tui-markdown zeichnet Tabellen mit ┌ │ ├ └-Rahmen.
pub(super) fn is_table_line(line: &Line) -> bool {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.trim_start().starts_with(['┌', '│', '├', '└'])
}

pub(super) fn starts_with_char(line: &Line, c: char) -> bool {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    text.trim_start().starts_with(c)
}

/// Zell-Ausrichtung einer Tabellenspalte.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum CellAlign {
    Left,
    Center,
    Right,
}

/// Eine Tabellen-Zelle (Inhalt ohne Zellrand-Padding).
#[derive(Clone)]
pub(super) struct CellData {
    chars: Vec<(char, Style)>,
    lead: usize,
    trail: usize,
    pad_style: Style,
}

// ---------------------------------------------------------------------------
// Tabellen-Neuanordnung: tui-markdown liefert eine fertig ausgerichtete Tabelle
// mit fixen Spaltenbreiten. Passt sie in die verfügbare Breite, bleibt sie
// kompakt (natürliche Spaltenbreiten). Passt sie nicht (langer Eintrag), werden
// nur die breitesten Spalten verkleinert und lange Zelleninhalte mehrzeilig
// (innerhalb ihrer Zelle) umgebrochen. Die Rahmenlinien laufen durch.
// ---------------------------------------------------------------------------

/// Zählt die Spalten einer Tabelle aus dem oberen Rand (`┌──┬──┐`).
pub(super) fn table_cols(top: &Line) -> usize {
    let text: String = top.spans.iter().map(|s| s.content.as_ref()).collect();
    text.chars()
        .filter(|c| matches!(c, '┌' | '┬' | '┐'))
        .count()
        .saturating_sub(1)
        .max(1)
}

/// Liest die Zellen einer Inhaltszeile (`│ a │ b │`) aus.
pub(super) fn table_row_cells<'a>(line: &'a Line<'a>, cols: usize) -> Vec<CellData> {
    let flat = flatten(line);
    let bars: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|&(_, (c, _))| *c == '│')
        .map(|(i, _)| i)
        .collect();
    let mut cells = Vec::with_capacity(cols);
    for i in 0..cols {
        let lo = bars.get(i).copied().unwrap_or(0).saturating_add(1);
        let hi = bars.get(i + 1).copied().unwrap_or(flat.len());
        let slice = &flat[lo.min(hi).min(flat.len())..hi.min(flat.len())];
        let mut lead = 0;
        while lead < slice.len() && slice[lead].0.is_whitespace() {
            lead += 1;
        }
        let mut trail = 0;
        while trail + lead < slice.len() && slice[slice.len() - 1 - trail].0.is_whitespace() {
            trail += 1;
        }
        let content = slice[lead..slice.len() - trail].to_vec();
        let pad_style = slice.first().map(|&(_, s)| s).unwrap_or_default();
        cells.push(CellData {
            chars: content,
            lead,
            trail,
            pad_style,
        });
    }
    cells
}

/// Klemmt eine Zellen-Zeile auf die Zellbreite `width` (Ausrichtung). Spalten
/// sind breiter als ihr längster Inhalt (je 1 Randspalt links und rechts);
/// Zellinhalte werden auf die innere Breite hart beschnitten.
pub(super) fn pad_cell(
    chars: &[(char, Style)],
    width: usize,
    align: CellAlign,
    pad_style: Style,
) -> Vec<(char, Style)> {
    let inner = width.saturating_sub(2);
    let mut content: Vec<(char, Style)> = Vec::new();
    let mut cw = 0usize;
    for &(c, s) in chars {
        let cw0 = char_w(c);
        if cw + cw0 > inner && !content.is_empty() {
            break;
        }
        content.push((c, s));
        cw += cw0;
    }
    let slack = inner.saturating_sub(cw);
    let (lead, trail) = match align {
        CellAlign::Left => (0, slack),
        CellAlign::Center => (slack / 2, slack - slack / 2),
        CellAlign::Right => (slack, 0),
    };
    let mut out = Vec::with_capacity(width);
    if width > 0 {
        out.push((' ', pad_style));
    }
    for _ in 0..lead {
        out.push((' ', pad_style));
    }
    out.extend(content);
    for _ in 0..trail {
        out.push((' ', pad_style));
    }
    while out.iter().map(|(c, _)| char_w(*c)).sum::<usize>() < width {
        out.push((' ', pad_style));
    }
    out
}

/// Einfache Rahmenzeile (`┌───┬───┐` / `├───┼───┤` / `└───┴───┘`).
pub(super) fn border_row(
    start: char,
    mid: char,
    end: char,
    widths: &[usize],
    border: Style,
) -> Line<'static> {
    let mut s = String::new();
    s.push(start);
    for (i, w) in widths.iter().enumerate() {
        for _ in 0..*w {
            s.push('─');
        }
        s.push(if i + 1 == widths.len() { end } else { mid });
    }
    Line::from(Span::styled(s, border))
}

/// Rendert eine Inhaltszeile; lange Zellen werden im Rahmen ihrer Spalte
/// mehrzeilig. Rückgabe: physische Zeilen der Tabelle (ohne PAD).
pub(super) fn render_table_row(
    cells: &[CellData],
    widths: &[usize],
    aligns: &[CellAlign],
    border: Style,
) -> Vec<Line<'static>> {
    let mut col_lines: Vec<Vec<Vec<(char, Style)>>> = Vec::with_capacity(cells.len());
    let mut height = 1usize;
    for (cell, (&w, &align)) in cells.iter().zip(widths.iter().zip(aligns.iter())) {
        let chunks = wrap_words(&cell.chars, w);
        let lines: Vec<Vec<(char, Style)>> = if chunks.is_empty() {
            vec![pad_cell(&[], w, align, cell.pad_style)]
        } else {
            chunks
                .iter()
                .map(|c| pad_cell(c, w, align, cell.pad_style))
                .collect()
        };
        height = height.max(lines.len());
        col_lines.push(lines);
    }
    let mut out = Vec::with_capacity(height);
    for r in 0..height {
        let mut spans = vec![Span::styled("│", border)];
        for ((lines, cell), (&w, &align)) in col_lines
            .iter()
            .zip(cells.iter())
            .zip(widths.iter().zip(aligns.iter()))
        {
            let line_chars = lines.get(r).cloned().unwrap_or_else(|| {
                // Zelle ist kürzer als die Zeilenhöhe: leere Zelle.
                pad_cell(&[], w, align, cell.pad_style)
            });
            spans.extend(spans_of(&line_chars).spans);
            spans.push(Span::styled("│", border));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Berechnet die finalen Spaltenbreiten: Passt die Tabelle in die verfügbare
/// Breite, behält jede Spalte ihre natürliche Breite (die Tabelle bleibt
/// kompakt). Überschreitet sie die Breite, wird wiederholt nur die jeweils
/// breiteste Spalte verkleinert (lange Zellen brechen dann mehrzeilig um),
/// schmale Spalten bleiben erhalten.
pub(super) fn table_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    let fixed = natural.len() + 1; // Rahmenspalten │
    let avail = budget.saturating_sub(fixed);
    let sum: usize = natural.iter().sum();
    if sum <= avail {
        // Passt: kompakt lassen.
        return natural.to_vec();
    }
    // Zu breit: wiederholt die breiteste Spalte abbauen (niemals unter 1).
    let mut widths = natural.to_vec();
    let mut excess = sum - avail;
    while excess > 0 {
        let mut idx = 0;
        for (i, w) in widths.iter().enumerate() {
            if widths[i] > widths[idx] {
                idx = i;
            }
            let _ = w;
        }
        if widths[idx] <= 1 {
            break; // alles bei Mindestbreite – Rest akzeptieren
        }
        widths[idx] -= 1;
        excess -= 1;
    }
    widths
}

/// Erkennt die Spalten-Ausrichtung aus den Zellen mit dem größten Füllraum
/// (die Zellränder tragen die Ausrichtung von tui-markdown).
pub(super) fn table_aligns(
    header: &[Vec<CellData>],
    data: &[Vec<CellData>],
    cols: usize,
) -> Vec<CellAlign> {
    let candidates: Vec<Vec<CellData>> = header.iter().chain(data.iter()).cloned().collect();
    let mut aligns = vec![CellAlign::Left; cols];
    for (c, target) in aligns.iter_mut().enumerate() {
        let mut best: Option<(usize, isize)> = None;
        for row in &candidates {
            if let Some(cell) = row.get(c) {
                let slack = cell.lead + cell.trail;
                if best.is_none_or(|(bs, _)| slack > bs) {
                    let d = cell.lead as isize - cell.trail as isize;
                    best = Some((slack, d));
                }
            }
        }
        *target = match best {
            Some((_, d)) if d > 0 => CellAlign::Right,
            Some((_, d)) if d < 0 => CellAlign::Left,
            Some((slack, _)) if slack > 0 => CellAlign::Center,
            _ => CellAlign::Left,
        };
    }
    aligns
}

/// Zeichnet einen Tabellenblock aus den gerenderten tui-markdown-Zeilen neu
/// auf: kompakt bei passender Breite, Zell-Umbruch bei Überbreite, weitergezogene
/// Rahmenlinien.
pub(super) fn render_table<'a>(
    block: &[Line<'a>],
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    let budget = width.saturating_sub(indent).max(1);
    let border = Style::default().fg(BOX_BORDER);
    let Some(top_idx) = block.iter().position(|l| starts_with_char(l, '┌')) else {
        // Kein Rahmen ersichtlich: notfalls unangetastet übernehmen.
        return block
            .iter()
            .flat_map(|l| wrap_hard(l, budget))
            .map(|l| indent_line(l, indent))
            .collect();
    };
    let cols = table_cols(&block[top_idx]);
    let delims: Vec<usize> = block
        .iter()
        .enumerate()
        .filter(|(_, l)| starts_with_char(l, '├'))
        .map(|(i, _)| i)
        .collect();
    let header_rows: Vec<Vec<CellData>> = block
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            *i > top_idx && starts_with_char(l, '│') && delims.first().is_none_or(|d| i < d)
        })
        .map(|(_, l)| table_row_cells(l, cols))
        .collect();
    let data_rows: Vec<Vec<CellData>> = block
        .iter()
        .enumerate()
        .filter(|(i, l)| starts_with_char(l, '│') && delims.first().is_some_and(|d| i > d))
        .map(|(_, l)| table_row_cells(l, cols))
        .collect();

    // Natürliche Breiten je Spalte: längster Inhalt + je 1 Randspalt.
    let mut natural = vec![0usize; cols];
    for row in header_rows.iter().chain(data_rows.iter()) {
        for (i, cell) in row.iter().enumerate() {
            let w: usize = cell.chars.iter().map(|(c, _)| char_w(*c)).sum();
            natural[i.min(cols - 1)] = natural[i.min(cols - 1)].max(w + 2);
        }
    }
    let aligns = table_aligns(&header_rows, &data_rows, cols);
    let widths = table_widths(&natural, budget);

    let render = |rows: &[Vec<CellData>]| {
        rows.iter()
            .flat_map(|r| render_table_row(r, &widths, &aligns, border))
            .collect::<Vec<_>>()
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(border_row('┌', '┬', '┐', &widths, border));
    out.extend(render(&header_rows));
    out.push(border_row('├', '┼', '┤', &widths, border));
    out.extend(render(&data_rows));
    out.push(border_row('└', '┴', '┘', &widths, border));
    out.into_iter().map(|l| indent_line(l, indent)).collect()
}

/// Bricht eine formatierte Zeile hart an `width` Zellen um, ohne Whitespace zu
/// verlieren oder Wörter zu trennen; passt sie, bleibt sie ganz.
pub(super) fn wrap_hard<'a>(line: &Line<'a>, width: usize) -> Vec<Line<'static>> {
    if line.width() == 0 {
        return vec![Line::default()];
    }
    if line.width() <= width {
        let mut spans = Vec::with_capacity(line.spans.len());
        for s in &line.spans {
            spans.push(Span::styled(s.content.to_string(), s.style));
        }
        return vec![Line::from(spans).style(line.style)];
    }
    let chars = flatten(line);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut chunk: Vec<(char, Style)> = Vec::new();
    let mut w = 0usize;
    for &(c, s) in &chars {
        let cw = char_w(c);
        if w + cw > width && !chunk.is_empty() {
            out.push(spans_of(&chunk));
            chunk.clear();
            w = 0;
        }
        chunk.push((c, s));
        w += cw;
    }
    if !chunk.is_empty() {
        out.push(spans_of(&chunk));
    }
    out
}

/// Inhalt einer Codeblock-Zeile: Code-Band; ohne Syntax-Farben bleibt
/// `CODE_FG`, vorhandene Farben (z. B. rust) bleiben stehen.
pub(super) fn code_content_line<'a>(line: &Line<'a>, budget: usize) -> Vec<Line<'static>> {
    let mut styled = Line::default();
    for s in &line.spans {
        let mut st = s.style;
        if st.bg.is_none() {
            st.bg = Some(CODE_BG);
        }
        if st.fg.is_none() {
            st.fg = Some(CODE_FG);
        }
        styled.spans.push(Span::styled(s.content.clone(), st));
    }
    wrap_hard(&styled, budget)
}

/// Setzt eine physische Zeile als Band: linker Rand bleibt Grundhintergrund,
/// Inhalt und Auffüllung rechts bis `width` tragen `bg`.
pub(super) fn fill_band<'a>(line: Line<'a>, width: usize, indent: usize, bg: Color) -> Line<'a> {
    let rest = width.saturating_sub(indent + line.width());
    let mut spans = vec![Span::styled(
        " ".repeat(indent),
        Style::default().bg(BASE_BG),
    )];
    spans.extend(line.spans);
    spans.push(Span::styled(" ".repeat(rest), Style::default().bg(bg)));
    Line::from(spans)
}

/// Setzt eine physische Zeile mit Einzug (ohne Hintergrund).
pub(super) fn indent_line<'a>(mut line: Line<'a>, indent: usize) -> Line<'a> {
    line.spans.insert(0, Span::raw(" ".repeat(indent)));
    line
}

/// Hängender Einzug einer Absatzzeile: Breite + Marker-Spans des Präfixes
/// (Aufzählungs-Marker wie `- `, `1. `, Zitat `> ` oder reine Einrückung).
/// Die Marker werden als eigene (owned) Spans zurückgegeben.
///
/// Wichtig: Es werden NUR echte Marker erkannt. `tui-markdown` liefert
/// Listen-/Zitat-Marker immer als eigenes Span (z. B. `"- "`, `"1. "`, `">"`),
/// Absatz-Text davor kann dagegen beliebig mit einem Leerzeichen enden (vor
/// einem Inline-Element wie `code`, `**fett**` oder `*kursiv*`). Würde man
/// jedes Span mit End-Leerzeichen als Marker werten, rückten alle
/// Fortsetzungszeilen solcher Absätze auf die Spalte des Inline-Elements ein
/// („Code-Wort-Einzug") – bei weit rechts stehendem Element bliebe nur noch
/// eine winzige Zeilenbreite übrig.
pub(super) fn split_prefix<'a>(line: &'a Line<'a>) -> (usize, Vec<Span<'static>>) {
    let Some(first) = line.spans.first() else {
        return (0, Vec::new());
    };
    let c = first.content.as_ref();
    // Blockzitat: „>“ allein, gefolgt von einem Leerzeichen.
    if c == ">" {
        let mut spans = vec![Span::styled(first.content.to_string(), first.style)];
        if line
            .spans
            .get(1)
            .map(|s| s.content.as_ref() == " ")
            .unwrap_or(false)
        {
            let s = &line.spans[1];
            spans.push(Span::styled(s.content.to_string(), s.style));
        }
        return (2, spans);
    }
    // Nur echte Marker (bzw. reine Whitespace-Einrückung) – nicht beliebiger
    // Text, der zufällig mit einem Leerzeichen endet.
    if is_marker_prefix(c) {
        return (
            first.width(),
            vec![Span::styled(first.content.to_string(), first.style)],
        );
    }
    (0, Vec::new())
}

/// Erkennt einen echten Einzug-Präfix: ungeordnete Liste (`-`, `*`, `+`),
/// geordnete Liste (`1.`, `12)`, …) – jeweils optional mit folgendem
/// Whitespace – oder reine Whitespace-Einrückung.
pub(super) fn is_marker_prefix(content: &str) -> bool {
    if content.is_empty() {
        return false;
    }
    if content.chars().all(char::is_whitespace) {
        return true;
    }
    let t = content.trim();
    let bytes = t.as_bytes();
    match bytes[0] {
        b'-' | b'*' | b'+' => t.len() == 1 || t[1..].chars().all(|c| c.is_whitespace()),
        b'0'..=b'9' => {
            let mut i = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            i > 0
                && i < bytes.len()
                && matches!(bytes[i], b'.' | b')')
                && t[i + 1..].chars().all(|c| c.is_whitespace())
        }
        _ => false,
    }
}

/// Wortweiser Umbruch einer Absatz-/Listen-Zeile mit hängendem Einzug: Die
/// erste physische Zeile trägt den Marker (`1. `, `- `, Einrückung, `> `),
/// Fortsetzungszeilen rücken unter den Text ein.
pub(super) fn wrap_paragraph<'a>(
    line: &'a Line<'a>,
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    if line.width() == 0 {
        return vec![Line::default()];
    }
    let line_style = line.style;
    let (prefix_w, marker) = split_prefix(line);
    let budget = width.saturating_sub(indent + prefix_w).max(1);
    let mut rest = flatten(line);
    let prefix_len: usize = marker.iter().map(|s| s.content.chars().count()).sum();
    if prefix_len > rest.len() {
        rest.clear();
    } else {
        rest.drain(0..prefix_len);
    }
    let content_style = rest.first().map(|&(_, s)| s).unwrap_or(line_style);
    let chunks = wrap_words(&rest, budget);
    let pad = " ".repeat(indent);
    let cont = " ".repeat(prefix_w);
    let mut out = Vec::with_capacity(chunks.len().max(1));
    if chunks.is_empty() {
        let mut spans = vec![Span::raw(pad)];
        spans.extend(marker.iter().cloned());
        out.push(Line::from(spans).style(line_style));
        return out;
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let mut spans = vec![Span::raw(pad.clone())];
        if i == 0 {
            spans.extend(marker.iter().cloned());
        } else {
            spans.push(Span::styled(cont.clone(), line_style.patch(content_style)));
        }
        spans.extend(spans_of(chunk).spans);
        out.push(Line::from(spans).style(line_style));
    }
    out
}

/// Trennzeichen, hinter denen ein zu langes Wort bevorzugt getrennt wird –
/// das Zeichen bleibt sichtbar, die Folgezeile beginnt erst danach.
pub(super) fn is_break_separator(c: char) -> bool {
    matches!(c, '-' | '/' | '.')
}

/// Bricht ein einzelnes, breiteres als `width` Wort in Chunks:
/// 1. Nach dem letzten Trennzeichen (`-`, `/`, `.`) im füllbaren Bereich
///    trennen – das Zeichen bleibt am Zeilenende sichtbar;
/// 2. gibt es kein passendes Trennzeichen, blind an der Zeilengrenze brechen.
///    Die Folgezeile wird dabei normal gefüllt (keine Ein-Zeichen-Zeilen).
pub(super) fn split_long_word(word: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let n = word.len();
    let mut out: Vec<Vec<(char, Style)>> = Vec::new();
    if n == 0 {
        return out;
    }
    let mut start = 0usize;
    while start < n {
        // Maximal füllbarer Bereich ab `start` – mindestens ein Zeichen, damit
        // auch ein einzelnes (sehr breites) Zeichen eine eigene Zeile bekommt.
        let mut end = start;
        let mut w = 0usize;
        while end < n {
            let cw = char_w(word[end].0);
            if w + cw > width && end > start {
                break;
            }
            w += cw;
            end += 1;
        }
        if end >= n {
            out.push(word[start..].to_vec());
            break;
        }
        // Letztes Trennzeichen im füllbaren Bereich (vor `end`): dahinter
        // trennen, das Zeichen bleibt sichtbar.
        let split = (start..end).rev().find(|&i| is_break_separator(word[i].0));
        match split {
            Some(i) => {
                out.push(word[start..=i].to_vec());
                start = i + 1;
            }
            None => {
                out.push(word[start..end].to_vec());
                start = end;
            }
        }
    }
    out
}

/// Prüft ob ein Style einen Hintergrund gesetzt hat (Code-Schreibweise,
/// fetter Text o. Ä.). Wird benutzt, um Whitespace zwischen Code-Wörtern
/// als „bedeutsam" zu erkennen und beizubehalten.
fn has_bg(s: Style) -> bool {
    s.bg.is_some()
}

/// Bricht eine Zeichenfolge (mit Stilen) wortweise an `width` Zellen um: nur an
/// Whitespaces; zu lange Wörter werden mit [`split_long_word`] geteilt.
///
/// Whitespaces, die zu einem Code-Span gehören (erkennbar am Hintergrund),
/// werden beibehalten – sowohl ihre Anzahl als auch ihr Style.  Trenn-Spaces
/// zwischen Wörtern sind immer neutral (`Style::default()`).
pub(super) fn wrap_words(chars: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    if chars.is_empty() {
        return Vec::new();
    }

    // ── Phase 1: In Segmente gleichen Typs aufteilen (Whitespace-Run vs.
    //    Nicht-Whitespace-Run). ────────────────────────────────────────────
    let mut segments: Vec<Vec<(char, Style)>> = Vec::new();
    let mut seg: Vec<(char, Style)> = Vec::new();
    let mut seg_is_ws: Option<bool> = None;
    for &(c, s) in chars {
        let is_ws = c.is_whitespace();
        if seg_is_ws == Some(is_ws) {
            seg.push((c, s));
        } else {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
            seg.push((c, s));
            seg_is_ws = Some(is_ws);
        }
    }
    if !seg.is_empty() {
        segments.push(seg);
    }

    // ── Phase 2: Segmente zu „Wörtern" zusammenfassen. ───────────────────
    //    Whitespace mit eigenem Hintergrund (also Teil eines Code-Spans,
    //    wie bei `a b`) wird am vorherigen Wort angeheftet und damit
    //    beibehalten – sowohl Anzahl als auch Style bleiben erhalten.
    //    Whitespace ohne Hintergrund (zwischen getrennten Code-Spans wie
    //    `a` `b`) wird verworfen und später als einzelnes Leerzeichen mit
    //    dem Style des vorherigen Wortes eingefügt.
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    for segment in &segments {
        let is_ws = segment[0].0.is_whitespace();
        if !is_ws {
            words.push(segment.clone());
        } else {
            // Whitespace-Segment: nur beibehalten, wenn es selbst einen
            // Hintergrund hat (also Teil eines Code-Spans ist, z.B.
            // „a b" als ein Span). Whitespace ohne Hintergrund (z.B.
            // zwischen `a` und `b`) wird verworfen.
            let ws_has_bg = has_bg(segment[0].1);
            if ws_has_bg {
                if let Some(last) = words.last_mut() {
                    last.extend(segment.iter().cloned());
                }
            }
            // Sonst: Whitespace verwerfen (wird als einzelnes Space
            // reingefügt).
        }
    }

    if words.is_empty() {
        return Vec::new();
    }

    // ── Phase 3: Wörter in Zeilen der Breite `width` pakieren. ───────────
    let mut out: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;

    for w in &words {
        // Führende Whitespaces zählen (beim Zeilenumbruch entfernt).
        let leading_ws: usize = w.iter().take_while(|(c, _)| c.is_whitespace()).count();
        let content_w: usize = w[leading_ws..].iter().map(|(c, _)| char_w(*c)).sum();

        // Kein zusätzliches Space, wenn die aktuelle Zeile bereits mit
        // Whitespace endet (z. B. angeheftetes Code-Whitespace).
        let cur_ends_ws = cur.last().map(|&(c, _)| c.is_whitespace()).unwrap_or(false);
        let need_space = cur_w > 0 && leading_ws == 0 && !cur_ends_ws;
        let space_w = usize::from(need_space);

        if cur_w + space_w + content_w <= width {
            // Passt auf die aktuelle Zeile.
            if need_space {
                // Trenn-Space immer neutral – nur Whitespaces, die
                // tatsächlich zu einem Code-Span gehören (in Phase 2
                // angeheftet), tragen den Code-Style.
                cur.push((' ', Style::default()));
                cur_w += 1;
            }
            cur.extend(w[leading_ws..].iter().cloned());
            cur_w += content_w;
        } else if content_w <= width {
            // Passt allein auf eine neue Zeile.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.extend(w[leading_ws..].iter().cloned());
            cur_w = content_w;
        } else {
            // Wort breiter als die Zeile: in Chunks teilen (Trennzeichen-/
            // Blind-Bruch).  Führende Whitespaces werden ignoriert – sie
            // gehören zum vorherigen Wort bzw. zum Zeilenumbruch.
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let content: Vec<(char, Style)> = w[leading_ws..].to_vec();
            let mut chunks = split_long_word(&content, width).into_iter();
            let last = chunks.next_back().expect("split_long_word liefert Chunks");
            out.extend(chunks);
            cur = last;
            cur_w = total_width(&cur);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Flacht eine formatierte Zeile zu Zeichen+Stil-Paaren ab.
pub(super) fn flatten<'a>(line: &Line<'a>) -> Vec<(char, Style)> {
    let mut out = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            out.push((c, span.style));
        }
    }
    out
}

/// Bricht logische Markdown-Zeilen wortweise auf `width` Zellen um und
/// versieht jede physische Zeile mit `indent` Leerzellen (hängender Einzug,
/// der bei allen Fortsetzungszeilen berücksichtigt wird). Umbruch erfolgt nur
/// an Whitespaces; zu lange Wörter werden hart getrennt.
pub(super) fn wrap_block<'a>(logical: &[Line<'a>], width: usize, indent: usize) -> Vec<Line<'a>> {
    // Symmetrischer Rand: rechts endet der Inhalt `PAD_R` Zellen vor dem Rand.
    let budget = width.saturating_sub(indent + PAD_R).max(1);
    let pad = " ".repeat(indent);
    let mut out = Vec::new();
    for line in logical {
        if line.width() == 0 {
            out.push(Line::from(""));
            continue;
        }
        for wl in wrap_line(line, budget) {
            let mut spans = Vec::with_capacity(wl.spans.len() + 1);
            spans.push(Span::raw(pad.clone()));
            spans.extend(wl.spans);
            out.push(Line::from(spans));
        }
    }
    out
}

/// Bricht eine logische Zeile an Whitespace-Grenzen zu physischen Zeilen der
/// Breite `width` um (Wort-Umbruch, Styled-Zeichen bleiben erhalten).
pub(super) fn wrap_line<'a>(line: &Line<'a>, width: usize) -> Vec<Line<'a>> {
    // Wörter: Läufe nicht-Whitespace-Zeichen mit ihrem Style.
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    for span in &line.spans {
        let style = span.style;
        let mut word: Vec<(char, Style)> = Vec::new();
        for c in span.content.chars() {
            if c.is_whitespace() {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            } else {
                word.push((c, style));
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    if words.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<Line> = Vec::new();
    let mut cur: Vec<(char, Style)> = Vec::new();
    let mut cur_w = 0usize;
    for word in words {
        let ww = total_width(&word);
        let space = usize::from(cur_w > 0);
        if cur_w + space + ww <= width {
            if space > 0 {
                cur.push((' ', Style::default()));
                cur_w += 1;
            }
            cur.extend(word);
            cur_w += ww;
        } else if ww <= width {
            if !cur.is_empty() {
                out.push(spans_of(&cur));
                cur.clear();
            }
            cur = word;
            cur_w = ww;
        } else {
            // Wort breiter als die Zeile: Rest beenden, dann mit
            // Trennzeichen-/Blind-Bruch aufteilen. Das letzte Stück bleibt
            // als `cur`, damit ein folgendes Wort die Zeile auffüllen kann.
            if !cur.is_empty() {
                out.push(spans_of(&cur));
                cur.clear();
            }
            let mut chunks = split_long_word(&word, width).into_iter();
            let last = chunks.next_back().expect("split_long_word liefert Chunks");
            out.extend(chunks.map(|c| spans_of(&c)));
            cur = last;
            cur_w = total_width(&cur);
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(spans_of(&cur));
    }
    out
}

/// Wandelt Styled-Zeichen in konsolidierte Spans um (gleiche Style-Läufe).
pub(super) fn spans_of(chars: &[(char, Style)]) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut st: Option<Style> = None;
    for (c, s) in chars {
        if st != Some(*s) {
            if !buf.is_empty() {
                out.push(Span::styled(
                    std::mem::take(&mut buf),
                    st.unwrap_or_default(),
                ));
            }
            st = Some(*s);
        }
        buf.push(*c);
    }
    if !buf.is_empty() {
        out.push(Span::styled(buf, st.unwrap_or_default()));
    }
    Line::from(out)
}

/// Status-Emoji, die Terminals breit (2 Zellen) rendern, aber unicode-width
/// mit 1 zählt. Die Glyph-Ersatzzeichen (✓, ✗, ●, ☑, ⚠, !, ▲, ★…) sind hier
/// BEWUSST nicht enthalten – deren Breite ist überall konsistent 1.
pub(super) fn is_wide_emoji(c: char) -> bool {
    matches!(
        c,
        '\u{2705}' // ✅
            | '\u{2714}' // ✔
            | '\u{274C}' // ❌
            | '\u{274E}' // ❎
            | '\u{2B1C}' // ⬜
            | '\u{2757}' // ❗
            | '\u{2753}' // ❓
            | '\u{1F534}' // 🔴
            | '\u{1F7E2}' // 🟢
            | '\u{1F7E1}' // 🟡
            | '\u{1F44D}' // 👍
            | '\u{1F44E}' // 👎
            | '\u{1F6A8}' // 🚨
            | '\u{1F197}' // 🆗
            | '\u{1F4A1}' // 💡
    )
}

pub(super) fn char_w(c: char) -> usize {
    if is_wide_emoji(c) {
        2
    } else {
        // `unwrap_or(1)`: Zeichen, deren Breite unicode-width NICHT kennt
        // (Steuer-/unbekannte Zeichen), bekommen sicher 1 Zelle, damit das
        // Layout nicht zusammenklappt. Bekannte Breite-0-Zeichen (Combining-
        // Marks, ZWJ, Variation-Selectors) bleiben dagegen bei 0 – sie werden
        // vom Terminal unsichtbar/mitgeführt gerendert und dürfen bei der
        // Vermessung (rechtsbündiger Balken, „…“-Kürzung) nicht als 1 Zelle
        // zählen, sonst rutschen Balken und Token-Zahlen nach links.
        UnicodeWidthChar::width(c).unwrap_or(1)
    }
}

pub(super) fn total_width(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|(c, _)| char_w(*c)).sum()
}
