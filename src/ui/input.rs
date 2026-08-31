//! Eingabe-Band am unteren Rand: Berechtigungs-Prompt, Textzeilen, Cursor.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::editor::InputLayout;
use crate::perm::Permission;

use super::{INPUT_BG, MUTED, PAD};

/// Farbliche Markierung einer Berechtigung – read=blau, write=gelb/amber,
/// execute=rot. Hebt Prompt, Eingabetext und User-Band im Chat hervor.
pub(crate) fn permission_color(p: Permission) -> Color {
    match p {
        Permission::Read => Color::Rgb(121, 182, 242),
        Permission::Write => Color::Rgb(238, 198, 93),
        Permission::Execute => Color::Rgb(240, 113, 120),
    }
}

/// Prompt-Bauplan der Eingabezeile: mit gebundenem Kanal das kurze
/// Berechtigungs-Label (`read > `, `write > `, `exec > `) und dessen Breite
/// (für Fortsetzungszeilen/Cursor). Ohne Kanal (`None`) gibt es keine
/// Berechtigung – der Prompt ist dann neutral (`> `, ohne Hinweis).
pub(crate) fn permission_prompt(p: Option<Permission>) -> (String, usize) {
    let label = match p {
        Some(p) => format!("{} > ", p.label()),
        None => "> ".to_string(),
    };
    let width = label.chars().count();
    (label, width)
}

pub(crate) fn draw_input(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    app: &App,
    layout: &InputLayout,
    show_cursor: bool,
) {
    let editor = &app.sessions[app.active].editor;
    let sel_range = editor.selected_range();
    let (cur_row, cur_off) = editor.cursor_row_col(layout);
    let pad = " ".repeat(PAD);
    // Ohne Kanal gibt es keine Berechtigung: neutraler Prompt (`> `) in
    // gedämpfter Farbe, statt des read/write/exec-Labels in Berechtigungsfarbe.
    let bound = app.sessions[app.active].channel.is_some();
    let color = if bound {
        permission_color(app.sessions[app.active].permission)
    } else {
        MUTED
    };
    let (label, _) = permission_prompt(bound.then_some(app.sessions[app.active].permission));

    let mut lines: Vec<Line> = Vec::with_capacity(layout.rows());
    for row in 0..layout.rows() {
        let mut spans: Vec<Span> = vec![Span::raw(pad.clone())];
        if row == 0 {
            // Berechtigungs-Prompt: nur das kurze Label, fett in der
            // Berechtigungsfarbe, damit der Userinput hervorgehoben ist.
            spans.push(Span::styled(
                label.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        } else {
            // Hängender Einzug: Fortsetzungszeile unter dem Text, nicht unter dem Prompt.
            spans.push(Span::raw(" ".repeat(layout.indent)));
        }
        for i in layout.row_range(row) {
            let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            if let Some(range) = &sel_range {
                if range.contains(&i) {
                    style = style.add_modifier(Modifier::REVERSED);
                }
            }
            spans.push(Span::styled(editor.text()[i].to_string(), style));
        }
        lines.push(Line::from(spans));
    }

    let para = Paragraph::new(lines).style(Style::default().bg(INPUT_BG));
    f.render_widget(para, area);

    // Cursor-Position (Spalte relativ zum Text, zzgl. Abstand + Prompt).
    // Solange ein Dialog offen ist (`show_cursor == false`), wird der Cursor
    // hier nicht gesetzt – ratatui versteckt ihn dann nach dem Frame, statt
    // dass er in der Eingabezeile stehen bleibt. Die Ausnahme (Dialog mit
    // eigenem Textinput) setzt den Cursor separat in `draw_channel_builder`.
    if show_cursor {
        let cell_col = PAD + layout.indent + cur_off;
        let x = area.x + cell_col.min(area.width.saturating_sub(1) as usize) as u16;
        let y = area.y + (cur_row as u16).min(area.height.saturating_sub(1));
        f.set_cursor_position((x, y));
    }
}
