//! Eingabe-Band am unteren Rand: Berechtigungs-Prompt, Textzeilen, Cursor.

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::App;
use crate::editor::InputLayout;
use crate::perm::Permission;

use super::{theme, PAD};

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
    // Ohne Kanal gibt es keine Berechtigung: neutraler Prompt (`> `) in
    // gedämpfter Farbe, statt des read/write/exec-Labels in Berechtigungsfarbe.
    let bound = app.sessions[app.active].channel.is_some();
    let color = if bound {
        permission_color(app.sessions[app.active].permission)
    } else {
        theme().muted
    };
    let (label, _) = permission_prompt(bound.then_some(app.sessions[app.active].permission));

    let lines = build_input_lines(editor, layout, &label, color, sel_range);

    let para = Paragraph::new(lines).style(Style::default().bg(theme().band_bg));
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

/// Baut die sichtbaren Textzeilen des Eingabefelds aus dem Umbruch-Layout.
/// Je `layout`-Zeile eine `Line` (`<PAD>`, Prompt bzw. hängender Einzug, dann
/// die Zeichen des Zeilenbereichs). Selektion wird invertiert dargestellt.
///
/// Als eigene Funktion, damit die Darstellung mehrzeiliger Eingaben direkt
/// getestet werden kann (das Layout schneidet explizite `\n` aus den Bereichen
/// heraus; `\n` wird hier NIE als Zeichen in eine Zeile gerendert).
fn build_input_lines(
    editor: &crate::editor::Editor,
    layout: &InputLayout,
    label: &str,
    color: Color,
    sel_range: Option<Range<usize>>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::with_capacity(layout.rows());
    for row in 0..layout.rows() {
        let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(PAD))];
        if row == 0 {
            // Berechtigungs-Prompt: nur das kurze Label, fett in der
            // Berechtigungsfarbe, damit der Userinput hervorgehoben ist.
            spans.push(Span::styled(
                label.to_string(),
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
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Zeichnet den Eingabe-Inhalt `text` genau wie `draw_input` (Prompt `> `,
    /// PAD, hängender Einzug) in einen Test-Screen und liefert die Zellen je Zeile.
    fn render_input(text: &str, width: u16, height: u16) -> Vec<String> {
        let mut ed = Editor::new(width as usize);
        ed.insert_snippet(text);
        ed.set_indent(2); // "> "
        let layout = ed.layout();
        let lines = build_input_lines(&ed, &layout, "> ", Color::White, None);
        let para = Paragraph::new(lines).style(Style::default().bg(theme().band_bg));
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("TestBackend");
        term.draw(|f| f.render_widget(para, f.area()))
            .expect("render");
        let buf = term.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.get(x, y).symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// Kern der Frage „Wird mehrzeilige Eingabe im Feld dargestellt?“:
    /// Ein eingefügter Text mit `\n` MUSS als mehrere sichtbare Zeilen erscheinen –
    /// das `\n` selbst ist dabei nirgends als Zeichen zu sehen.
    #[test]
    fn mehrzeiliger_paste_erscheint_als_mehrere_zeilen() {
        let rows = render_input("aaa\nbbb", 20, 4);
        for (y, r) in rows.iter().enumerate() {
            println!("row {y}: {r:?}");
        }
        // Zeile 0: "<PAD><Prompt>aaa", Zeile 1: "<PAD>  bbb".
        assert_eq!(&rows[0][0..4], "  > ");
        assert_eq!(&rows[0][4..7], "aaa");
        assert_eq!(&rows[1][0..4], "    ");
        assert_eq!(&rows[1][4..7], "bbb");
        // Kein `\n` als sichtbares Zeichen irgendwo.
        assert!(rows.iter().all(|r| !r.contains('\n')));
    }

    /// Auch CRLF/CR aus der Zwischenablage entsteht durch die Normalisierung in
    /// `insert_snippet` ein sichtbarer Umbruch (zwei getrennte Zeilen) statt
    /// einer endlosen Zeile.
    #[test]
    fn crlf_paste_zeigt_zeilenumbruch() {
        let rows = render_input("aaa\r\nbbb", 20, 4);
        for (y, r) in rows.iter().enumerate() {
            println!("row {y}: {r:?}");
        }
        assert_eq!(&rows[0][4..7], "aaa");
        assert_eq!(&rows[1][4..7], "bbb");
        assert_ne!(rows[0], rows[1]);
    }

    /// Alleinstehendes `\r` (Alt-Mac-Stil) erzeugt ebenfalls eine sichtbare
    /// zweite Zeile – konsistent mit dem Chat, der an CR umbricht.
    #[test]
    fn cr_paste_zeigt_zeilenumbruch() {
        let rows = render_input("aaa\rbbb", 20, 4);
        assert_eq!(&rows[0][4..7], "aaa");
        assert_eq!(&rows[1][4..7], "bbb");
    }
}
