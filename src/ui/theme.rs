//! Farben, Abstände und sonstige Layout-Konstanten des Farbschemas.
//!
//! Alle Konstanten hier sind `pub(crate)` und werden von `mod.rs` per
//! `pub(crate) use theme::*;` re-exportiert, damit die übrigen Untermodule
//! (und `markdown.rs`) sie über `super::*` erreichen können.

use ratatui::style::Color;

// Modernes, dezentes Farbschema.
pub(crate) const BASE_BG: Color = Color::Rgb(18, 19, 25);
pub(crate) const INPUT_BG: Color = Color::Rgb(40, 43, 54);
pub(crate) const STATUS_BG: Color = Color::Rgb(26, 28, 36);
pub(crate) const ACCENT: Color = Color::Rgb(121, 182, 242);
pub(crate) const MUTED: Color = Color::Rgb(120, 124, 138);
pub(crate) const ERROR_FG: Color = Color::Rgb(240, 113, 120);
/// Dezente Fehlerfarbe für Werkzeug-Fehlerzeilen/-Köpfe (weniger grell).
pub(crate) const ERROR_DIM: Color = Color::Rgb(198, 118, 122);
/// Hintergrund der Run-Konsolen-Box (fast schwarz, wie eine Konsole/Ausgabe).
pub(crate) const BOX_BG: Color = Color::Rgb(11, 12, 17);
/// Randstege der Konsolen-Box.
pub(crate) const BOX_BORDER: Color = Color::Rgb(80, 86, 100);
/// Text in der Konsolen-Box (zartes Weiß, wie das `code`-Rendering).
pub(crate) const BOX_TEXT: Color = Color::Rgb(217, 217, 224);
/// Abstand der Konsolen-Box vom Bildschirmrand (links/rechts).
pub(crate) const BOX_MARGIN: usize = 3;
/// Innenabstand des Texts zur Box-Umrandung (links/rechts).
pub(crate) const BOX_PAD: usize = 2;
/// Maximale Anzahl Ausgabezeilen, die eine zuklappte Run-Konsolen-Box zeigt.
pub(crate) const RUN_PREVIEW_LINES: usize = 5;
/// Innenabstand (links/rechts) des Inhalts im Kanal-Auswahl-Overlay: Der Rand
/// gehört überall zum Overlay, erst danach beginnt der Inhalt (inkl. ⬢-Indikator).
pub(crate) const PICKER_PAD: usize = 4;
/// Hintergrund von Codeblöcken (Band, auch ohne Syntax-Highlighting).
pub(crate) const CODE_BG: Color = Color::Rgb(11, 12, 17);
/// Textfarbe in Codeblöcken ohne Syntax-Hervorhebung (z. B. `toml`).
pub(crate) const CODE_FG: Color = Color::Rgb(205, 207, 214);
/// Farben der zweispaltigen Diff-Box: entfernte Zeilen (rot), hinzugefügte
/// Zeilen (grün); `_MARK_*` für die geänderten Zeichen innerhalb der Zeile.
pub(crate) const DIFF_DEL_FG: Color = Color::Rgb(240, 113, 120);
pub(crate) const DIFF_DEL_BG: Color = Color::Rgb(38, 20, 22);
pub(crate) const DIFF_DEL_MARK_BG: Color = Color::Rgb(88, 32, 36);
pub(crate) const DIFF_ADD_FG: Color = Color::Rgb(129, 201, 149);
pub(crate) const DIFF_ADD_BG: Color = Color::Rgb(18, 38, 24);
pub(crate) const DIFF_ADD_MARK_BG: Color = Color::Rgb(24, 82, 44);
/// Maximal angezeigte Zeilenpaare einer zugeklappten Diff-Box.
pub(crate) const DIFF_PREVIEW_ROWS: usize = 8;
/// Textfarbe für ausgewählte/aktive Einträge in Pickern.
pub(crate) const ACCENT_FG: Color = Color::Rgb(229, 192, 123);
/// Semantische Farben der Status-Symbole (✅/❌/⚠️ …).
pub(crate) const SYM_OK: Color = Color::Rgb(129, 201, 149);
pub(crate) const SYM_ERR: Color = Color::Rgb(240, 113, 120);
pub(crate) const SYM_WARN: Color = Color::Rgb(238, 198, 93);
pub(crate) const SYM_MUTED: Color = Color::Rgb(138, 141, 156);

/// Horizontaler Abstand des Textes von den Rändern.
pub(crate) const PAD: usize = 2;
/// Rechter Rand: symmetrisch zum linken Einzug (Text endet `PAD_R` Zellen vor
/// dem Bildschirmrand, Blöcke mit Hintergrund lassen diese Zellen frei).
pub(crate) const PAD_R: usize = 2;
/// Zusätzlicher Einzug für das Gedanken-Element.
pub(crate) const THOUGHT_INDENT: usize = PAD + 2;
