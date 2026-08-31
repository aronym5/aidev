//! Zentrierte Overlay-Dialoge: Kanal-Auswahl, Modell-Auswahl, Channel Builder
//! und Bestätigungsdialoge (Beenden, Worktree, Container, lokale Ausführung).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Phase};
use crate::channel::ChannelStatus;

use super::*;

/// Schätzt die nötige Innen-Breite (ohne Innenrand) des Channel-Pickers,
/// damit Kanalnamen und der neu eingeblendete Session-Hinweis
/// (z. B. „· Session 1, 3 (aktiv)") nicht rechts abgeschnitten werden.
pub(crate) fn channel_picker_content_width(app: &App, picker: &crate::app::ChannelPicker) -> usize {
    // Titel und Fußzeile (die längste statische Zeile) mitberücksichtigen.
    let mut w = " Choose channel for this session ".chars().count();
    w = w.max(key_help_full_width(&CHANNEL_PICKER_KEYS));
    // „(no channel)"-Zeile: Indikator(1) + „ {name}".
    w = w.max(1 + 1 + "(no channel)".chars().count());
    // Kanal-Zeilen: Indikator(1) + „ {name}" + ggf. „  {usage}".
    for item in picker.items.iter().skip(1) {
        let usage = channel_usage(app, item);
        let mut row = 1 + 1 + item.chars().count();
        if !usage.is_empty() {
            row += 2 + usage.chars().count();
        }
        w = w.max(row);
    }
    w
}

/// Zentrierter Auswahl-Dialog für den Kanal der aktiven Session.
pub(crate) fn draw_channel_picker(f: &mut Frame, app: &App) {
    let Some(picker) = &app.channel_picker else {
        return;
    };
    let area = f.area();

    // Breite dynamisch an den Inhalt anpassen – sonst werden Kanalnamen samt
    // Session-Hinweis („· Session …") rechts abgeschnitten, sobald dieser
    // länger als die bisherige Festbreite ist.
    let content_w = channel_picker_content_width(app, picker);
    let max_w = area.width.saturating_sub(4) as usize;
    let width = ((content_w + PICKER_PAD * 2).clamp(24, max_w)) as u16;

    let mut height = picker.items.len() as u16 + 4;
    height = height.min(area.height.saturating_sub(6).max(5));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    // Hintergrund des Dialogs flächig füllen (modern ohne schwere Rahmen).
    // Das `Clear` löscht zuerst den gesamten Inhaltsbereich unter dem Dialog,
    // damit das Overlay den Chat vollständig abdeckt.
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(STATUS_BG));
    f.render_widget(bg, rect);

    // Inhalt beginnt erst nach einem Innenrand – der Rand gehört überall zum
    // Overlay, nicht nur links vom Text (die Lampen liegen dadurch im Overlay).
    let pad = PICKER_PAD as u16;
    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );
    let title = Line::from(Span::styled(
        " Choose channel for this session ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let mut lines: Vec<Line> = vec![title];
    lines.push(channel_picker_row(
        "(no channel)",
        None,
        picker.cursor == 0,
        "",
    ));
    for (i, item) in picker.items.iter().enumerate().skip(1) {
        if item == "new channel" {
            lines.push(channel_picker_new_channel_row(i == picker.cursor));
        } else {
            let status = app.channels.get(item).map(|ch| ch.status());
            let usage = channel_usage(app, item);
            lines.push(channel_picker_row(item, status, i == picker.cursor, &usage));
        }
    }
    lines.push(key_help(&CHANNEL_PICKER_KEYS, inner.width as usize));
    let para = Paragraph::new(lines).style(Style::default().bg(STATUS_BG));
    f.render_widget(para, inner);
}

/// Zentrierter Auswahl-Dialog für das Modell der aktiven Session (`/model`).
/// „(Standard)" erscheint nur als eigener Eintrag, wenn das Default-Modell
/// bei den konfigurierten Modellen fehlt.
pub(crate) fn draw_model_picker(f: &mut Frame, app: &App) {
    let Some(picker) = &app.model_picker else {
        return;
    };
    let area = f.area();

    // Breite dynamisch an den Inhalt anpassen (Titel, Zeilen, Fußzeile).
    let title = " Model for this session ";
    let mut content_w = title
        .chars()
        .count()
        .max(key_help_full_width(&MODEL_PICKER_KEYS));
    if picker.show_default {
        // +3: ⬢-Indikator ist 2 Zellen breit + führendes Leerzeichen.
        content_w = content_w.max(" (default)".chars().count() + 3);
    }
    for (_key, display) in &picker.items {
        // +3 statt +1: Der ⬢-Indikator ist 2 Zellen breit und es folgt ein
        // Leerzeichen – sonst wird die Demand-Angabe rechts abgeschnitten.
        let row = display.chars().count() + 3;
        content_w = content_w.max(row);
    }
    if picker.loading {
        content_w = content_w.max(" … fetching models …".chars().count() + 1);
    }
    let max_w = area.width.saturating_sub(4) as usize;
    let width = ((content_w + PICKER_PAD * 2).clamp(28, max_w)) as u16;

    let extra = if picker.show_default { 1 } else { 0 };
    let rows = picker.items.len() as u16 + extra;
    let mut height = rows + 4;
    height = height.min(area.height.saturating_sub(6).max(5));
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(STATUS_BG));
    f.render_widget(bg, rect);

    let pad = PICKER_PAD as u16;
    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    if picker.show_default {
        lines.push(model_picker_row("(default)", picker.cursor == 0, None));
    }
    for (i, (key, display)) in picker.items.iter().enumerate() {
        let idx = if picker.show_default { i + 1 } else { i };
        // Farblicher Status-Indikator aus der Model-Registry.
        let color = app.model_registry.status(key).map(|s| match s {
            crate::app::models::ModelStatus::ConfigAndFetched => SYM_OK, // grün
            crate::app::models::ModelStatus::ConfigStale => SYM_ERR,     // rot
            crate::app::models::ModelStatus::FetchedOnly => SYM_MUTED,   // grau
        });
        lines.push(model_picker_row(display, idx == picker.cursor, color));
    }
    if picker.loading {
        lines.push(Line::from(Span::styled(
            " … fetching models …",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(key_help(&MODEL_PICKER_KEYS, inner.width as usize));
    let para = Paragraph::new(lines).style(Style::default().bg(STATUS_BG));
    f.render_widget(para, inner);
}

/// Eine Zeile des Modell-Auswahl-Dialogs: Status-Indikator (⬢) in der Farbe
/// des Refresh-Status + Anzeigetext. Reine Funktion, damit sie ohne Terminal
/// testbar ist.
fn model_picker_row(display: &str, cursor: bool, status_color: Option<Color>) -> Line<'static> {
    let base_color = if cursor {
        Color::White
    } else if display == "(default)" {
        MUTED
    } else {
        ACCENT_FG
    };
    let bold = if cursor {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let indicator = Span::styled(
        "\u{2b22}",
        Style::default().fg(status_color.unwrap_or(SYM_MUTED)),
    );
    // Aufteilen in "provider/alias (name)" und "· demand".
    let (main, demand) = match display.split_once('·') {
        Some((m, d)) => (m.trim_end(), Some(d)),
        None => (display, None),
    };
    let mut spans = vec![indicator];
    // Base-Anteil: nur das "provider/alias"-Kürzel in base_color, ein
    // dahinterstehender Modellname in Klammern in MUTED (wie die Demand-Angabe).
    let gray = Style::default().fg(MUTED);
    match main.find(" (") {
        Some(idx) if main.ends_with(')') => {
            let (short, name) = main.split_at(idx);
            spans.push(Span::styled(
                format!(" {short}"),
                Style::default().fg(base_color).add_modifier(bold),
            ));
            spans.push(Span::styled(name.to_string(), gray));
        }
        _ => {
            spans.push(Span::styled(
                format!(" {main}"),
                Style::default().fg(base_color).add_modifier(bold),
            ));
        }
    }
    if let Some(d) = demand {
        spans.push(Span::styled(format!(" ·{d}"), gray));
    }
    Line::from(spans)
}

/// Channel Builder: Tunnel | Host/Repo | Worktree. Ohne Repo-Wurzel als
/// Host-Pfad entfällt die rechte Spalte (nur zwei Spalten).
pub(crate) fn draw_channel_builder(f: &mut Frame, app: &App) {
    let Some(builder) = &app.channel_builder else {
        return;
    };
    let area = f.area();
    let width = (area.width * 9 / 10).clamp(60, 120);

    // Innenbreite – hängt nur von `width` ab (nicht von der Höhe) und wird
    // schon hier gebraucht, um die Spaltenbreiten und damit die
    // Umbruchszeilen der Inhalte zu bestimmen.
    let pad = PICKER_PAD as u16;
    let inner_width = width.saturating_sub(pad * 2);

    // Spalten-Breiten: Mit Worktree-Spalte drei gleiche Drittel. Ohne
    // (Host-Pfad ist keine Repo-Wurzel) bleiben Tunnel und Host an ihren
    // gewohnten Positionen – der Host nimmt dann aber die volle Restbreite
    // bis an den rechten Dialogrand ein.
    let show_worktrees = builder.current_is_git;
    let (tunnel_w, host_w, worktree_w) = if show_worktrees {
        let col_w = inner_width / 3;
        (col_w, col_w, inner_width.saturating_sub(col_w * 2))
    } else {
        let col_w = inner_width / 3;
        (col_w, inner_width.saturating_sub(col_w), 0)
    };

    // Spalten-Header
    let header_style = Style::default().fg(MUTED).add_modifier(Modifier::BOLD);
    let tunnel_header = Line::from(Span::styled(
        format!(" {:<width$}", "Tunnel", width = tunnel_w as usize - 1),
        header_style,
    ));
    let host_header = Line::from(Span::styled(
        format!(
            " {:<width$}",
            "Directory / Repo on host",
            width = host_w as usize - 1
        ),
        header_style,
    ));
    let worktree_header = if show_worktrees {
        Some(Line::from(Span::styled(
            format!(
                " {:<width$}",
                "Worktree / Branch",
                width = worktree_w as usize - 1
            ),
            header_style,
        )))
    } else {
        None
    };

    // === Style-Konstanten ===
    // Aktive Spalte: cursor → White + Bold, kein Hintergrund
    let active_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    // Inaktive Spalte: genau ein markierter Eintrag
    let selected_style = Style::default().fg(Color::White);
    // Restliche Einträge: gedämpft
    let normal_style = Style::default().fg(ACCENT_FG);
    // Branches ohne Worktree: noch gedämpfter
    let dim_style = Style::default().fg(MUTED);

    // Tunnel-Spalte
    let mut tunnel_lines: Vec<Line> = vec![tunnel_header];
    for (i, tunnel) in builder.tunnels.iter().enumerate() {
        let is_active_col = builder.col == 0;
        let is_selected = i == builder.tunnel_idx;
        let style = if is_active_col && is_selected {
            active_style
        } else if is_selected {
            selected_style
        } else {
            normal_style
        };
        let marker = if is_active_col && is_selected {
            "▶ "
        } else if is_selected {
            "● "
        } else {
            "  "
        };
        tunnel_lines.push(Line::from(Span::styled(
            format!("{}{}", marker, tunnel.label()),
            style,
        )));
    }
    // Lade-Indikator solange Podman-Images noch nicht geladen sind
    if !builder.images_loaded {
        tunnel_lines.push(Line::from(Span::styled("  …", Style::default().fg(MUTED))));
    }

    // Host-Spalte
    let mut host_lines: Vec<Line> = vec![host_header];
    for (i, hp) in builder.host_paths.iter().enumerate() {
        let is_active_col = builder.col == 1;
        let is_selected = i == builder.host_idx;
        let style = if is_active_col && is_selected {
            active_style
        } else if is_selected {
            selected_style
        } else {
            normal_style
        };
        let marker = if is_active_col && is_selected {
            "▶ "
        } else if is_selected {
            "● "
        } else {
            "  "
        };
        host_lines.push(Line::from(Span::styled(
            format!("{}{}", marker, hp.label()),
            style,
        )));
    }

    // Worktree-Spalte – entfällt komplett, wenn der Host-Pfad keine
    // Repo-Wurzel ist.
    let worktree_lines: Option<Vec<Line>> = if show_worktrees {
        let mut lines = vec![worktree_header.expect("Header bei Worktree-Spalte")];
        if !builder.worktrees.is_empty() {
            for (i, wt) in builder.worktrees.iter().enumerate() {
                let is_active_col = builder.col == 2;
                let is_selected = i == builder.worktree_idx;
                let base_style = if wt.has_worktree {
                    normal_style
                } else {
                    dim_style
                };
                let style = if is_active_col && is_selected {
                    active_style
                } else if is_selected {
                    selected_style
                } else {
                    base_style
                };
                let marker = if is_active_col && is_selected {
                    "▶ "
                } else if is_selected {
                    "● "
                } else if wt.has_worktree {
                    "  "
                } else {
                    "○ "
                };
                lines.push(Line::from(Span::styled(
                    format!("{}{}", marker, wt.label),
                    style,
                )));
            }
        } else {
            lines.push(Line::from(Span::styled(
                "  (no worktrees)",
                Style::default().fg(MUTED),
            )));
        }
        Some(lines)
    } else {
        None
    };

    // Wie viele Zeilen braucht jede Spalte nach dem Umbruch langer Pfade?
    // Jede Zeile wird mit `wrapped_row_count` auf die Spaltenbreite
    // umgebrochen; dadurch rücken nachfolgende Einträge in der Spalte nach
    // unten (statt rechts abgeschnitten zu werden).
    let col_rows = |lines: &[Line<'_>], col_w: u16| -> u16 {
        lines.iter().map(|l| wrapped_row_count(l, col_w)).sum()
    };
    let mut max_rows = col_rows(&tunnel_lines, tunnel_w).max(col_rows(&host_lines, host_w));
    if let Some(wt) = &worktree_lines {
        max_rows = max_rows.max(col_rows(wt, worktree_w));
    }
    let max_rows = max_rows as usize;

    // Dialog-Höhe: ebenfalls am (umgebrochenen) Inhalt orientiert. Der
    // Spaltenbereich ist `inner.height - 4` (2 Titelzeilen + 1 Container-
    // Status + 1 Footer), also gilt height = Spaltenhöhe + 4; damit die
    // größte Spalte vollständig sichtbar ist: height = max_rows + 6.
    let height = (max_rows + 6).clamp(15, area.height.saturating_sub(2) as usize) as u16;
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    // Hintergrund
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(STATUS_BG));
    f.render_widget(bg, rect);

    // Innenrand
    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );

    // Titel
    let title = Line::from(Span::styled(
        " New channel ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));

    // Spalten-Inhalte: Titel über den Spalten (Platz ist reserviert:
    // Spalten beginnen bei inner.y + 2 = Titel + Leerzeile).
    f.render_widget(
        Paragraph::new(vec![title, Line::from("")]).style(Style::default().bg(STATUS_BG)),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );

    // Spalten-Layout: ohne Repo nur zwei Spalten (Worktree-Spalte entfällt).
    let mut constraints = vec![Constraint::Length(tunnel_w), Constraint::Length(host_w)];
    if show_worktrees {
        constraints.push(Constraint::Length(worktree_w));
    }
    let cols = Layout::horizontal(constraints).split(Rect::new(
        inner.x,
        inner.y + 2, // Nach Titel + Leerzeile
        inner.width,
        inner.height.saturating_sub(4), // Titel + Footer
    ));

    // Spalten rendern (auf max_rows kürzen). Mit `Wrap` werden zu lange Pfade
    // umgebrochen, statt rechts abgeschnitten zu werden.
    let render_col = |f: &mut Frame, area: Rect, lines: &[Line], max: usize| {
        let display: Vec<Line> = lines.iter().take(max).cloned().collect();
        let para = Paragraph::new(display)
            .style(Style::default().bg(STATUS_BG))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    };

    render_col(f, cols[0], &tunnel_lines, max_rows);
    render_col(f, cols[1], &host_lines, max_rows);
    if let Some(lines) = &worktree_lines {
        render_col(f, cols[2], lines, max_rows);
    }

    // Container-Status-Zeile (unterhalb der Spalten)
    if let Some(info) = &builder.container_info {
        let status_line = Line::from(Span::styled(
            format!(" Container: {} ({})", info.name, info.status),
            Style::default().fg(MUTED),
        ));
        let status_rect = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(2),
            inner.width,
            1,
        );
        f.render_widget(Paragraph::new(status_line), status_rect);
    }

    // Fußzeile: Tasten-Hilfe. Beim Pfad-Edit ist „Pfad eingeben“ eine
    // Beschreibung (kein Tastenkürzel); die eigentlichen Kürzel folgen über
    // `key_help`.
    let footer: Line<'static> = if builder.host_path_edit.is_some() {
        let prefix = " Enter a path ·";
        let mut spans = vec![Span::styled(prefix, Style::default().fg(MUTED))];
        let budget = inner.width.saturating_sub(prefix.chars().count() as u16) as usize;
        spans.extend(key_help(&[("↵", "confirm"), ("esc", "cancel")], budget).spans);
        Line::from(spans)
    } else {
        key_help(
            &[
                ("⇅", "select"),
                ("⇄", "column"),
                ("a", "add path"),
                ("↵", "confirm"),
                ("esc", "cancel"),
            ],
            inner.width as usize,
        )
    };
    let footer_rect = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(footer), footer_rect);

    // Pfad-Input: inline unter dem letzten Host-Pfad-Eintrag.
    if let Some(editor) = &builder.host_path_edit {
        let input_x = cols[1].x;
        let input_w = if show_worktrees {
            (cols[2].x + cols[2].width).saturating_sub(cols[1].x)
        } else {
            cols[1].width
        };
        // Fußzeile ist die letzte Zeile des Dialogs; die Editor-Zeile (`entry_y+1`)
        // muss strikt darüber liegen. Damit das Eingabefeld samt Cursor IMMER
        // sichtbar bleibt (auch bei vielen Host-Pfaden), wird es nicht unter
        // den letzten (ggf. abgeschnittenen) Eintrag gelegt, sondern nach oben
        // begrenzt, sodass Label + Editor-Zeile vor der Fußzeile enden.
        let footer_y = inner.y + inner.height.saturating_sub(1);
        let edit_row = cols[1].y + 1 + builder.host_paths.len() as u16 + 1;
        let entry_y = edit_row.min(footer_y.saturating_sub(2));
        if entry_y + 1 < footer_y {
            draw_builder_path_input_inline(f, input_x, entry_y, input_w, editor);
        }
    }
}

/// Zeichnet das Pfad-Eingabe-Feld inline im Channel Builder: Label + Editor
/// direkt unter dem letzten Host-Pfad-Eintrag, aligned mit den Spalten.
fn draw_builder_path_input_inline(
    f: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    editor: &crate::editor::Editor,
) {
    use crate::editor::InputLayout;

    let prompt = "> ";
    let indent = prompt.len();

    // Label
    let label = Line::from(Span::styled(
        " Add path:",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(label).style(Style::default().bg(STATUS_BG)),
        Rect::new(x, y, width, 1),
    );

    // Editor-Zeile
    let content_width = width as usize;
    let layout = InputLayout {
        width: content_width + indent,
        indent,
        content: content_width,
        ranges: vec![0..editor.text().len()],
        total: editor.text().len(),
    };

    let sel_range = editor.selected_range();
    let (cur_row, cur_off) = editor.cursor_row_col(&layout);
    let text_str: String = editor.text().iter().collect();

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        prompt,
        Style::default()
            .fg(Color::Rgb(245, 158, 11))
            .add_modifier(Modifier::BOLD),
    ));

    // Cursor-Block mit expliziten Farben statt `REVERSED` ohne Farben: Auf
    // einem Span mit Reset-Farben (kein gesetztes fg/bg) bewirkt REVERSED
    // keine sichtbare Änderung – die Zelle sähe dann aus wie ein normaler
    // Textzeichenträger und der Cursor wäre praktisch unsichtbar. Ein weißer
    // Block mit dunkler Schrift ist dagegen immer klar erkennbar.
    let cursor_style = Style::default().fg(STATUS_BG).bg(Color::White);

    let chars: Vec<char> = text_str.chars().collect();
    let display_end = chars.len().min(content_width);
    for (i, ch) in chars[..display_end].iter().enumerate() {
        let in_selection = sel_range.as_ref().is_some_and(|r| r.contains(&i));
        let is_cursor = cur_row == 0 && cur_off == i;
        let style = if in_selection {
            Style::default().fg(Color::White).bg(INPUT_BG)
        } else if is_cursor {
            cursor_style
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(ch.to_string(), style));
    }

    // Cursor am Ende des Textes (oder im leeren Feld): ein sichtbarer Block
    // hinter dem letzten Zeichen bzw. an Position 0.
    if cur_row == 0 && cur_off >= display_end {
        spans.push(Span::styled(" ", cursor_style));
    }

    let para = Paragraph::new(Line::from(spans)).style(Style::default().bg(STATUS_BG));
    f.render_widget(para, Rect::new(x, y + 1, width, 1));

    // Während das Pfad-Feld aktiv ist, gehört der Terminal-Cursor hierher –
    // sonst würde er (wie bei allen anderen Dialogen ohne Textinput) als
    // weiterhin leerer Platz in der Chat-Eingabezeile stehen bleiben.
    let cell_col = indent + cur_off;
    let cx = x + cell_col.min(width.saturating_sub(1) as usize) as u16;
    let cy = y + 1;
    f.set_cursor_position((cx, cy));
}

/// Eine Zeile der Kanal-Auswahl: Status-Indikator (⬢) in der Farbe des
/// Kanal-Zustands + Name. Die erste Zeile („kein Kanal“, `None`) zeigt einen
/// grauen, leeren Indikator. Reine Funktion (ohne Registry-Zugriff), damit
/// sie ohne Terminal getestet werden kann.
pub(crate) fn channel_picker_row(
    name: &str,
    status: Option<ChannelStatus>,
    cursor: bool,
    usage: &str,
) -> Line<'static> {
    let indicator_char = if status.is_some() { "⬢" } else { " " };
    let indicator = Span::styled(
        indicator_char,
        Style::default().fg(status.map(channel_status_color).unwrap_or(SYM_MUTED)),
    );
    let name_style = Style::default()
        .fg(if cursor {
            Color::White
        } else if name == "(kein Kanal)" {
            MUTED
        } else {
            ACCENT_FG
        })
        .add_modifier(if cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let mut spans = vec![indicator, Span::styled(format!(" {name}"), name_style)];
    if !usage.is_empty() {
        spans.push(Span::styled(
            format!("  {usage}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

/// Sonderzeile „new channel" im Channel-Picker: „+"-Indikator in Akzentfarbe,
/// um den Unterschied zu bestehenden Kanälen deutlich zu machen.
pub(crate) fn channel_picker_new_channel_row(cursor: bool) -> Line<'static> {
    let indicator = Span::styled("+", Style::default().fg(ACCENT));
    let name_style = Style::default()
        .fg(if cursor { Color::White } else { ACCENT_FG })
        .add_modifier(if cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    Line::from(vec![indicator, Span::styled(" new channel", name_style)])
}

/// Kurzer Hinweis, welche Sessions einen Kanal gerade nutzen – für die
/// Anzeige im Channel-Picker (z. B. „ · Session 1, 3 (aktiv)").
fn channel_usage(app: &App, name: &str) -> String {
    let mut nums: Vec<usize> = Vec::new();
    let mut active = false;
    for (i, s) in app.sessions.iter().enumerate() {
        let matches = s
            .channel
            .as_ref()
            .and_then(|ch| app.channels.find_name(ch))
            .as_deref()
            == Some(name);
        if matches {
            nums.push(i + 1);
            if s.phase == Phase::WaitingForLLM || s.phase == Phase::WaitingForTool || !s.open_tool_ids.is_empty() {
                active = true;
            }
        }
    }
    if nums.is_empty() {
        return String::new();
    }
    let list = nums
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if active {
        format!("· Session {list} (aktiv)")
    } else {
        format!("· Session {list}")
    }
}

/// Kürzt eine Zeile für die Dialog-Anzeige (Kommando usw.) auf `n` Zeichen.
fn clip(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Auswahlzeile eines Bestätigungsdialogs: markiert die aktive Option.
fn option_row(text: &str, cursor: bool) -> Line<'static> {
    let fg = if cursor { Color::White } else { ACCENT_FG };
    let style = Style::default().fg(fg).add_modifier(if cursor {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    Line::from(vec![
        Span::styled(if cursor { "▶" } else { " " }, Style::default()),
        Span::styled(format!(" {text}"), style),
    ])
}

/// Schätzt, wie viele sichtbare Zeilen eine `Line` bei der gegebenen Breite
/// einnimmt, wenn ratatui sie umbricht (`Wrap { trim: true }`). Die Schätzung
/// folgt der gleichen gierigen Wort-Trennung wie ratatuis `WordWrapper` (ein
/// Wort, das breiter als die Zeile ist, wird in mehrere Zeilen zerlegt) und
/// unterschätzt die tatsächliche Zeilenzahl nie – der Dialog wird also eher
/// etwas höher als zu klein (und damit abschneidend).
fn wrapped_row_count(line: &Line<'_>, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let width = width as usize;
    let mut text = String::new();
    for s in &line.spans {
        text.push_str(&s.content);
    }
    if text.is_empty() {
        return 1;
    }
    let mut rows = 1u16;
    let mut col = 0usize; // bereits belegte Spalten in der aktuellen Zeile
    for raw in text.split(' ') {
        let w = raw.chars().count();
        if w == 0 {
            continue; // Leerstellen-Reste ignorieren (trim verwirft sie ohnehin)
        }
        if col == 0 {
            // Wort beginnt am Zeilenanfang; ist es breiter als die Zeile, wird
            // es in mehrere Zeilen zerlegt.
            let chunks = w.div_ceil(width);
            rows += (chunks - 1) as u16;
            col = w % width;
            if col == 0 {
                col = width; // exakt passend: Zeile ist voll
            }
        } else if col + 1 + w <= width {
            col += 1 + w; // Leerzeichen + Wort passen noch in die Zeile
        } else {
            // Umbruch nötig: Wort (evtl. zerlegt) kommt in die nächste Zeile.
            rows += 1;
            let chunks = w.div_ceil(width);
            rows += (chunks - 1) as u16;
            col = w % width;
            if col == 0 {
                col = width;
            }
        }
    }
    rows
}

/// Gemeinsames Gerüst der zentrierten Bestätigungsdialoge (flächig, ohne
/// schwere Rahmen – wie die Kanal-Auswahl): Titel, Inhalt und Fußzeile.
///
/// Der Dialog ist bewusst großzügig breit und seine Höhe richtet sich nach der
/// tatsächlich benötigten Zeilenzahl (inkl. Umbrüche), damit lange
/// Überschriften/Texte – etwa die Container-Zeilen im Beenden-Dialog – nicht
/// rechts abgeschnitten oder unten weggeclippt werden.
fn confirmation_dialog(
    f: &mut Frame,
    title: &str,
    title_fg: Color,
    lines: Vec<Line<'static>>,
    footer: &[(&str, &str)],
) {
    let area = f.area();
    // Breit genug: möglichst viel der verfügbaren Breite nutzen, aber mit
    // vernünftigem Maximum und nie breiter als das Terminal selbst.
    let width = (area.width * 9 / 10)
        .clamp(56, 120)
        .min(area.width.saturating_sub(4));
    let pad = PICKER_PAD as u16;
    let inner_width = width.saturating_sub(pad * 2);

    // Gesamten Inhalt vorab zusammensetzen, damit wir die benötigte Höhe
    // (inkl. Umbrüchen) ermitteln können.
    let mut all = Vec::with_capacity(lines.len() + 2);
    all.push(Line::from(Span::styled(
        format!(" {title} "),
        Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
    )));
    all.extend(lines);
    all.push(key_help(footer, inner_width as usize));

    // Höhe an die (ggf. umbrochenen) Zeilen anpassen, statt pauschal eine
    // Zeile pro Eintrag anzunehmen – sonst werden lange Zeilen unten
    // abgeschnitten.
    let needed: u16 = all
        .iter()
        .map(|line| wrapped_row_count(line, inner_width))
        .sum();
    // +2: kleiner Puffer falls wrapped_row_count leicht unterschätzt.
    let height = (needed + 2).clamp(5, area.height.saturating_sub(2).max(5));

    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(STATUS_BG));
    f.render_widget(bg, rect);

    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );
    let para = Paragraph::new(all)
        .style(Style::default().bg(STATUS_BG))
        // Text umbrechen, statt ihn am rechten Rand abzuschneiden – lange
        // Überschriften/Texte (z. B. Container-Zeilen) laufen in Folgezeilen.
        .wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

/// Bestätigungsdialoge: VOR dem Absenden mit `execute` auf einem Local-Kanal
/// (drei Optionen – verantworten, einzeln bestätigen, abbrechen), die
/// Einzel-Bestätigung eines `run`-Aufrufs im ConfirmEach-Modus sowie das
/// Beenden bei wesentlichen Änderungen in selbst gestarteten Containern.
pub(crate) fn draw_confirmation(f: &mut Frame, app: &App) {
    if let Some(d) = &app.channel_close {
        match &d.phase {
            crate::app::ChannelClosePhase::ActiveConfirm { cursor } => {
                let options = ["Close anyway (active session running)", "Cancel"];
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(
                            " Channel \"{}\" is still in use by an active session.",
                            d.name
                        ),
                        Style::default().fg(ERROR_FG),
                    )),
                    Line::from(Span::styled(
                        " Closing it may interrupt that work.",
                        Style::default().fg(ERROR_FG),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, *cursor == i));
                }
                confirmation_dialog(
                    f,
                    "Close channel – active session?",
                    ERROR_FG,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
            crate::app::ChannelClosePhase::Worktree {
                summary,
                cursor,
                options,
            } => {
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(
                            " Worktree of channel \"{}\" has uncommitted changes.",
                            d.name
                        ),
                        Style::default().fg(ERROR_FG),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for note in summary.lines() {
                    lines.push(Line::from(Span::styled(
                        format!(" {note}"),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                lines.push(Line::from(Span::raw(" ")));
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, *cursor == i));
                }
                confirmation_dialog(
                    f,
                    "Worktree has uncommitted changes",
                    ERROR_FG,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
            crate::app::ChannelClosePhase::Container { notes, cursor } => {
                let options = ["Yes, close channel & stop container", "No, cancel"];
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(" Container of channel \"{}\" has unsaved changes.", d.name),
                        Style::default().fg(ERROR_FG),
                    )),
                    Line::from(Span::styled(
                        " Closing would stop it and this state would be lost.",
                        Style::default().fg(ERROR_FG),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for note in notes {
                    lines.push(Line::from(Span::styled(
                        format!("   • {note}"),
                        Style::default().fg(Color::White),
                    )));
                }
                lines.push(Line::from(Span::raw(" ")));
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, *cursor == i));
                }
                confirmation_dialog(
                    f,
                    "Container has unsaved changes",
                    ERROR_FG,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
        }
    }
    if let Some(d) = &app.stop_confirm {
        let options = ["Yes, quit & stop containers", "No, cancel"];
        let mut lines = vec![
            Line::from(Span::styled(
                " Self-started containers contain unsaved changes.",
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::styled(
                " Quitting would stop them and this state would be lost.",
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::raw(" ")),
        ];
        // Je betroffenem Container eine Überschrift (mit Name) und darunter
        // die einzelnen Befunde als Aufzählung – übersichtlicher als eine
        // einzelne, gekürzte Zeile pro Kanal.
        for entry in &d.entries {
            let header = match &entry.container {
                Some(c) => format!(" Container \"{}\" (channel \"{}\")", c, entry.channel),
                None => format!(" Channel \"{}\"", entry.channel),
            };
            lines.push(Line::from(Span::styled(
                header,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
            for note in &entry.notes {
                lines.push(Line::from(Span::styled(
                    format!("   • {note}"),
                    Style::default().fg(Color::White),
                )));
            }
            lines.push(Line::from(Span::raw(" ")));
        }
        if d.more > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "   … and {more} more {noun} with changes",
                    more = d.more,
                    noun = if d.more == 1 {
                        "container"
                    } else {
                        "containers"
                    }
                ),
                Style::default().fg(MUTED),
            )));
            lines.push(Line::from(Span::raw(" ")));
        }
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.cursor == i));
        }
        confirmation_dialog(f, "Really quit?", ERROR_FG, lines, &NAV_CONFIRM_KEYS);
        return;
    }
    if let Some(d) = &app.branch_confirm {
        let mut lines = vec![
            Line::from(Span::styled(
                format!(" {}", d.summary),
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in d.options.iter().enumerate() {
            lines.push(option_row(text, d.cursor == i));
        }
        confirmation_dialog(
            f,
            "/branch – branch already exists",
            ERROR_FG,
            lines,
            &NAV_CONFIRM_KEYS,
        );
        return;
    }
    if let Some(d) = &app.pre_send_confirm {
        let options = [
            "I take responsibility (e.g. sandbox)",
            "Send, but confirm each exec individually",
            "Cancel",
        ];
        let mut lines = vec![
            Line::from(Span::styled(
                " The model may execute commands directly on your local system.",
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::styled(
                " This can be dangerous – only send if you accept the consequences.",
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in options.iter().enumerate() {
            let line = option_row(text, d.cursor == i);
            lines.push(line);
        }
        confirmation_dialog(
            f,
            "Local execution – confirmation",
            ERROR_FG,
            lines,
            &NAV_CONFIRM_KEYS,
        );
        return;
    }
    if let Some(d) = &app.exec_confirm {
        let options = ["Yes, run", "No, decline"];
        let mut lines = vec![
            Line::from(Span::styled(
                " Run this command on the local system?",
                Style::default().fg(ERROR_FG),
            )),
            Line::from(Span::styled(
                format!("   {}", clip(&d.command, 60)),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("   {}", d.label),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.cursor == i));
        }
        confirmation_dialog(f, "Run command?", ERROR_FG, lines, &EXEC_KEYS);
    }
    if let Some(d) = &app.options_dialog {
        let mouse_label = if app.mouse_enabled {
            "Mouse capture: ON"
        } else {
            "Mouse capture: OFF"
        };
        let status_label = format!("Model: {}", app.display_model(app.active),);
        let options = [mouse_label, &status_label];
        let mut lines = vec![Line::from(Span::raw(" "))];
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.cursor == i));
        }
        confirmation_dialog(f, "Options", Color::Cyan, lines, &NAV_CONFIRM_KEYS);
    }
}

/// Zeigt die HTTP-Response-Header der letzten erfolgreichen LLM-Antwort der
/// aktiven Session (`Alt+H`). Reine Anzeige – Esc/Enter schließen wieder.
pub(crate) fn draw_http_headers_dialog(f: &mut Frame, app: &App) {
    if !app.http_headers_dialog {
        return;
    }
    let s = &app.sessions[app.active];
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &s.last_http_headers {
        Some(headers) if !headers.is_empty() => {
            for (name, value) in headers {
                lines.push(Line::from(Span::styled(
                    format!(" {name}: {value}"),
                    Style::default().fg(Color::White),
                )));
            }
        }
        _ => {
            lines.push(Line::from(Span::styled(
                " (no HTTP response captured yet for this session)",
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    confirmation_dialog(
        f,
        "HTTP headers of last response",
        Color::Cyan,
        lines,
        &[("esc", "close")],
    );
}
