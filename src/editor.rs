use std::ops::Range;

/// Umbruchstruktur des (mehrzeiligen) Eingabefelds.
///
/// Zeile 0 beginnt mit dem Prompt (`> `), alle Folgezeilen werden mit einem
/// hängenden Einzug (`indent`) unter dem eigentlichen Text ausgerichtet.
///
/// `ranges` beschreibt die tatsächlich gerenderten Zeichenbereiche je Zeile;
/// Whitespace an Umbruchstellen gehört keiner Zeile an und wird übersprungen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLayout {
    /// Terminal-Breite in Zellen.
    pub width: usize,
    /// Zellen, die der Prompt (Zeile 0) bzw. der hängende Einzug belegt.
    pub indent: usize,
    /// Verfügbare Zellen pro Zeile für den eigentlichen Text.
    pub content: usize,
    /// Gerenderter Zeichenbereich je Zeile (ohne Whitespace-Lücken).
    pub ranges: Vec<Range<usize>>,
    pub total: usize,
}

impl InputLayout {
    pub fn rows(&self) -> usize {
        self.ranges.len()
    }

    pub fn row_range(&self, row: usize) -> Range<usize> {
        self.ranges[row].clone()
    }
}

/// Mehrzeiliges Eingabefeld mit Cursor, Selektion und Wort-Umbruch.
pub struct Editor {
    text: Vec<char>,
    cursor: usize,
    sel: Option<usize>,
    width: usize,
    /// Zellen, die der Prompt (Zeile 0) bzw. der hängende Einzug belegen –
    /// dynamisch, z. B. für den Berechtigungs-Prompt (`read> …`).
    indent: usize,
}

const INDENT: usize = 2; // "> "

impl Editor {
    pub fn new(width: usize) -> Self {
        Self {
            text: Vec::new(),
            cursor: 0,
            sel: None,
            width,
            indent: INDENT,
        }
    }

    pub fn text(&self) -> &[char] {
        &self.text
    }

    pub fn text_string(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set_width(&mut self, w: usize) {
        self.width = w;
    }

    /// Setzt die Breite des Prompts bzw. des hängenden Einzugs (Zellen), damit
    /// Fortsetzungszeilen und Cursor unter dem eigentlichen Text ausgerichtet.
    pub fn set_indent(&mut self, indent: usize) {
        self.indent = indent.max(1);
    }

    /// Leert den Input und setzt Cursor/Selektion zurück.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.sel = None;
    }

    /// Überschreibt den gesamten Inhalt und setzt den Cursor ans Ende – für das
    /// Laden einer vergangenen Eingabe aus der Historie.
    pub fn set_text(&mut self, s: &str) {
        self.text = s.chars().collect();
        self.cursor = self.text.len();
        self.sel = None;
    }

    /// Normalisierter markierter Bereich `[lo, hi)`. Tippen ersetzt ihn.
    pub fn selected_range(&self) -> Option<Range<usize>> {
        self.sel.map(|anchor| {
            let lo = anchor.min(self.cursor);
            let hi = anchor.max(self.cursor);
            lo..hi
        })
    }

    fn delete_selection(&mut self) {
        if let Some(range) = self.selected_range() {
            if !range.is_empty() {
                self.text.drain(range.clone());
            }
            self.cursor = range.start;
            self.sel = None;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        let at = self.cursor.min(self.text.len());
        self.text.insert(at, c);
        self.cursor = at + 1;
    }

    pub fn backspace(&mut self) {
        if self.sel.is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.text.remove(self.cursor);
    }

    pub fn delete_at_cursor(&mut self) {
        if self.sel.is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        self.text.remove(self.cursor);
    }

    pub fn move_cursor(&mut self, delta: isize, select: bool) {
        if !select {
            self.sel = None;
        } else if self.sel.is_none() {
            self.sel = Some(self.cursor);
        }
        let len = self.text.len() as isize;
        let next = (self.cursor as isize + delta).clamp(0, len) as usize;
        self.cursor = next;
    }

    pub fn move_home(&mut self, select: bool) {
        if !select {
            self.sel = None;
        } else if self.sel.is_none() {
            self.sel = Some(self.cursor);
        }
        self.cursor = 0;
    }

    pub fn move_end(&mut self, select: bool) {
        if !select {
            self.sel = None;
        } else if self.sel.is_none() {
            self.sel = Some(self.cursor);
        }
        self.cursor = self.text.len();
    }

    /// Bewegt den Cursor zeilenweise im umbrochenen Input (↑/↓).
    pub fn move_line(&mut self, dir: isize, select: bool) {
        if !select {
            self.sel = None;
        } else if self.sel.is_none() {
            self.sel = Some(self.cursor);
        }
        let layout = self.layout();
        let (row, off) = self.cursor_row_col(&layout);
        let new_row = match dir {
            d if d < 0 => {
                if row == 0 {
                    return;
                }
                row - 1
            }
            _ => {
                if row + 1 >= layout.rows() {
                    return;
                }
                row + 1
            }
        };
        let range = layout.row_range(new_row);
        let len = range.len();
        let goal = off.min(len.saturating_sub(1));
        self.cursor = range.start + goal;
    }

    /// Wortweises Springen: dir > 0 → vorwärts, dir < 0 → rückwärts.
    pub fn move_word(&mut self, dir: isize, select: bool) {
        if !select {
            self.sel = None;
        } else if self.sel.is_none() {
            self.sel = Some(self.cursor);
        }
        let n = self.text.len();
        let is_ws = |i: usize| self.text[i].is_whitespace();
        match dir {
            d if d > 0 => {
                let mut i = self.cursor;
                if i < n && !is_ws(i) {
                    while i < n && !is_ws(i) {
                        i += 1;
                    }
                }
                while i < n && is_ws(i) {
                    i += 1;
                }
                self.cursor = i;
            }
            _ => {
                let mut i = self.cursor;
                while i > 0 && is_ws(i - 1) {
                    i -= 1;
                }
                while i > 0 && !is_ws(i - 1) {
                    i -= 1;
                }
                self.cursor = i;
            }
        }
    }

    /// Umbruch-Layout für die aktuelle Breite (Wort-Umbruch an Whitespaces).
    pub fn layout(&self) -> InputLayout {
        let width = self.width.max(1);
        let content = width.saturating_sub(self.indent).max(1);
        let ranges = wrap_ranges(&self.text, content);
        InputLayout {
            width,
            indent: self.indent,
            content,
            ranges,
            total: self.text.len(),
        }
    }

    /// Zeile (`row`) und Spalten-Offset des Cursors relativ zum Text (ohne Einzug).
    pub fn cursor_row_col(&self, layout: &InputLayout) -> (usize, usize) {
        let Some(_last) = layout.ranges.last() else {
            return (0, 0);
        };
        let c = self.cursor.min(layout.total);
        for (r, range) in layout.ranges.iter().enumerate() {
            if c < range.end {
                return (r, c.saturating_sub(range.start));
            }
        }
        let last = layout.ranges.last().expect("mindestens eine Zeile");
        (layout.ranges.len() - 1, c.saturating_sub(last.start))
    }

    /// Steht der Cursor in der ersten (obersten) umbrochenen Zeile?
    pub fn cursor_is_first_row(&self) -> bool {
        let layout = self.layout();
        self.cursor_row_col(&layout).0 == 0
    }

    /// Steht der Cursor in der letzten (untersten) umbrochenen Zeile?
    pub fn cursor_is_last_row(&self) -> bool {
        let layout = self.layout();
        let (row, _) = self.cursor_row_col(&layout);
        row + 1 >= layout.rows()
    }
}

/// Bricht eine Zeichenfolge an Whitespace-Grenzen zu Zeilen-Ranges der Breite
/// `width` um. Whitespace an Umbruchstellen wird übersprungen (gehört keiner
/// Zeile an). Einzelne lange Wörter weichen auf Trennzeichen (`-`, `/`, `.`)
/// aus; gibt es keins, wird blind gebrochen (die Folgezeile wird gefüllt).
fn wrap_ranges(chars: &[char], width: usize) -> Vec<Range<usize>> {
    let n = chars.len();
    let mut ranges = Vec::new();
    if n == 0 {
        // Leerer Input: eine leere Zeile (für den Prompt) statt keine Zeile.
        return vec![Range { start: 0, end: 0 }];
    }
    let mut i = 0usize;
    while i < n {
        if i + width >= n {
            // Rest passt in eine Zeile.
            ranges.push(i..n);
            break;
        }
        let end = i + width;
        // Letzter Whitespace-Index im Fenster [i, end): dort umbrechen.
        let mut k = end - 1;
        while k > i && !chars[k].is_whitespace() {
            k -= 1;
        }
        let (line_end, next) = if chars[k].is_whitespace() {
            // Whitespace bei k gehört keiner Zeile an.
            let mut next = k;
            while next < n && chars[next].is_whitespace() {
                next += 1;
            }
            (k, next)
        } else {
            // Kein Whitespace im Fenster → auf Trennzeichen (-, /, .) ausweichen
            // (bleibt sichtbar, dahinter wird getrennt), sonst blind brechen.
            match (i..end)
                .rev()
                .find(|&p| matches!(chars[p], '-' | '/' | '.'))
            {
                Some(p) => (p + 1, p + 1),
                None => (end, end),
            }
        };
        if line_end > i {
            ranges.push(i..line_end);
        }
        i = if next > i { next } else { end };
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed() -> Editor {
        Editor::new(80)
    }

    fn type_text(e: &mut Editor, s: &str) {
        for c in s.chars() {
            e.insert_char(c);
        }
    }

    fn text(e: &Editor) -> String {
        e.text_string()
    }

    #[test]
    fn einfuegen_am_ende() {
        let mut e = ed();
        type_text(&mut e, "abc");
        assert_eq!(text(&e), "abc");
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn cursor_bewegt_und_fuegt_mitte_ein() {
        let mut e = ed();
        type_text(&mut e, "abcd");
        e.move_cursor(-2, false);
        e.insert_char('X');
        assert_eq!(text(&e), "abXcd");
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn backspace_und_delete() {
        let mut e = ed();
        type_text(&mut e, "abcd");
        e.move_cursor(-1, false); // vor 'd'
        e.backspace(); // 'c' entfernen
        assert_eq!(text(&e), "abd");
        assert_eq!(e.cursor, 2);
        e.move_cursor(-1, false); // vor 'b'
        e.delete_at_cursor(); // 'b' entfernen
        assert_eq!(text(&e), "ad");
        assert_eq!(e.cursor, 1);
    }

    #[test]
    fn selektion_wird_ersetzt() {
        let mut e = ed();
        type_text(&mut e, "abXYcd");
        e.move_cursor(-2, false);
        e.move_cursor(-1, true);
        e.move_cursor(-1, true);
        assert!(e.selected_range() == Some(2..4));
        e.insert_char('z');
        assert_eq!(text(&e), "abzcd");
        assert_eq!(e.cursor, 3);
        assert!(e.sel.is_none());
    }

    #[test]
    fn home_end_und_clear() {
        let mut e = ed();
        type_text(&mut e, "abc");
        e.move_home(false);
        assert_eq!(e.cursor, 0);
        e.move_end(false);
        assert_eq!(e.cursor, 3);
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn wortnavigation() {
        let mut e = ed();
        type_text(&mut e, "foo bar  baz");
        e.move_home(false);
        e.move_word(1, false);
        assert_eq!(e.cursor, 4);
        e.move_word(1, false);
        assert_eq!(e.cursor, 9);
        e.move_word(-1, false);
        assert_eq!(e.cursor, 4);
        e.move_word(-1, false);
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn umbruch_an_whitespace() {
        let mut e = ed();
        e.set_width(8); // indent 2 → content 6
        type_text(&mut e, "foo bar baz");
        let layout = e.layout();
        assert_eq!(layout.content, 6);
        assert_eq!(layout.row_range(0), 0..3); // "foo"
        assert_eq!(layout.row_range(1), 4..7); // "bar"
        assert_eq!(layout.row_range(2), 8..11); // "baz"
        assert_eq!(layout.rows(), 3);
    }

    #[test]
    fn hart_umbruch_langes_wort() {
        let mut e = ed();
        e.set_width(6); // content 4
        type_text(&mut e, "abcdefg");
        let layout = e.layout();
        assert_eq!(layout.row_range(0), 0..4);
        assert_eq!(layout.row_range(1), 4..7);
        assert_eq!(layout.rows(), 2);
    }

    #[test]
    fn langes_wort_weicht_auf_trennzeichen_aus() {
        let mut e = ed();
        e.set_width(10); // content 8
        type_text(&mut e, "abc-def-ghi");
        let layout = e.layout();
        assert_eq!(layout.row_range(0), 0..8); // "abc-def-" – Trennzeichen bleibt
        assert_eq!(layout.row_range(1), 8..11); // "ghi"
        assert_eq!(layout.rows(), 2);
    }

    #[test]
    fn leerer_input_kollabiert_nicht() {
        let mut e = ed();
        e.set_width(10);
        assert_eq!(e.layout().rows(), 1); // eine leere Zeile
                                          // Darf nicht abstürzen, muss stabile Position liefern.
        assert_eq!(e.cursor_row_col(&e.layout()), (0, 0));
    }

    #[test]
    fn cursor_zeile_und_spalte() {
        let mut e = ed();
        e.set_width(6); // content 4
        type_text(&mut e, "abcdefg");
        e.cursor = 0;
        assert_eq!(e.cursor_row_col(&e.layout()), (0, 0));
        e.cursor = 4;
        assert_eq!(e.cursor_row_col(&e.layout()), (1, 0));
        e.cursor = 6;
        assert_eq!(e.cursor_row_col(&e.layout()), (1, 2));
    }

    #[test]
    fn move_line_navigiert() {
        let mut e = ed();
        e.set_width(6); // content 4
        type_text(&mut e, "abcdefg");
        e.cursor = 4; // Zeile 1, Offset 0
        e.move_line(-1, false); // hoch in Zeile 0
        assert_eq!(e.cursor, 0);
        e.move_line(1, false); // runter zurück
        assert_eq!(e.cursor, 4);
    }
}
