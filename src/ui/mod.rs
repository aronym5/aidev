use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Phase};
use crate::editor::InputLayout;

pub(crate) use theme::*;
mod blocks;
mod dialogs;
mod input;
mod status;
mod theme;
pub(crate) use blocks::*;
pub(crate) use dialogs::*;
pub(crate) use input::*;
pub(crate) use status::*;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// ASCII-Art-Logo, das am Anfang jeder Session oben steht.
/// Bewusst als rohe Zeilen ohne Umbruch (Logo soll nicht gewrappt werden).
const LOGO: [&str; 4] = [
    "       ▀      █             ",
    " ▀▀▀▄ ▀█   ▄▄▄█  ▄▀▀▀█ █▄ ▄█",
    "▄▀▀▀█  █  █   █  █▄▀▀   █▄█ ",
    "▀▄▄▄▀▄ ▀▄ ▀▄▄▀█▄ ▀▄▄▄▀   █  ",
];

/// Die Logo-Zeilen, horizontal zentriert; der erste markante Glyph der ersten
/// Zeile (das „i-Punkt“) leuchtet in der Akzentfarbe. Kein Wort-Umbruch.
fn logo_lines(width: usize) -> Vec<Line<'static>> {
    LOGO.iter()
        .enumerate()
        .map(|(i, l)| {
            let chars: Vec<char> = l.chars().collect();
            let left = width.saturating_sub(chars.len()) / 2;
            let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(left))];
            if i == 0 {
                if let Some(dot) = chars.iter().position(|c| !c.is_whitespace()) {
                    spans.push(Span::raw(chars[..dot].iter().collect::<String>()));
                    spans.push(Span::styled(
                        chars[dot..dot + 1].iter().collect::<String>(),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::styled(
                        chars[dot + 1..].iter().collect::<String>(),
                        Style::default().fg(MUTED),
                    ));
                    return Line::from(spans);
                }
            }
            spans.push(Span::styled(l.to_string(), Style::default().fg(MUTED)));
            Line::from(spans)
        })
        .collect()
}

/// Höhe des Logos (Zeilenanzahl) – für die vertikale Zentrierung.
const LOGO_ROWS: u16 = 4;

/// Markdown-Stylesheet passend zum Farbschema: Heading-Marker entfallen,
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Gesamter Hintergrund zuerst – unterliegt allen Rändern/Paddings.
    let bg = Paragraph::new("").style(Style::default().bg(BASE_BG));
    f.render_widget(bg, area);

    let active = app.active;
    // Eingabe-Band: Editor-Breite um den linken Abstand reduziert.
    app.sessions[active]
        .editor
        .set_width(area.width.saturating_sub(PAD as u16) as usize);
    // Der Berechtigungs-Prompt bestimmt den hängenden Einzug (Fortsetzungszeilen
    // richten sich unter dem eigentlichen Text aus, nicht unter dem Prompt).
    // Ohne Kanal gibt es keine Berechtigung – der Einzug entspricht dann dem
    // neutralen Prompt (`> `).
    let bound = app.sessions[active].channel.is_some();
    let permission = app.sessions[active].permission;
    let (_, prompt_width) = permission_prompt(bound.then_some(permission));
    app.sessions[active].editor.set_indent(prompt_width);
    let layout = app.sessions[active].editor.layout();
    let input_rows = layout.rows();
    let max_input = area.height.saturating_sub(2).max(1) as usize;
    let input_height = input_rows.min(max_input).max(1) as u16;

    // Bei mehreren Konversationen erscheint oben eine Tab-Leiste.
    let tab_rows = if app.sessions.len() > 1 { 1 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(tab_rows), // Tabs
        Constraint::Min(1),           // Chat (schrumpft)
        Constraint::Length(input_height),
        Constraint::Length(1), // Status-Hinweiszeile
    ])
    .split(area);

    // Reiter-Titel ggf. auffrischen (max. 1×/s, Git-Aufrufe drin) – vor dem
    // Zeichnen, damit die Leiste immer aktuelle Titel zeigt.
    app.refresh_tab_labels();
    draw_tabs(f, chunks[0], app);
    // `draw_chat` aktualisiert den Scroll-Zustand (Anker/Delta/Viewport) selbst.
    draw_chat(f, chunks[1], app);
    draw_input(f, chunks[2], app, &layout);
    draw_status(f, chunks[3], app);
    draw_channel_picker(f, app);
    draw_model_picker(f, app);
    draw_channel_builder(f, app);
    draw_confirmation(f, app);
    draw_http_headers_dialog(f, app);
}

/// Obere Reiter-Leiste, nur sichtbar bei mehr als einer Konversation.
/// Der aktive Reiter ist hervorgehoben; ein laufender Agent zeigt eine
/// Warte-Animation an (`spinner`), ruhende Reiter einen Punkt. Jeder Reiter
/// trägt einen Titel (siehe `App::session_tab_label`): `branch@repo` bei
/// Repo-Kanälen, sonst der Host-Ordner, ohne Kanal der Modellname.
fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    if app.sessions.len() <= 1 {
        return;
    }
    let mut spans: Vec<Span> = Vec::with_capacity(app.sessions.len());
    for (i, s) in app.sessions.iter().enumerate() {
        let is_active = i == app.active;
        // Wartet die Session auf eine User-Entscheidung (blockierender
        // Bestätigungsdialog), steht der Spinner still: ein statischer, nicht
        // animierter Marker statt der rotierenden Braille-Zeichen.
        let mark = if app.session_awaits_decision(i) {
            "…"
        } else if s.phase == Phase::WaitingForLLM || s.phase == Phase::WaitingForTool {
            SPINNER[app.spinner % SPINNER.len()]
        } else {
            "·"
        };
        // Titel aus dem Cache; fehlt der Eintrag (sollte durch
        // `refresh_tab_labels` nie passieren), nur die Nummer zeigen.
        let label = app.tab_labels.get(i).map(String::as_str).unwrap_or("");
        let text = format!(" {label} {mark} ");
        let style = if is_active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
                .bg(INPUT_BG)
        } else {
            Style::default().fg(MUTED).bg(STATUS_BG)
        };
        spans.push(Span::styled(text, style));
    }
    let band = Paragraph::new(Line::from(spans)).style(Style::default().bg(STATUS_BG));
    f.render_widget(band, area);
}

/// Rendert den Chat-Bereich der aktiven Konversation in einzelnen Blöcken
/// (je Nachricht/Element). Die abgeschlossene Historie kommt aus dem gecachten
/// `HistoryCache` (nur bei Breite/Toggle/Inhaltsänderung neu umgebrochen), der
/// laufende Turn wird pro Frame neu gebaut. Die manuelle Scroll-Position ist
/// ein Block-Anker `(Block, Offset)` und bleibt beim Auf-/Zuklappen (Tab)
/// stabil; hier wird sie pro Frame in Bildschirmzeilen übersetzt.
fn draw_chat(f: &mut Frame, area: Rect, app: &mut App) {
    let viewport = area.height as usize;
    let width = area.width as usize;
    let mode = app.config.symbols;
    let active = app.active;
    let model = app.display_model(active);
    let window = app
        .config
        .context_window_for(app.sessions[active].model_alias.as_deref());

    // Historie-Cache ggf. neu aufbauen (mutabler Borrow, endet sofort).
    {
        let s = &mut app.sessions[active];
        ensure_history_cache(s, width, mode, &model, window);
    }

    // Layout + Scroll + Rendern in einem Scope, der die Chat-Daten immutable
    // hält; danach wird der Scroll-Zustand der Session aktualisiert.
    let (new_anchor, at_bottom, live_used) = {
        let s = &app.sessions[active];
        let cache = s.history_cache.as_ref().expect("History-Cache vorhanden");
        let (live, live_used) = build_live_blocks(s, width, mode, window, &cache.end_ctx);

        let logo_gap = viewport.saturating_sub(LOGO_ROWS as usize) / 2;
        let logo = ChatBlock {
            lines: logo_lines(width),
            bg: None,
            gap: logo_gap as u16,
            is_tool: false,

        };
        let (placed, total) = layout_blocks(&logo, &cache.blocks, &live);

        // Content-Tops/-Höhen (ohne Logo) für die Anker-Umrechnung.
        let mut tops = Vec::with_capacity(placed.len() - 1);
        let mut heights = Vec::with_capacity(placed.len() - 1);
        for p in &placed[1..] {
            tops.push(p.top);
            heights.push(p.height);
        }

        let max_scroll = total.saturating_sub(viewport);
        // Sobald genug Historie da ist (max_scroll reicht bis unter das Logo), ist
        // das Logo „vergessen": Manuelles Zurückscrollen stoppt unten an der
        // ersten Konversationszeile (logo_gap + Logo-Höhe + Leerzeile), der
        // halb-leere Logo-Bildschirm wird nicht wieder eingeblendet.
        let first_msg = logo_gap + LOGO_ROWS as usize + 1;
        let min_scroll = if max_scroll >= first_msg {
            first_msg
        } else {
            0
        };

        let follow = s.chat_follow;
        let anchor = s.chat_anchor;
        // Anstehende Seitenbewegung (PgUp/PgDn) auf den Anker anwenden.
        let mut center = anchor_to_line(&tops, &heights, anchor);
        if let Some(delta) = s.chat_scroll_delta {
            let max_line = total.saturating_sub(1) as i64;
            center = (center as i64 + delta).clamp(0, max_line) as usize;
        }
        let scroll_y = if follow {
            max_scroll
        } else {
            center
                .saturating_sub(viewport / 2)
                .clamp(min_scroll, max_scroll)
        };
        // Manuell bis ganz ans Ende gescrollt? Dann zurück in den Auto-Follow:
        // Der Fixpunkt ist wieder das untere Ende, neue Nachrichten hängen an.
        let at_bottom = !follow && scroll_y == max_scroll;
        // Anker auf die reale Mittelzeile aktualisieren – so entspricht er nach
        // jedem Frame der Wirklichkeit und bleibt beim Tab-Toggle stabil.
        let center_actual = (scroll_y + viewport / 2).min(total.saturating_sub(1));
        let new_anchor = line_to_anchor(&tops, &heights, center_actual);

        for p in &placed {
            let height = p.height;
            if height == 0 || p.top + height <= scroll_y {
                continue; // komplett oberhalb des Viewports
            }
            if p.top >= scroll_y + viewport {
                break; // Blöcke sind geordnet – Rest liegt unterhalb
            }
            let vis_top = p.top.max(scroll_y) - scroll_y;
            let vis_bottom = (p.top + height).min(scroll_y + viewport) - scroll_y;
            let inner_scroll = scroll_y.saturating_sub(p.top);
            let rect = Rect::new(
                area.x,
                area.y + vis_top as u16,
                area.width,
                (vis_bottom - vis_top) as u16,
            );
            // Nur sichtbare Blöcke klonen – die Zahl ist durch die Viewport-Höhe
            // begrenzt und unabhängig von der Länge der Session.
            let mut para = Paragraph::new(p.block.lines.clone());
            if let Some(bg) = p.block.bg {
                para = para.style(Style::default().bg(bg));
            }
            para = para.scroll((inner_scroll as u16, 0));
            f.render_widget(para, rect);
        }

        (new_anchor, at_bottom, live_used)
    };

    // Scroll-Zustand der Session persistieren (mutabler Borrow).
    let s = &mut app.sessions[active];
    s.chat_anchor = new_anchor;
    // Am unteren Rand angekommen → wieder Auto-Follow (unterer Fixpunkt).
    if at_bottom {
        s.chat_follow = true;
    }
    s.chat_scroll_delta = None;
    s.chat_viewport = area.height;
    // Kontextstand des Live-Tails ablegen – speist die Übersichts-Balken der
    // Schätzung (nicht mehr die Statusleiste, die nur die gestreamte
    // serverbestätigte `total_tokens` zeigt).
    s.live_context = live_used;
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    status::draw_status(f, area, app);
}

/// Ist gerade (mindestens) ein modaler Dialog offen? Solange ein Dialog den
/// Bildschirm überlagert, soll der Cursor NICHT in der Chat-Eingabezeile
/// stehen bleiben – ratatui versteckt ihn, wenn in einem Frame keine
/// Position gesetzt wird. (Ausnahme: Ein Dialog mit einem eigenen Textinput –
/// der Channel Builder beim „add path" – setzt den Cursor selbst.)
fn dialog_open(app: &App) -> bool {
    app.any_dialog_open()
}

fn draw_input(f: &mut Frame, area: Rect, app: &App, layout: &InputLayout) {
    input::draw_input(
        f,
        area,
        app,
        layout,
        /* show_cursor= */ !dialog_open(app),
    );
}

// Markdown-Rendering: Deko, Wortumbruch, Tabellen – als eigenes Untermodul.
mod markdown;

#[cfg(test)]
mod tests;
