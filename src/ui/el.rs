//! `ElBackend` – Backend-Wrapper, der jede Zeile nach dem letzten Inhalt mit
//! `ESC[K` (Erase-to-End-of-Line) abschließt.
//!
//! Problem: Ratatui füllt beim Rendering die Zeilen bis zur Terminalbreite mit
//! Leerzeichen-Zellen. Beim Markieren/Kopieren im Terminal wird dann pro Zeile
//! ein Rechteck inkl. dieser Trailing-Spaces plus Umbruch mitgenommen. Mit
//! `ESC[K` endet die gespeicherte Terminalzeile am letzten echten Zeichen –
//! Auswahl/Kopie wird zeilenbewusst.
//!
//! Sicherheit: Eine Zeile wird nur gekürzt, wenn rechts vom Inhalt ausschließlich
//! leere Zellen mit Terminal-Default-Hintergrund stehen. Zeilen mit gemaltem
//! Hintergrund rechts vom Inhalt (Status-/Band-Zeilen, Box-Hintergründe) bleiben
//! unangetastet (sonst würde `ESC[K` das gemalte Hintergrund-BG löschen).
//! Wide-Zeichen-Folgezellen (`CellDiffOption::Skip`) zählen als Inhalt, damit
//! `ESC[K` nie in ein breites Zeichen hineinschneidet.
//!
//! Der Wrapper rendert wie `CrosstermBackend<Stdout>` und schreibt die `ESC[K`-
//! Sequenzen über ein zweites `Stdout`-Handle in denselben (globalen, von Rust
//! geteilten) Terminal-Puffer → die Reihenfolge bleibt korrekt (Zellen erst,
//! Abschluss danach) und der Frame-Flush spült beides zusammen.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType as TermClear};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::layout::{Position, Size};
use ratatui::style::Color;

/// Kompakte Erfassung einer Bildschirmzelle für den Zeilenabschluss.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CellInfo {
    /// Inhalt (nicht-leer bzw. kein Wide-Zeichen-Folgezell).
    blank: bool,
    /// Hintergrund = Terminal-Default (nicht explizit gemalt).
    bg_default: bool,
}

/// Nicht gesetzte Zelle = leer + Terminal-Default-Hintergrund.
impl Default for CellInfo {
    fn default() -> Self {
        CellInfo {
            blank: true,
            bg_default: true,
        }
    }
}

impl CellInfo {
    fn from_cell(c: &Cell) -> Self {
        CellInfo {
            blank: c.symbol() == " " && c.diff_option != CellDiffOption::Skip,
            bg_default: c.bg == Color::Reset,
        }
    }
}

/// Berechnet je **berührter** Zeile, an welcher Spalte sie per `ESC[K`
/// abgeschlossen werden soll: Spalte = letzter Inhalt + 1. Nur Zeilen, deren
/// Rest nach dem Inhalt leer + Default-Hintergrund ist.
///
/// Wichtig: Nur Zeilen, die der aktuelle Frame-Diff berührt (`touched`), werden
/// beendet – unberührte Zeilen behalten ihren (bereits korrekten) Terminal-
/// Zustand vom Frame ihres letzten Zeichnens. Ohne diese Einschränkung würde
/// zum Beispiel das (nur beim Start gezeichnete) Logo bei einem späteren Tippen-
/// Frame fälschlich an der kleinen, getippten Breite abgeschnitten. `width` ist
/// die bekannte Terminalbreite (max. je berührte Spalte) – der guard „x + 1 <
/// width“ stellt sicher, dass rechts vom Inhalt noch (leere) Zellen existieren,
/// auch wenn die Snapshot-Zeile selbst nur bis zur berührten Kolonne reicht.
/// (Pur/testbar; der Wrapper pflegt `cells` aus dem Diff.)
fn plan_els(
    cells: &[Vec<CellInfo>],
    touched: &[bool],
    max_y: usize,
    width: usize,
) -> Vec<(u16, u16)> {
    let mut els = Vec::new();
    for y in 0..=max_y {
        if !touched.get(y).copied().unwrap_or(false) {
            continue;
        }
        let Some(row) = cells.get(y) else { continue };
        let mut last_content: Option<usize> = None;
        let mut painted_bg = false;
        for x in (0..row.len()).rev() {
            let info = row[x];
            if info.blank {
                if !info.bg_default {
                    painted_bg = true;
                    break;
                }
                continue;
            }
            last_content = Some(x);
            break;
        }
        if let Some(x) = last_content {
            // Nur kürzen, wenn rechts vom Inhalt eine (leere, Default-)Zelle
            // folgt und kein Hintergrund-BG gemalt ist.
            if x + 1 < width && !painted_bg {
                els.push((x as u16 + 1, y as u16));
            }
        }
    }
    els
}

/// Backend-Wrapper um [`CrosstermBackend<Stdout>`]: rendert wie das Original
/// und hängt je Frame `ESC[K` an die Inhaltszeilen an (siehe Modul-Doku).
pub(crate) struct ElBackend {
    inner: CrosstermBackend<io::Stdout>,
    /// Zweites `Stdout`-Handle für die `ESC[K`-Sequenzen (teilt den globalen
    /// Terminal-Puffer mit `inner` → korrekte Reihenfolge im Frame-Flush).
    out: io::Stdout,
    /// Vereinfachte Kopie der zuletzt gerenderten Bildschirmzellen (nur
    /// `blank`/`bg_default`), damit die EL-Entscheidung auch ohne den aktuellen
    /// Frame-Diff den rechten Rand jeder Zeile kennt.
    cells: Vec<Vec<CellInfo>>,
    /// Bekannte Terminalbreite: die größte je berührte Spalte. Bestimmt, ob
    /// rechts vom Inhalt überhaupt noch (leere) Zellen existieren.
    width: usize,
}

impl ElBackend {
    pub(crate) fn new(inner: CrosstermBackend<io::Stdout>) -> Self {
        ElBackend {
            inner,
            out: io::stdout(),
            cells: Vec::new(),
            width: 0,
        }
    }

    /// Hängt die `ESC[K`-Zeilenabschlüsse für die Zeilen an, die dieser Frame
    /// berührt hat (`touched` = Zeilen mit Diff-Zelle, `max_y` = letzte berührte).
    fn emit_row_ends(&mut self, touched: Vec<bool>, max_y: usize) -> io::Result<()> {
        let els = plan_els(&self.cells, &touched, max_y, self.width);
        if els.is_empty() {
            return Ok(());
        }
        for (x, y) in els {
            queue!(self.out, MoveTo(x, y), Clear(TermClear::UntilNewLine))?;
        }
        Ok(()) // Flush übernimmt der Terminal-Wrapper am Frame-Ende.
    }
}

/// Wendet einen Frame-Diff auf den Zell-Snapshot an (wächst bei Bedarf) und
/// liefert, welche Zeilen der Diff berührt hat (`touched[y]`) samt letzter
/// berührter Zeile `max_y` und größter berührter Spalte `max_x`. (Getrennt,
/// damit das Snapshot-Handling mit echten `Cell`s testbar ist.)
fn update_snapshot(
    cells: &mut Vec<Vec<CellInfo>>,
    diff: &[(u16, u16, &Cell)],
) -> (usize, usize, Vec<bool>) {
    let (mut max_x, mut max_y) = (0usize, 0usize);
    let mut touched: Vec<bool> = Vec::new();
    for &(x, y, cell) in diff {
        let (xi, yi) = (x as usize, y as usize);
        max_x = max_x.max(xi);
        max_y = max_y.max(yi);
        if yi >= cells.len() {
            cells.resize_with(yi + 1, Vec::new);
        }
        let row = &mut cells[yi];
        if xi >= row.len() {
            row.resize(xi + 1, CellInfo::default());
        }
        row[xi] = CellInfo::from_cell(cell);
        if yi >= touched.len() {
            touched.resize(yi + 1, false);
        }
        touched[yi] = true;
    }
    (max_x, max_y, touched)
}

impl Backend for ElBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Diff einmal sammeln: für den Snapshot UND zum Delegieren an den
        // Crossterm-Backend (der Iterator ist nicht clonbar).
        let cells: Vec<(u16, u16, &Cell)> = content.collect();
        let (max_x, max_y, touched) = update_snapshot(&mut self.cells, &cells);
        self.width = self.width.max(max_x);

        self.inner.draw(cells.into_iter())?;
        self.emit_row_ends(touched, max_y)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.cells.clear();
        self.width = 0;
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        // Spült den geteilten globalen Puffer inkl. der zuvor gequeueten
        // `ESC[K`-Sequenzen (Zellen + Abschluss in korrekter Reihenfolge).
        Write::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alle Zeilen 0..=`max_y` als berührt markieren.
    fn all_touched(max_y: usize) -> Vec<bool> {
        vec![true; max_y + 1]
    }

    #[test]
    fn richtiger_zeilenabschluss_nach_letztem_inhalt() {
        // "hello" bei (0,0) und (4,0); Rest der Zeile leer + Default → EL an Spalte 5.
        let mut cells = vec![vec![CellInfo::default(); 10]; 3];
        cells[0][0] = CellInfo {
            blank: false,
            bg_default: true,
        };
        cells[0][4] = CellInfo {
            blank: false,
            bg_default: true,
        };
        cells[1][0] = CellInfo {
            blank: false,
            bg_default: true,
        };
        let touched = all_touched(2);
        let els = plan_els(&cells, &touched, 2, 10);
        assert!(els.contains(&(5, 0)), "Zeile 0 nach 'hello' abgeschlossen: {els:?}");
        assert!(els.contains(&(1, 1)), "Zeile 1 nur bei Spalte 0 → EL bei 1: {els:?}");
        // Zeile 2: ganz leer → kein Abschluss.
        assert!(!els.iter().any(|&(_, y)| y == 2), "{els:?}");
    }

    #[test]
    fn unberuehrte_zeile_wird_nicht_gekuerzt() {
        // Regression fürs Logo: Zeile 0 hat (vom Start-Frame) Inhalt bis Spalte
        // 30; der AKTUELLE Frame berührt sie nicht (touched[0] = false), sondern
        // nur Zeile 1 (z. B. getippte Eingabezeile, kleine Breite). Ohne die
        // touched-Einschränkung würde plan_els jetzt an der kleinen Breite
        // schneiden und den Logo-Inhalt jenseits davon löschen.
        let mut cells = vec![vec![CellInfo::default(); 40]; 2];
        cells[0][..10].fill(CellInfo {
            blank: false,
            bg_default: true,
        });
        cells[1][0] = CellInfo {
            blank: false,
            bg_default: true,
        };
        let touched = vec![false, true];
        let els = plan_els(&cells, &touched, 1, 40);
        assert!(
            !els.iter().any(|&(_, y)| y == 0),
            "unberührte Logo-Zeile bleibt unangetastet: {els:?}"
        );
        assert_eq!(els, vec![(1, 1)], "nur die getippte Zeile wird beendet: {els:?}");
    }

    #[test]
    fn hintergrund_zeile_wird_nicht_gekuerzt() {
        // Gemalter Hintergrund rechts vom Inhalt: ESC[K würde das BG löschen.
        let mut row = vec![CellInfo::default(); 6];
        row[0] = CellInfo {
            blank: false,
            bg_default: true,
        };
        row[1] = CellInfo {
            blank: true,
            bg_default: false, // gemaltes BG (z. B. status_bg)
        };
        let cells = vec![row];
        assert!(plan_els(&cells, &[true], 0, 6).is_empty(), "kein EL bei gemaltem BG rechts");
    }

    #[test]
    fn wide_zeichen_folgezelle_zaehlt_als_inhalt() {
        // Breites Zeichen bei (4,0), Folgezelle (diff Skip) bei (5,0) → EL erst
        // bei 6, damit ESC[K nicht ins breite Zeichen schneidet.
        let mut row = vec![CellInfo::default(); 8];
        row[..4].fill(CellInfo {
            blank: true,
            bg_default: true,
        });
        row[4] = CellInfo {
            blank: false,
            bg_default: true,
        };
        row[5] = CellInfo {
            blank: false, // Skip-Zelle zählt als Inhalt
            bg_default: true,
        };
        let cells = vec![row];
        assert_eq!(plan_els(&cells, &[true], 0, 8), vec![(6, 0)]);
    }

    #[test]
    fn snapshot_aus_echten_cells_und_diff_ergibt_el() {
        use ratatui::buffer::Cell;
        // Zeile 0: "ab" bei Spalten 0-1 plus eine (gemalte-freie) Leerzelle bei
        // 2 → EL bei 2. Zeile 1: ein Zeichen bei 0 → EL bei 1.
        let c = |s: &str, skip: bool| {
            let mut cell = Cell::default();
            cell.set_symbol(s);
            if skip {
                cell.set_diff_option(CellDiffOption::Skip);
            }
            cell
        };
        let cells: Vec<Cell> = vec![c("a", false), c("b", false), c(" ", false), c("x", false)];
        let diff: Vec<(u16, u16, &Cell)> = vec![
            (0, 0, &cells[0]),
            (1, 0, &cells[1]),
            (2, 0, &cells[2]), // blank → größte berührte Spalte = 2
            (0, 1, &cells[3]),
        ];
        let mut snap: Vec<Vec<CellInfo>> = Vec::new();
        let (max_x, max_y, touched) = update_snapshot(&mut snap, &diff);
        assert_eq!((max_x, max_y), (2, 1));
        assert_eq!(touched, vec![true, true]);
        // width = max_x + 1: rechts vom Inhalt in Zeile 1 existiert (leere) Zelle.
        let els = plan_els(&snap, &touched, max_y, max_x + 1);
        assert!(els.contains(&(2, 0)), "Zeile 0 nach 'ab' → {els:?}");
        assert!(els.contains(&(1, 1)), "Zeile 1 nach Zeichen → {els:?}");
    }
}