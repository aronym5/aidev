//! `ElBackend` – Backend-Wrapper, der jede Zeile (und jede Leerzeile) mit
//! `ESC[K` (Erase-to-End-of-Line) abschließt.
//!
//! Problem: Ratatui füllt beim Rendering die Zeilen bis zur Terminalbreite mit
//! Leerzeichen-Zellen. Beim Markieren/Kopieren im Terminal wird dann pro Zeile
//! ein Rechteck inkl. dieser Trailing-Spaces plus Umbruch mitgenommen. Mit
//! `ESC[K` endet die gespeicherte Terminalzeile am letzten echten Zeichen –
//! Auswahl/Kopie wird zeilenbewusst.
//!
//! ## Gemalte Flächen (Code-/Konsolen-Bänder, User-Eingabe)
//!
//! `ESC[K` löscht mit dem *aktuellen* SGR-Hintergrund (BCE, Background Color
//! Erase) – die gelöschten Zellen behalten also ihre Farbe. Deshalb gilt die
//! Zeile auch dort als "beendet", wo rechts vom Text eine gemalte Fläche folgt
//! (schwarzes Codeband, Box-/Dialog-Hintergrund, Eingabe-Band der
//! User-Aussage): Vor dem `ESC[K` wird der Hintergrund des jeweiligen Laufs
//! gesetzt, der Schwanz wird in Läufe gleicher Hintergrundfarbe zerlegt und
//! jeder Lauf einzeln abgeschlossen. Weil dabei jede Zelle exakt die Farbe
//! bekommt, die sie im Buffer bereits hat, ist das Bild pixelgleich – nur die
//! Terminalzeile endet früher. Das betrifft auch ein Band über die **volle**
//! Breite (User-Aussage, Eingabe-/Statuszeile): sein letzter Lauf malt dieselbe
//! Farbe bis zum Rand, die er dort ohnehin schon hat – sichtbar unverändert, nur
//! die kopierte Zeile endet am Text.
//!
//! Wichtig dabei: **jeder** Lauf setzt seinen Hintergrund selbst, auch der
//! Reset-Lauf am rechten Rand (das ist das `ESC[49m`, das ohne den
//! Hintergrundwechsel des Laufs fehlen würde). Sonst malt der Reset-Lauf den
//! Farbton des Laufs davor in den Canvas-Schwanz – bei einem offenen
//! Overlay-Dialog läuft dessen Hintergrund dann bis zum rechten Terminalrand
//! weiter, und die erste Reset-Zeile der Folgezeile erbt denselben Ton und
//! färbt sich ab dem linken Rand ein.
//!
//! Sicherheit: Gelöscht wird ausschließlich der Schwanz nach dem letzten
//! Inhalt, und nur wenn er leer ist und keine auf Leerzellen sichtbaren
//! Modifikatoren trägt (Unterstreichung/Durchstreichen/Blinken/Reverse würden
//! verschwinden). Der Schwanz reicht dabei bis zum rechten **Terminalrand**
//! (`Backend::size`) – rechts davon gibt es keine Zellen, die ein `ESC[K`
//! fälschlich einfärben könnte. Zellen, die der Snapshot nicht kennt (noch nie
//! geschrieben), gelten als leer + Terminal-Default, was auch genau ihr
//! Terminalzustand ist. Wide-Zeichen-Folgezellen (`CellDiffOption::Skip`) zählen
//! vorsichtshalber als Inhalt, damit `ESC[K` nicht in ein breites Zeichen
//! hineinschneidet –
//! ratatuis Diff liefert sie allerdings gar nicht (sie werden übersprungen),
//! der Schutz greift also nur, falls sie im Snapshot auftauchen.
//!
//! ## Leerzeilen
//!
//! Auch eine Zeile ganz ohne Inhalt (Abstände zwischen den Blöcken, Leerzeilen
//! im Prosa-Teil) wird abgeschlossen – von Spalte 0, mit dem Canvas-
//! Hintergrund. Sichtbar ändert das nichts (die Zellen sind leer und schon
//! Terminal-Default), aber die Terminalzeile ist damit markiert: ohne diesen
//! Abschluss nähme eine Kopie über solche Zeilen hinweg die volle
//! Bildschirmbreite an Leerzeichen mit. Geschieht wird das nur einmal pro
//! Zeile (`sealed`) und erneut, sobald ein Diff wieder in die Zeile schreibt.
//!
//! Der Wrapper rendert wie `CrosstermBackend<Stdout>` und schreibt die `ESC[K`-
//! Sequenzen über ein zweites `Stdout`-Handle in denselben (globalen, von Rust
//! geteilten) Terminal-Puffer → die Reihenfolge bleibt korrekt (Zellen erst,
//! Abschluss danach) und der Frame-Flush spült beides zusammen.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{Color as CtColor, SetBackgroundColor};
use crossterm::terminal::{Clear, ClearType as TermClear};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

/// Modifikatoren, die auch auf einer *leeren* Zelle sichtbar sind. Ein `ESC[K`
/// würde sie wegräumen – Zeilen mit solchen Zellen im Schwanz bleiben daher
/// unangetastet (im Chat trifft das praktisch nur Unterstreichungen von
/// Überschriften/Links, die nie über den Text hinaus gefüllt werden).
const BLANK_DECORATION: Modifier = Modifier::REVERSED
    .union(Modifier::UNDERLINED)
    .union(Modifier::SLOW_BLINK)
    .union(Modifier::RAPID_BLINK)
    .union(Modifier::CROSSED_OUT);

/// Ratatui-Farbe → crossterm-Farbe (`SetBackgroundColor` erwartet die
/// crossterm-Eigenen). Die hellen/dunklen ANSI-Namen sind bei crossterm
/// vertauscht (`Red` = 91, `DarkRed` = 31) – Zuordnung wie in ratatui.
fn ct_color(c: Color) -> CtColor {
    match c {
        Color::Reset => CtColor::Reset,
        Color::Black => CtColor::Black,
        Color::Red => CtColor::DarkRed,
        Color::Green => CtColor::DarkGreen,
        Color::Yellow => CtColor::DarkYellow,
        Color::Blue => CtColor::DarkBlue,
        Color::Magenta => CtColor::DarkMagenta,
        Color::Cyan => CtColor::DarkCyan,
        Color::Gray => CtColor::Grey,
        Color::DarkGray => CtColor::DarkGrey,
        Color::LightRed => CtColor::Red,
        Color::LightGreen => CtColor::Green,
        Color::LightYellow => CtColor::Yellow,
        Color::LightBlue => CtColor::Blue,
        Color::LightMagenta => CtColor::Magenta,
        Color::LightCyan => CtColor::Cyan,
        Color::White => CtColor::White,
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        Color::Indexed(i) => CtColor::AnsiValue(i),
    }
}
/// Kompakte Erfassung einer Bildschirmzelle für den Zeilenabschluss.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CellInfo {
    /// Inhalt (nicht-leer bzw. kein Wide-Zeichen-Folgezell).
    blank: bool,
    /// Hintergrund der Zelle (`Color::Reset` = Terminal-Default).
    bg: Color,
    /// Auf der Leerzelle sichtbarer Modifikator → nicht löschen.
    decorated: bool,
}

/// Nicht gesetzte Zelle = leer + Terminal-Default-Hintergrund.
impl Default for CellInfo {
    fn default() -> Self {
        CellInfo {
            blank: true,
            bg: Color::Reset,
            decorated: false,
        }
    }
}
impl CellInfo {
    fn from_cell(c: &Cell) -> Self {
        CellInfo {
            blank: c.symbol() == " " && c.diff_option != CellDiffOption::Skip,
            bg: c.bg,
            decorated: c.modifier.intersects(BLANK_DECORATION),
        }
    }
}
/// Geplanter Zeilenabschluss: `ESC[K` ab Spalte `x` in Zeile `y`, gefüllt mit
/// Hintergrund `bg` (BCE). Ein Schwanz mit mehreren Farbläufen braucht
/// mehrere Abschlüsse: jeder Lauf wird mit *seinem* Hintergrund gelöscht, der
/// jeweils bis zum Zeilenende wirkt und so vom nächsten Lauf überschrieben wird.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct El {
    x: u16,
    y: u16,
    bg: Color,
}
/// Berechnet die Abschlüsse der Zeilenschwänze: Läufe gleicher Hintergrund-
/// farbe, beginnend direkt nach dem letzten Inhalt (bei einer Zeile ganz ohne
/// Inhalt ab Spalte 0 – so wird auch die Leerzeile selbst zu Ende markiert).
///
/// Wichtig: Zeilen **mit** Inhalt werden nur beendet, wenn der aktuelle Frame-
/// Diff sie berührt (`touched`) – unberührte Zeilen behalten ihren (bereits
/// korrekten) Terminal-Zustand vom Frame ihres letzten Zeichnens. Ohne diese
/// Einschränkung würde zum Beispiel das (nur beim Start gezeichnete) Logo bei
/// einem späteren Tippen-Frame fälschlich an der kleinen, getippten Breite
/// abgeschnitten.
///
/// Zeilen **ohne** Inhalt sind davon ausgenommen: Sie bestehen nur aus leeren
/// Zellen, ein Abschluss entfernt also nichts Sichtbares – ohne ihn bliebe die
/// Terminalzeile dort aber unmarkiert und eine Kopie nähme die volle
/// Bildschirmbreite an Leerzeichen mit (Abstände im Chat, Leerzeilen im
/// Prosa-Teil). `sealed` merkt sich, welche Zeilen bereits abgeschlossen
/// wurden, damit das nicht in jedem Frame erneut geschickt wird; geschrieben
/// wird in eine Zeile nur per Diff, und dann ist sie wieder `touched`.
///
/// `width` ist die Terminalbreite (`Backend::size`): der Schwanz einer Zeile
/// reicht damit bis zum rechten Terminalrand, es gibt keine Zellen mehr, die ein
/// `ESC[K` fälschlich einfärben könnte. Zellen, die der Snapshot nicht kennt
/// (noch nie geschrieben), gelten als leer + Terminal-Default – das ist auch
/// genau ihr Terminalzustand. (Pur/testbar; der Wrapper pflegt `cells`/`sealed`
/// aus dem Diff und liest die Breite pro Frame.)
fn plan_els(
    cells: &[Vec<CellInfo>],
    touched: &[bool],
    sealed: &mut Vec<bool>,
    max_y: usize,
    width: usize,
) -> Vec<El> {
    let empty: Vec<CellInfo> = Vec::new();
    let mut els = Vec::new();
    for y in 0..=max_y {
        if y >= sealed.len() {
            sealed.resize(y + 1, false);
        }
        let is_touched = touched.get(y).copied().unwrap_or(false);
        if !is_touched && sealed[y] {
            continue; // bereits abgeschlossen, seither unberührt
        }
        let row: &[CellInfo] = cells.get(y).map_or(&empty, Vec::as_slice);
        let start = match row.iter().rposition(|c| !c.blank) {
            // Schwanz direkt nach dem letzten Inhalt …
            Some(x) => {
                if !is_touched {
                    continue; // Inhalt + unberührt: stehen lassen
                }
                x + 1
            }
            // … bzw. die ganze Zeile, wenn sie keinen Inhalt hat.
            None => 0,
        };
        // Schwanz in Läufe gleicher Hintergrundfarbe zerlegen. Zellen rechts
        // vom Snapshot (noch nie geschrieben) sind Terminal-Default.
        let mut runs: Vec<El> = Vec::new();
        let mut ok = start < width;
        for x in start..width {
            let info = row.get(x).copied().unwrap_or_default();
            // Nach dem letzten Inhalt kann nichts anderes als eine Leerzelle
            // kommen; Zellen mit sichtbarem Modifikator dürfen nicht sterben.
            if !info.blank || info.decorated {
                ok = false;
                break;
            }
            match runs.last_mut() {
                Some(run) if run.bg == info.bg => {}
                _ => runs.push(El {
                    x: x as u16,
                    y: y as u16,
                    bg: info.bg,
                }),
            }
        }
        // Der letzte Lauf reicht bis zum rechten Terminalrand – es gibt keine
        // Zellen dahinter, die er fälschlich einfärben könnte. Er darf darum
        // auch *gemalt* sein: bei einem über die volle Breite gemalten Band
        // (User-Eingabe) malt er genau die Farbe, die dort ohnehin steht. Das
        // Bild bleibt pixelgleich, und die Terminalzeile endet am Text – vorher
        // blieb sie bei einem solchen Band unmarkiert und eine Kopie nahm die
        // volle Breite an Leerzeichen mit.
        if !ok {
            continue;
        }
        sealed[y] = true;
        els.extend(runs);
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
    /// `blank`/`bg`/`decorated`), damit die EL-Entscheidung auch ohne den
    /// aktuellen Frame-Diff den rechten Rand jeder Zeile kennt.
    cells: Vec<Vec<CellInfo>>,
    /// Terminalbreite (`Backend::size`): so weit reicht der Schwanz einer Zeile.
    /// Pro Frame neu gelesen, weil ratatui bei einer Größenänderung die Puffer
    /// anpasst – ein alter Wert ergäbe zu kurze Abschlüsse (die Zeile würde
    /// nicht bis zum Rand, sondern nur bis zur alten Breite markiert).
    width: usize,
    /// Zeilen, deren Schwanz bereits abgeschlossen wurde. Leerzeilen werden
    /// auch *ohne* Diff abgeschlossen (sonst nähme eine Kopie dort die volle
    /// Breite an Leerzeichen mit) – das merkt sich diese Liste, damit es nicht
    /// in jedem Frame erneut gesendet wird. Sobald ein Diff in die Zeile
    /// schreibt, ist sie wieder `touched` und wird neu geplant.
    sealed: Vec<bool>,
}

impl ElBackend {
    pub(crate) fn new(inner: CrosstermBackend<io::Stdout>) -> Self {
        ElBackend {
            inner,
            out: io::stdout(),
            cells: Vec::new(),
            width: 0,
            sealed: Vec::new(),
        }
    }

    /// Hängt die Zeilenabschlüsse (`ESC[K`) für die Zeilen an, die dieser
    /// Frame berührt hat (`touched` = Zeilen mit Diff-Zelle, `max_y` = letzte
    /// berührte) – sowie für Leerzeilen, die noch nicht abgeschlossen sind.
    fn emit_row_ends(&mut self, touched: Vec<bool>, max_y: usize) -> io::Result<()> {
        let els = plan_els(&self.cells, &touched, &mut self.sealed, max_y, self.width);
        write_row_ends(&mut self.out, &els)
    }

    /// Alles verwerfen, was den zuletzt *geschriebenen* Bildschirmzustand
    /// spiegelt (Snapshot, bekannte Breite, `sealed`) – nachdem der Screen
    /// gelöscht wurde, beschreiben diese Daten den Terminalzustand nicht mehr.
    fn forget_screen(&mut self) {
        self.cells.clear();
        self.width = 0;
        self.sealed.clear();
    }
}
/// Schreibt die geplanten Abschlüsse in `w`.
///
/// `ESC[K` füllt mit dem *aktuell gesetzten* SGR-Hintergrund (BCE) – deshalb
/// setzt jeder Lauf seinen Hintergrund selbst, auch ein Reset-Lauf (siehe
/// Modul-Doku: ohne das `ESC[49m` erbt er den Ton des Laufs davor und malt den
/// Dialog-/Band-Hintergrund in den Canvas-Schwanz). Geschrieben wird der Wechsel
/// nur, wenn er wirklich stattfindet, damit ein reiner Canvas-Schwanz
/// bytegleich zum bisherigen Verhalten bleibt.
///
/// `aktiv` startet auf dem Terminal-Default (`CrosstermBackend::draw` beendet
/// jeden Frame mit SGR-Default) und steht am Ende wieder darauf, weil ratatui
/// den nächsten Frame nicht mit einem Alt-Hintergrund beginnt – sonst würde die
/// erste Zelle des nächsten Frames den Band-Hintergrund erben.
fn write_row_ends(w: &mut impl Write, els: &[El]) -> io::Result<()> {
    if els.is_empty() {
        return Ok(());
    }
    let mut aktiv = Color::Reset;
    for el in els {
        if el.bg != aktiv {
            queue!(w, SetBackgroundColor(ct_color(el.bg)))?;
            aktiv = el.bg;
        }
        queue!(w, MoveTo(el.x, el.y), Clear(TermClear::UntilNewLine))?;
    }
    if aktiv != Color::Reset {
        queue!(w, SetBackgroundColor(CtColor::Reset))?;
    }
    Ok(()) // Flush übernimmt der Terminal-Wrapper am Frame-Ende.
}
/// Wendet einen Frame-Diff auf den Zell-Snapshot an (wächst bei Bedarf) und
/// liefert, welche Zeilen der Diff berührt hat (`touched[y]`) samt letzter
/// berührter Zeile `max_y`. (Getrennt, damit das Snapshot-Handling mit echten
/// `Cell`s testbar ist.)
fn update_snapshot(
    cells: &mut Vec<Vec<CellInfo>>,
    diff: &[(u16, u16, &Cell)],
) -> (usize, Vec<bool>) {
    let mut max_y = 0usize;
    let mut touched: Vec<bool> = Vec::new();
    for &(x, y, cell) in diff {
        let (xi, yi) = (x as usize, y as usize);
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
    (max_y, touched)
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
        let (max_y, touched) = update_snapshot(&mut self.cells, &cells);
        // Der Schwanz einer Zeile reicht bis zum rechten Terminalrand. `size()`
        // schlägt fehl, wenn stdout kein Terminal ist – dann bleibt der letzte
        // bekannte Wert stehen (und im ungünstigsten Fall wird nichts gelöscht).
        if let Ok(size) = self.inner.size() {
            self.width = size.width as usize;
        }

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
        self.forget_screen();
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        // Ratatuis `Terminal::resize` räumt über `clear_region(All)` auf – danach
        // beschreibt der Snapshot den Terminalzustand nicht mehr (die Puffer
        // behalten ihren Inhalt, der Screen ist aber leer). Snapshot, Breite und
        // `sealed` verwerfen: der nächste Frame zeichnet alles neu und füllt
        // den Snapshot wieder auf.
        if clear_type == ClearType::All {
            self.forget_screen();
        }
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

    /// Gemalter Code-/Box-Hintergrund (`theme().surface_bg` im Dark-Theme).
    const BAND: Color = Color::Rgb(11, 12, 17);

    /// Alle Zeilen 0..=`max_y` als berührt markieren.
    fn all_touched(max_y: usize) -> Vec<bool> {
        vec![true; max_y + 1]
    }

    /// Inhalt (nicht-leere Zelle) mit Terminal-Default-Hintergrund.
    fn text() -> CellInfo {
        CellInfo {
            blank: false,
            ..CellInfo::default()
        }
    }

    /// Leere Zelle mit gemaltem Hintergrund (z. B. Code-/Box-Band).
    fn painted(bg: Color) -> CellInfo {
        CellInfo {
            bg,
            ..CellInfo::default()
        }
    }

    /// Textzelle **auf** einem gemalten Hintergrund (z. B. die User-Aussage im
    /// Eingabe-Band: Text und Band haben denselben Hintergrund).
    fn text_auf(bg: Color) -> CellInfo {
        CellInfo {
            blank: false,
            bg,
            ..CellInfo::default()
        }
    }

    /// Leere Zelle mit sichtbarem Modifikator (darf nicht gelöscht werden).
    fn decorated(bg: Color) -> CellInfo {
        CellInfo {
            bg,
            decorated: true,
            ..CellInfo::default()
        }
    }

    /// `plan_els` mit frischer `sealed`-Liste: ein Plan pro Frame.
    fn plan(cells: &[Vec<CellInfo>], touched: &[bool], max_y: usize, width: usize) -> Vec<El> {
        plan_els(cells, touched, &mut Vec::new(), max_y, width)
    }

    #[test]
    fn richtiger_zeilenabschluss_nach_letztem_inhalt() {
        // "hello" bei (0,0) und (4,0); Rest der Zeile leer + Default → EL an Spalte 5.
        let mut cells = vec![vec![CellInfo::default(); 10]; 3];
        cells[0][0] = text();
        cells[0][4] = text();
        cells[1][0] = text();
        let els = plan(&cells, &all_touched(2), 2, 10);
        assert_eq!(
            els,
            vec![
                El {
                    x: 5,
                    y: 0,
                    bg: Color::Reset
                },
                El {
                    x: 1,
                    y: 1,
                    bg: Color::Reset
                },
                // Zeile 2 ist ganz leer → Abschluss ab Spalte 0, sonst nähme
                // eine Kopie über sie hinweg die volle Breite mit.
                El {
                    x: 0,
                    y: 2,
                    bg: Color::Reset
                },
            ],
            "Zeile 0 nach 'hello', Zeile 1 nach dem Zeichen bei Spalte 0, Zeile 2 komplett"
        );
    }

    #[test]
    fn unberuehrte_zeile_wird_nicht_gekuerzt() {
        // Regression fürs Logo: Zeile 0 hat (vom Start-Frame) Inhalt bis Spalte
        // 30; der AKTUELLE Frame berührt sie nicht (touched[0] = false), sondern
        // nur Zeile 1 (z. B. getippte Eingabezeile, kleine Breite). Ohne die
        // touched-Einschränkung würde plan_els jetzt an der kleinen Breite
        // schneiden und den Logo-Inhalt jenseits davon löschen.
        let mut cells = vec![vec![CellInfo::default(); 40]; 2];
        cells[0][..10].fill(text());
        cells[1][0] = text();
        let els = plan(&cells, &[false, true], 1, 40);
        assert!(
            !els.iter().any(|el| el.y == 0),
            "unberührte Logo-Zeile bleibt unangetastet: {els:?}"
        );
        assert_eq!(
            els,
            vec![El {
                x: 1,
                y: 1,
                bg: Color::Reset
            }],
            "nur die getippte Zeile"
        );
    }

    #[test]
    fn user_band_bis_zum_rechten_rand_wird_mit_seiner_farbe_abgeschlossen() {
        // Geometrie einer User-Aussage: `PAD` Spalten Einzug, der Text, danach
        // das über die volle Breite gemalte Band bis zum rechten Rand. Der
        // Abschluss sitzt direkt hinter dem Text und trägt die Bandfarbe – die
        // kopierte Zeile endet am Text, während das Band sichtbar bis zum Rand
        // stehen bleibt (der `ESC[K` malt dort genau die vorhandene Farbe).
        let mut cells = vec![vec![CellInfo::default(); 6]];
        cells[0].fill(painted(BAND));
        cells[0][2..4].fill(text_auf(BAND));
        assert_eq!(
            plan(&cells, &[true], 0, 6),
            vec![El {
                x: 4,
                y: 0,
                bg: BAND
            }]
        );
    }

    #[test]
    fn nie_geschriebene_zellen_rechts_bleiben_terminal_default() {
        // Der Snapshot kennt die Zeile nur bis zur letzten geschriebenen Spalte;
        // alles rechts davon wurde nie geschrieben und ist Terminal-Default. Der
        // Schwanz reicht trotzdem bis zum rechten Rand, und dieser unbekannte
        // Teil ist ein *eigener* Reset-Lauf: er macht die Terminalzeile bis zum
        // Rand frei, ohne eine Farbe zu erfinden.
        let mut cells = vec![vec![CellInfo::default(); 3]];
        cells[0][0] = text();
        cells[0][1..3].fill(painted(BAND));
        assert_eq!(
            plan(&cells, &[true], 0, 10),
            vec![
                El {
                    x: 1,
                    y: 0,
                    bg: BAND
                },
                El {
                    x: 3,
                    y: 0,
                    bg: Color::Reset
                },
            ],
            "Band bis zum Snapshot-Rand, danach der nie geschriebene Rest"
        );
    }

    #[test]
    fn codeband_wird_je_farbislauf_geloescht() {
        // Codeblock-Zeile: Text, dann schwarzes Band bis `width - PAD_R`, dann
        // der Canvas-Rand. Zwei Läufe, zwei Abschlüsse – jeweils mit dem
        // Hintergrund des Laufs, damit das Bild unverändert bleibt, die
        // Terminalzeile aber am Text endet (Kopie ohne Trailing-Spaces).
        let mut cells = vec![vec![CellInfo::default(); 12]];
        cells[0][0..3].fill(text());
        cells[0][3..10].fill(painted(BAND));
        assert_eq!(
            plan(&cells, &[true], 0, 12),
            vec![
                El {
                    x: 3,
                    y: 0,
                    bg: BAND
                },
                El {
                    x: 10,
                    y: 0,
                    bg: Color::Reset
                },
            ]
        );
    }

    #[test]
    fn leere_codezeile_wird_ebenfalls_geloescht() {
        // Leerzeile IM Codeblock: kein Text, aber ein gemaltes Band. Ohne
        // Abschluss würde die Zeile als volle Leerzeichenreihe kopiert.
        // Abgeschlossen wird ab Spalte 0, damit auch der linke Canvas-Rand
        // als gelöscht gilt – die drei Läufe ergeben das Band unveraendert.
        let mut cells = vec![vec![CellInfo::default(); 12]];
        cells[0][2..10].fill(painted(BAND));
        assert_eq!(
            plan(&cells, &[true], 0, 12),
            vec![
                El {
                    x: 0,
                    y: 0,
                    bg: Color::Reset
                },
                El {
                    x: 2,
                    y: 0,
                    bg: BAND
                },
                El {
                    x: 10,
                    y: 0,
                    bg: Color::Reset
                },
            ]
        );
    }

    /// Leerzeilen werden auch *ohne* Diff abgeschlossen – sonst nähme eine
    /// Kopie über sie hinweg (Abstände im Chat, Leerzeilen im Prosa-Teil) die
    /// volle Bildschirmbreite an Leerzeichen mit. Sichtbar ist davon nichts.
    #[test]
    fn unberuehrte_leerzeile_wird_abgeschlossen() {
        // Zeile 0 nie geschrieben (Snapshot leer = Terminal-Default), Zeile 1
        // ebenfalls leer, Zeile 2 hat Inhalt und ist unberührt (Logo-Fall).
        let mut cells = vec![Vec::new(), vec![CellInfo::default(); 40]];
        cells.push({
            let mut row = vec![CellInfo::default(); 40];
            row[..10].fill(text());
            row
        });
        let els = plan(&cells, &[false, false, false], 2, 40);
        assert_eq!(
            els,
            vec![
                El {
                    x: 0,
                    y: 0,
                    bg: Color::Reset
                },
                El {
                    x: 0,
                    y: 1,
                    bg: Color::Reset
                },
            ],
            "Leerzeilen ja, Inhaltszeile nein"
        );
    }

    /// `sealed`: Leerzeilen werden nur einmal abgeschlossen (sonst geht bei
    /// jedem Frame ein `ESC[K` pro Leerzeile raus), und ein Diff in die Zeile
    /// macht sie wieder planbar.
    #[test]
    fn leerzeile_wird_nur_einmal_abgeschlossen() {
        let cells = vec![vec![CellInfo::default(); 12]];
        let mut sealed: Vec<bool> = Vec::new();
        let first = plan_els(&cells, &[false], &mut sealed, 0, 12);
        assert_eq!(
            first,
            vec![El {
                x: 0,
                y: 0,
                bg: Color::Reset
            }]
        );
        assert_eq!(sealed, vec![true]);
        // Zweiter Frame, unberührt → nichts mehr.
        assert!(plan_els(&cells, &[false], &mut sealed, 0, 12).is_empty());
    }

    #[test]
    fn beruehrte_zeile_wird_nach_seal_neu_geplant() {
        // Zeile war leer und abgeschlossen, jetzt schreibt ein Diff Text
        // hinein → neu planen (der alte Abschluss bei Spalte 0 wäre falsch).
        let mut cells = vec![vec![CellInfo::default(); 12]];
        let mut sealed: Vec<bool> = Vec::new();
        assert_eq!(
            plan_els(&cells, &[false], &mut sealed, 0, 12),
            vec![El {
                x: 0,
                y: 0,
                bg: Color::Reset
            }]
        );
        cells[0][0..3].fill(text());
        assert_eq!(
            plan_els(&cells, &[true], &mut sealed, 0, 12),
            vec![El {
                x: 3,
                y: 0,
                bg: Color::Reset
            }],
            "nach dem Diff neu geplant"
        );
    }

    #[test]
    fn sichtbare_deko_im_schwanz_bricht_die_abschluesse_ab() {
        // Unterstreichung/Reverse auf einer Leerzelle würde ein ESC[K löschen →
        // Zeile bleibt wie bisher unangetastet.
        let mut cells = vec![vec![CellInfo::default(); 12]];
        cells[0][0..3].fill(text());
        cells[0][3..10].fill(painted(BAND));
        cells[0][10] = decorated(Color::Reset);
        assert!(plan(&cells, &[true], 0, 12).is_empty());
    }

    #[test]
    fn wide_zeichen_folgezelle_zaehlt_als_inhalt() {
        // Breites Zeichen bei (4,0), Folgezelle (diff Skip) bei (5,0) → EL erst
        // bei 6, damit ESC[K nicht ins breite Zeichen schneidet.
        let mut row = vec![CellInfo::default(); 8];
        row[4] = text();
        row[5] = text(); // Skip-Zelle zählt als Inhalt
        let cells = vec![row];
        assert_eq!(
            plan(&cells, &[true], 0, 8),
            vec![El {
                x: 6,
                y: 0,
                bg: Color::Reset
            }]
        );
    }

    #[test]
    fn snapshot_aus_echten_cells_und_diff_ergibt_el() {
        use ratatui::buffer::Cell;
        // Zeile 0: "ab" bei Spalten 0-1 plus eine (gemalte-freie) Leerzelle bei
        // 2 → EL bei 2. Zeile 1: ein Zeichen bei 0 → EL bei 1. Terminalbreite 4:
        // die Spalte 3 wurde nie geschrieben und gilt als leer + Default.
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
        let (max_y, touched) = update_snapshot(&mut snap, &diff);
        assert_eq!(max_y, 1);
        assert_eq!(touched, vec![true, true]);
        let els = plan_els(&snap, &touched, &mut Vec::new(), max_y, 4);
        assert!(
            els.contains(&El {
                x: 2,
                y: 0,
                bg: Color::Reset
            }),
            "Zeile 0 nach 'ab': {els:?}"
        );
        assert!(
            els.contains(&El {
                x: 1,
                y: 1,
                bg: Color::Reset
            }),
            "Zeile 1 nach Zeichen: {els:?}"
        );
    }

    /// Byte-Ebene: **jeder** Abschluss setzt vorher seinen SGR-Hintergrund
    /// (BCE – sonst würde das gelöschte Band weiß/terminal-default). Der
    /// Reset-Lauf am rechten Rand des Bands braucht das `ESC[49m` genauso: er
    /// ist der Abschluss, der den Canvas-Schwanz löscht, und würde ohne
    /// eigenen Hintergrund den Band-Tont erben (das Band liefe dann bis zum
    /// rechten Terminalrand weiter). Am Ende ist nichts mehr zurückzusetzen –
    /// der letzte Lauf steht bereits auf Default.
    #[test]
    fn abschluss_schreibt_bce_hintergrund_und_setzt_zurueck() {
        let els = vec![
            El {
                x: 3,
                y: 7,
                bg: BAND,
            },
            El {
                x: 10,
                y: 7,
                bg: Color::Reset,
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        write_row_ends(&mut buf, &els).expect("write");
        let s = String::from_utf8(buf).expect("utf8");
        assert_eq!(
            s,
            "\x1b[48;2;11;12;17m\x1b[8;4H\x1b[K\x1b[49m\x1b[8;11H\x1b[K"
        );
    }

    /// Der Canvas-Reset einer Zeile darf den Farbton der Zeile davor nicht in
    /// die *nächste* Zeile hineinragen lassen: deren erster Abschluss (Leerzeile
    /// ab Spalte 0) würde sonst den Dialog-/Band-Tont erben und die Zeile ab dem
    /// linken Rand einfärben. Deshalb steht vor jedem Abschluss der Hintergrund,
    /// den er malt – auch dann, wenn er gar keine Farbe setzt.
    #[test]
    fn reset_lauf_setzt_auch_und_vor_der_folgenden_zeile_seinen_hintergrund() {
        let els = vec![
            // Dialogzeile: Dialog-Band bis zum rechten Dialogrand, dann Canvas.
            El {
                x: 20,
                y: 4,
                bg: BAND,
            },
            El {
                x: 60,
                y: 4,
                bg: Color::Reset,
            },
            // Folgezeile: komplett leer → Abschluss ab Spalte 0.
            El {
                x: 0,
                y: 5,
                bg: Color::Reset,
            },
        ];
        let mut buf: Vec<u8> = Vec::new();
        write_row_ends(&mut buf, &els).expect("write");
        let s = String::from_utf8(buf).expect("utf8");
        assert_eq!(
            s,
            "\x1b[48;2;11;12;17m\x1b[5;21H\x1b[K\x1b[49m\x1b[5;61H\x1b[K\
             \x1b[6;1H\x1b[K"
        );
    }

    /// Reiner Canvas-Schwanz (Textzeile) bleibt bytegleich zum bisherigen
    /// Verhalten: ein `ESC[K`, keine zusätzliche SGR-Sequenz.
    #[test]
    fn canvas_schwanz_bleibt_ohne_zusaetzliche_sgr() {
        let mut buf: Vec<u8> = Vec::new();
        write_row_ends(
            &mut buf,
            &[El {
                x: 5,
                y: 0,
                bg: Color::Reset,
            }],
        )
        .expect("write");
        assert_eq!(String::from_utf8(buf).expect("utf8"), "\x1b[1;6H\x1b[K");
    }

    /// Ende zu Ende über die echte Markdown-/Band-Geometrie: eine Chatzeile
    /// mit Text und fenced Codeblock, gerendert in einen Bildschirmpuffer und
    /// dann durch den echten Diff geschickt. Erwartung: die Textzeile bleibt
    /// bei einem `ESC[K` (Canvas), jede Codezeile bekommt zusätzlich den
    /// Band-Abschluss direkt nach ihrem Text plus den Canvas-Abschluss am
    /// rechten Bandrand (`width - PAD_R`) – das ist der Fall, den es vorher
    /// nicht gab und der das Kopieren des Codeblocks ohne Trailing-Spaces
    /// ermöglicht.
    #[test]
    fn codeblock_aus_markdown_bekommt_band_abschluss() {
        use crate::ui::markdown::{logical_lines, preserve_breaks, wrap_markdown};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Paragraph;
        use ratatui::{backend::TestBackend, Terminal};

        const WIDTH: u16 = 24;
        let src = preserve_breaks("text\n\n```\nlet a = 1;\n\nlet b = 2;\n```\n");
        let logical = logical_lines(&src);
        let lines = wrap_markdown(&logical, WIDTH as usize, crate::ui::PAD);

        let area = Rect::new(0, 0, WIDTH, lines.len() as u16);
        let mut term =
            Terminal::new(TestBackend::new(WIDTH, lines.len() as u16)).expect("TestBackend");
        term.draw(|f| f.render_widget(Paragraph::new(lines), area))
            .expect("render");
        let next = term.backend().buffer().clone();
        let diff = Buffer::empty(area).diff(&next);

        let mut snap: Vec<Vec<CellInfo>> = Vec::new();
        let (max_y, touched) = update_snapshot(&mut snap, &diff);
        // `width` ist im Betrieb die Terminalbreite aus `size()` – hier die
        // Fläche selbst.
        let els = plan_els(&snap, &touched, &mut Vec::new(), max_y, WIDTH as usize);
        let band = snap
            .iter()
            .flatten()
            .map(|c| c.bg)
            .find(|bg| *bg != Color::Reset)
            .expect("Codeband mit gemaltem Hintergrund");
        let band_right = WIDTH as usize - crate::ui::PAD_R;

        for (y, row) in snap.iter().enumerate() {
            if !row.iter().any(|c| c.bg == band) {
                continue; // keine Codezeile
            }
            let row_els: Vec<&El> = els.iter().filter(|el| el.y as usize == y).collect();
            // Jede Codezeile wird bis zum Textende abgeschlossen und danach
            // einmal je Farblauf (Band, Canvas) – der Abschluss sitzt also
            // unmittelbar nach dem Text bzw. am linken Bandrand bei Leerzeilen.
            assert!(
                row_els.iter().any(|el| el.bg == band),
                "Codezeile {y} schließt ihr Band ab: {row_els:?}"
            );
            assert_eq!(
                row_els.last().map(|el| (el.x as usize, el.bg)),
                Some((band_right, Color::Reset)),
                "Canvas-Abschluss am rechten Bandrand in Zeile {y}: {row_els:?}"
            );
            let first_text = row.iter().rposition(|c| !c.blank);
            let expected = first_text.map_or(crate::ui::PAD, |x| x + 1);
            assert_eq!(
                row_els
                    .iter()
                    .find(|el| el.bg == band)
                    .map(|el| el.x as usize),
                Some(expected),
                "Band-Abschluss direkt nach dem Text in Zeile {y}: {row_els:?}"
            );
        }
        // Textzeile über dem Block: ein Canvas-Abschluss direkt nach dem Text.
        let text_row: Vec<&El> = els.iter().filter(|el| el.y == 0).collect();
        assert_eq!(text_row.len(), 1, "Textzeile: {text_row:?}");
        assert_eq!(text_row[0].bg, Color::Reset);
    }

    // Scrollen: Hintergrundfarben der Abschlüsse gegen den *aktuellen* Puffer
    // prüfen. `ESC[K` malt eine Farbe – stimmt der Snapshot (und damit `bg`)
    // nicht mehr mit dem Puffer überein, würde ein Abschluss eine veraltete
    // Fläche über den Bildschirm malen (Geisterband, heller Rand). Der
    // Following-Check gilt daher für JEDEN von einem Abschluss berührten
    // Bildschirmplatz, über alle Frames einer Scroll-Sequenz.
    // -----------------------------------------------------------------------

    /// Rechnet nach, was die Abschlüsse im Terminal bewirken (spätere Abschlüsse
    /// überschreiben frühere, jeder wirkt bis zum Zeilenende) und vergleicht das
    /// mit dem aktuellen Buffer: Farbe muss gleich sein, und die Zelle muss
    /// leer sein – ein `ESC[K` darf nie Inhalt wegräumen.
    fn assert_els_passen_zum_buffer(els: &[El], next: &ratatui::buffer::Buffer) {
        for y in 0..next.area.height {
            for x in 0..next.area.width {
                // Letzter Abschluss, der diese Zelle abdeckt.
                let painted = els.iter().rev().find(|el| el.y == y && el.x <= x);
                let Some(el) = painted else {
                    continue; // nicht berührt: vom Diff geschrieben
                };
                let cell = next.cell((x, y)).expect("Zelle im Buffer");
                assert_eq!(cell.symbol(), " ", "ESC[K darf keinen Inhalt löschen");
                assert_eq!(
                    el.bg, cell.bg,
                    "Zelle ({x},{y}) wird auf {:?} statt auf {:?} gesetzt: {els:?}",
                    el.bg, cell.bg
                );
            }
        }
    }

    /// Scrollsequenz über einen Codeblock: Zeilen wandern nach oben, der Block
    /// rutscht aus dem Bild, seine Zeilen werden zu Canvas. Der Plan jedes
    /// Frames muss exakt die Farben des *jetzt* gültigen Puffers treffen –
    /// insbesondere darf auf einer Zeile, die gerade *kein* Codeband mehr hat,
    /// kein Band gemalt werden (Geisterband beim Scrollen).
    #[test]
    fn scrollen_aktualisiert_die_abschlussfarben() {
        use crate::ui::markdown::{logical_lines, preserve_breaks, wrap_markdown};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Style;
        use ratatui::widgets::Paragraph;
        use ratatui::{backend::TestBackend, Terminal};

        const WIDTH: u16 = 24;
        const HEIGHT: u16 = 6;
        let src = preserve_breaks("oben\n\n```\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\n");
        let lines = wrap_markdown(&logical_lines(&src), WIDTH as usize, crate::ui::PAD);
        let band = super::super::theme().surface_bg;

        // Ein Frame wie im Betrieb: Chatfläche (mit Viewport-Versatz) plus
        // Eingabeband auf der letzten Zeile. Das Band ist wichtig, weil es die
        // letzte Spalte schreibt – im Betrieb kommt die bekannte
        // Terminalbreite daher (Eingabe-/Statusband), nicht aus dem Chat.
        let frame = |scroll: u16| -> Buffer {
            let chat = Rect::new(0, 0, WIDTH, HEIGHT - 1);
            let input = Rect::new(0, HEIGHT - 1, WIDTH, 1);
            let mut term = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("TestBackend");
            term.draw(|f| {
                f.render_widget(Paragraph::new(lines.clone()).scroll((scroll, 0)), chat);
                f.render_widget(
                    Paragraph::new("").style(Style::default().bg(super::super::theme().band_bg)),
                    input,
                );
            })
            .expect("render");
            term.backend().buffer().clone()
        };

        let mut prev = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        let mut snap: Vec<Vec<CellInfo>> = Vec::new();
        let mut sealed: Vec<bool> = Vec::new();
        let width = WIDTH as usize;
        let mut band_els_sehen = 0;

        for scroll in 0..=(lines.len() as u16 + 1) {
            let next = frame(scroll);
            let diff = prev.diff(&next);
            let (max_y, touched) = update_snapshot(&mut snap, &diff);
            let els = plan_els(&snap, &touched, &mut sealed, max_y, width);
            assert_els_passen_zum_buffer(&els, &next);

            for y in 0..next.area.height {
                let row_gemalt = (0..WIDTH).any(|x| next.cell((x, y)).expect("Zelle").bg == band);
                let row_els: Vec<&El> = els.iter().filter(|el| el.y == y).collect();
                for el in &row_els {
                    if row_gemalt {
                        band_els_sehen += 1;
                    } else {
                        assert_ne!(
                            el.bg, band,
                            "Zeile {y} ohne Codeband, aber Abschluss malt Band: {els:?}"
                        );
                    }
                }
            }
            prev = next;
        }
        // Sicherstellen, dass die Sequenz den interessanten Fall überhaupt
        // enthält (sonst prüft der Test nichts).
        assert!(band_els_sehen > 0, "kein Band-Abschluss im Lauf gesehen");
    }

    /// Rechnet nach, was die Abschlüsse im *Terminal* bewirken: `ESC[K` füllt ab
    /// der Spalte bis zum Zeilenende mit dem gerade gesetzten SGR-Hintergrund
    /// (BCE) und leert die Zellen. Modelliert wird genau die Byte-Folge, die
    /// `write_row_ends` schreibt – inklusive der Hintergrundwechsel dazwischen.
    /// Das ist der Punkt, an dem ein fehlender Reset-Wechsel sichtbar wird.
    fn spiele_abschluesse(screen: &mut [Vec<(String, Color)>], bytes: &[u8]) {
        /// SGR-Parameter → Hintergrund (nur die von `write_row_ends` benutzten).
        fn sgr_bg(params: &[u8]) -> Color {
            let p = String::from_utf8(params.to_vec()).expect("ascii");
            let n: Vec<u8> = p.split(';').map(|v| v.parse().unwrap_or(0)).collect();
            match n.as_slice() {
                [] | [0] | [49] => Color::Reset,
                [48, 2, r, g, b] => Color::Rgb(*r, *g, *b),
                [48, 5, i] => Color::Indexed(*i),
                other => panic!("unerwarteter SGR {p:?} ({other:?})"),
            }
        }

        let mut bg = Color::Reset; // Frame-Start: Terminal-Default
        let mut cur = (0usize, 0usize);
        let mut i = 0;
        while i < bytes.len() {
            assert_eq!((bytes[i], bytes[i + 1]), (0x1b, b'['), "Folge: {bytes:?}");
            let mut j = i + 2;
            while !matches!(bytes[j], b'm' | b'H' | b'K') {
                j += 1;
            }
            match bytes[j] {
                b'm' => bg = sgr_bg(&bytes[i + 2..j]),
                b'H' => {
                    let n: Vec<u16> = String::from_utf8(bytes[i + 2..j].to_vec())
                        .expect("ascii")
                        .split(';')
                        .map(|v| v.parse().expect("Cursor-Position"))
                        .collect();
                    // 1-basiert im Terminal, 0-basiert im Buffer.
                    cur = (n[1] as usize - 1, n[0] as usize - 1);
                }
                b'K' => {
                    let (cx, cy) = cur;
                    for zelle in &mut screen[cy][cx..] {
                        *zelle = (" ".to_string(), bg);
                    }
                }
                _ => unreachable!(),
            }
            i = j + 1;
        }
    }

    /// Screen aus einem Buffer (das, was das Terminal nach dem Zeichnen des
    /// Frames vor sich hat).
    fn screen_von(buf: &ratatui::buffer::Buffer) -> Vec<Vec<(String, Color)>> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| {
                        let c = buf.cell((x, y)).expect("Zelle im Buffer");
                        (c.symbol().to_string(), c.bg)
                    })
                    .collect()
            })
            .collect()
    }

    use crate::app::App;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::{backend::TestBackend, Terminal};

    /// Treiber für [`overlay_dialog_bleibt_in_seinem_rect`]: zeichnet einen Frame
    /// mit der echten App, spiegelt Diff/Snapshot/Abschlüsse in ein
    /// Terminalmodell und sammelt alle Zellen, an denen das Modell vom gezeichneten
    /// Buffer abweicht.
    struct Terminalmodell {
        term: Terminal<TestBackend>,
        /// Buffer des letzten Frames (Ausgangspunkt des Terminalbilds).
        prev: Buffer,
        /// Snapshot/Plan des `ElBackend` (siehe `update_snapshot`/`plan_els`).
        snap: Vec<Vec<CellInfo>>,
        sealed: Vec<bool>,
        width: usize,
        app: App,
        befunde: Vec<String>,
        /// Abschlüsse des letzten Frames (Zusicherungen über einzelne Zeilen).
        letzte_els: Vec<El>,
    }

    impl Terminalmodell {
        const W: u16 = 80;
        const H: u16 = 24;

        fn new(app: App) -> Self {
            let area = Rect::new(0, 0, Self::W, Self::H);
            Self {
                term: Terminal::new(TestBackend::new(Self::W, Self::H)).expect("TestBackend"),
                prev: Buffer::empty(area),
                snap: Vec::new(),
                sealed: Vec::new(),
                width: 0,
                app,
                befunde: Vec::new(),
                letzte_els: Vec::new(),
            }
        }

        /// Ein Frame wie im Betrieb: zeichnen, Diff in den Snapshot, Abschlüsse
        /// planen und schreiben, dann das Terminalbild nachrechnen.
        fn frame(&mut self, label: &str) {
            self.term
                .draw(|f| super::super::draw(f, &mut self.app))
                .expect("draw");
            let next = self.term.backend().buffer().clone();
            let diff = self.prev.diff(&next);
            let (max_y, touched) = update_snapshot(&mut self.snap, &diff);
            // Im Betrieb liefert `Backend::size()` die Terminalbreite; im Modell
            // ist es die Breite des TestBackends.
            self.width = next.area.width as usize;
            let els = plan_els(&self.snap, &touched, &mut self.sealed, max_y, self.width);

            // Terminalbild: Stand des letzten Frames, dann der Diff (so wie
            // `CrosstermBackend::draw` schreibt), dann die Abschluss-Bytes.
            let mut screen = screen_von(&self.prev);
            for (x, y, cell) in diff {
                screen[y as usize][x as usize] = (cell.symbol().to_string(), cell.bg);
            }
            let mut bytes: Vec<u8> = Vec::new();
            write_row_ends(&mut bytes, &els).expect("write");
            spiele_abschluesse(&mut screen, &bytes);

            for y in 0..next.area.height {
                for x in 0..next.area.width {
                    let c = next.cell((x, y)).expect("Zelle im Buffer");
                    let soll = (c.symbol().to_string(), c.bg);
                    if screen[y as usize][x as usize] != soll {
                        self.befunde.push(format!(
                            "{label}: ({x},{y}) Terminal {:?} statt {:?}",
                            screen[y as usize][x as usize], soll
                        ));
                    }
                }
            }
            self.letzte_els = els;
            self.prev = next;
        }

        /// Fenstergröße geändert: `clear_region(All)` verwirft Snapshot und
        /// `sealed`, danach zeichnet ratatui den ganzen Frame neu.
        fn resize(&mut self) {
            self.snap.clear();
            self.sealed.clear();
            self.width = 0;
            self.prev = Buffer::empty(Rect::new(0, 0, Self::W, Self::H));
        }
    }

    /// Der Dialog darf mit seinem gemalten Hintergrund weder bis zum rechten
    /// Terminalrand noch in die Folgezeile bis zum linken Rand hineinreichen –
    /// sein Feld endet am Dialogrechteck, der Canvas davor und daneben bleibt
    /// Canvas.
    ///
    /// Geprüft wird das am *Terminalbild* (siehe [`Terminalmodell::frame`]): Ein
    /// `ESC[K` füllt mit dem gerade *gesetzten* SGR-Hintergrund, es zählt also
    /// nicht der geplante, sondern der tatsächlich gesetzte Ton. Fehlt der
    /// Wechsel auf Terminal-Default, malt der Canvas-Abschluss den Dialog-Ton in
    /// den Randbereich – bis zum rechten Terminalrand bzw. bis zum linken Rand der
    /// Folgezeile.
    ///
    /// Zusätzlich wird für die User-Aussage geprüft, dass ihre Zeilen (mehrzeilig,
    /// über die volle Breite gemalt) jeweils direkt nach dem Text abgeschlossen
    /// werden – sonst nähme eine Kopie über sie hinweg die volle Breite an
    /// Leerzeichen mit.
    #[test]
    fn overlay_dialog_bleibt_in_seinem_rect_und_user_zeilen_enden_am_text() {
        use crate::channel::ChannelRegistry;
        use crate::config::Config;
        use std::sync::mpsc;

        let cfg = || Config {
            model: "test/m".into(),
            mouse: false,
            ..Config::default()
        };
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(cfg(), ChannelRegistry::new_with_warnings(&cfg()).0, tx, rx);
        app.sessions[app.active].push_user_message(
            "erste frage\n\nzweite absatz ueber mehrere zeilen, damit das band umbricht".into(),
            None,
            "test/m".into(),
        );

        let mut m = Terminalmodell::new(app);
        m.frame("start");
        // Jede über die volle Breite gemalte Zeile (User-Band, Eingabe-/Statusband)
        // endet am Text: ein Abschluss mit der Bandfarbe direkt dahinter. Zeilen,
        // deren Text bis in die letzte Spalte reicht, haben nichts abzuschließen.
        let band = super::super::theme().band_bg;
        let mut geprueft = 0;
        for y in 0..m.prev.area.height {
            let zeile_band =
                (0..Terminalmodell::W).all(|x| m.prev.cell((x, y)).expect("Zelle").bg == band);
            if !zeile_band {
                continue;
            }
            let letzter_text = (0..Terminalmodell::W)
                .rfind(|&x| m.prev.cell((x, y)).expect("Zelle").symbol() != " ");
            let Some(x_text) = letzter_text else {
                continue; // leere Bandzeile (z. B. leere Eingabe)
            };
            if x_text + 1 >= Terminalmodell::W {
                continue; // Text reicht bis in die letzte Spalte
            }
            geprueft += 1;
            assert!(
                m.letzte_els.contains(&El {
                    x: x_text + 1,
                    y,
                    bg: band
                }),
                "Bandzeile {y} wird nicht nach Spalte {} abgeschlossen: {:?}",
                x_text + 1,
                m.letzte_els
            );
        }
        assert!(
            geprueft >= 3,
            "erwartet werden mehrere Bandzeilen (User-Aussage, Eingabe, Status), \
             geprüft: {geprueft}"
        );

        m.app.open_channel_picker();
        m.frame("picker offen");
        m.app
            .channel_picker
            .as_mut()
            .expect("Picker offen")
            .items
            .set_cursor(1);
        m.frame("cursor unten");
        // Fenstergröße geändert, Dialog bleibt offen: alle Zeilen sind berührt,
        // also werden auch die *leeren* Zeilen unterhalb des Dialogs neu
        // abgeschlossen – ab Spalte 0, mit Canvas-Hintergrund. Erbt dieser Reset
        // den Dialog-Ton, färbt sich die Zeile bis zum linken Rand ein.
        m.resize();
        m.frame("resize bei offenem Dialog");
        m.app.channel_picker = None;
        m.frame("picker zu");

        // Befunde nach Frame gruppieren: so ist beim Fehlschlag direkt sichtbar,
        // in welchem Frame es ausbricht – die Randzellen rechts (Dialog läuft bis
        // zum Terminalrand) und die Zeilen ab Spalte 0 (Dialog-Ton läuft bis zum
        // linken Rand) sind die beiden Symptome.
        let mut bericht: Vec<(String, usize, String)> = Vec::new();
        for b in &m.befunde {
            let (label, rest) = b.split_once(": ").expect("Befund 'Frame: (x,y) …'");
            match bericht.iter_mut().find(|(l, ..)| l == label) {
                Some((_, n, _)) => *n += 1,
                None => bericht.push((label.to_string(), 1, rest.to_string())),
            }
        }
        assert!(
            m.befunde.is_empty(),
            "{} Zellen weichen vom gezeichneten Buffer ab:\n{}",
            m.befunde.len(),
            bericht
                .iter()
                .map(|(l, n, rest)| format!("  {l}: {n} Zellen, z. B.{rest}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
