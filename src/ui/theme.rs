//! Farben, Abstände und sonstige Layout-Konstanten des Farbschemas.
//!
//! Das Farbschema ist als konsolidierte `Theme`-Struktur modelliert (2 feste
//! Instanzen `DARK`/`LIGHT`) und wird zur Laufzeit aufgelöst und austauschbar
//! abgelegt (`set_theme`, Zugriff über `theme()`). Die übrigen Untermodule
//! greifen über `super::*` auf `theme()` zu; `mod.rs` re-exportiert alles per
//! `pub(crate) use theme::*;`.
//!
//! Der Hauptinhalt (Chat-Kanvas) trägt bewusst KEINE explizite Fläche:
//! `CANVAS_BG` ist `Color::Reset` (= Terminal-Default, dort also transparent,
//! wenn das Terminal einen transparenten Grund hat). Explizite Hintergründe
//! gibt es nur noch für Panels/Bänder, Code-/Konsolen-Boxen und Overlay-
//! Dialoge – `theme` steuert dafür ausschließlich die Akzent-/Panel-Palette.

use ratatui::style::Color;

/// Grundfläche des Chat-Kanvas: bewusst `Color::Reset`, der Terminal-Default
/// (wird zum „Hintergrund durchlassen“ = transparent, falls das Terminal einen
/// transparenten Grund hat). Nicht theme-abhängig.
pub(crate) const CANVAS_BG: Color = Color::Reset;

/// Aufgelöstes Farbschema. Alle Felder sind theme-abhängig – nur die
/// Signal-/Layout-Konstanten (Abstände) sind `const` (siehe unten).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    /// Fläche für Konsolen-Boxen und Code-Bänder (war `BOX_BG`/`CODE_BG`).
    pub surface_bg: Color,
    /// Text auf `surface_bg` (war `BOX_TEXT`/`CODE_FG`).
    pub surface_fg: Color,
    /// Randstege der Konsolen-Box (war `BOX_BORDER`).
    pub surface_border: Color,
    /// Eingabe-Band (war `INPUT_BG`).
    pub band_bg: Color,
    /// Statuszeile + Dialog-/Picker-Fläche (war `STATUS_BG`).
    pub status_bg: Color,
    /// Text auf Bändern (`Color::White`-Ersatz: hell im Dark, dunkel im Light).
    pub band_fg: Color,
    /// Sekundärtext, Gedanken, Hints (war `MUTED`/`SYM_MUTED`).
    pub muted: Color,
    /// Akzentblau für Titel/Links/„+“ (war `ACCENT`).
    pub accent: Color,
    /// Warmgold für ausgewählte/aktive Picker-Einträge (war `ACCENT_FG`).
    pub highlight: Color,
    /// Fehler/Signalrot (war `ERROR_FG`/`SYM_ERR`).
    pub err: Color,
    /// Dezentes Fehlerrot (war `ERROR_DIM`).
    pub err_dim: Color,
    /// Erfolg/Signalgrün (war `SYM_OK`).
    pub ok: Color,
    /// Warnung/Amber (war `SYM_WARN`).
    pub warn: Color,
    /// Diff gelöscht – eigener Name, teilt die Farbe mit `err`.
    pub diff_del_fg: Color,
    pub diff_del_bg: Color,
    pub diff_del_mark_bg: Color,
    /// Diff hinzugefügt – eigener Name, teilt die Farbe mit `ok`.
    pub diff_add_fg: Color,
    pub diff_add_bg: Color,
    pub diff_add_mark_bg: Color,
}

/// Dunkles Theme (bisheriges Farbschema, unverändert).
pub(crate) const DARK: Theme = Theme {
    surface_bg: Color::Rgb(11, 12, 17),
    surface_fg: Color::Rgb(217, 217, 224),
    surface_border: Color::Rgb(80, 86, 100),
    band_bg: Color::Rgb(40, 43, 54),
    status_bg: Color::Rgb(26, 28, 36),
    band_fg: Color::Rgb(231, 233, 240),
    muted: Color::Rgb(120, 124, 138),
    accent: Color::Rgb(121, 182, 242),
    highlight: Color::Rgb(229, 192, 123),
    err: Color::Rgb(240, 113, 120),
    err_dim: Color::Rgb(198, 118, 122),
    ok: Color::Rgb(129, 201, 149),
    warn: Color::Rgb(238, 198, 93),
    diff_del_fg: Color::Rgb(240, 113, 120),
    diff_del_bg: Color::Rgb(38, 20, 22),
    diff_del_mark_bg: Color::Rgb(88, 32, 36),
    diff_add_fg: Color::Rgb(129, 201, 149),
    diff_add_bg: Color::Rgb(18, 38, 24),
    diff_add_mark_bg: Color::Rgb(24, 82, 44),
};

/// Helles Theme: Grundflächen invertiert, Signalfarben im Farbton erhalten,
/// aber für Kontrast auf hellem Grund abgedunkelt.
pub(crate) const LIGHT: Theme = Theme {
    surface_bg: Color::Rgb(233, 234, 240),
    surface_fg: Color::Rgb(43, 44, 51),
    surface_border: Color::Rgb(184, 187, 200),
    band_bg: Color::Rgb(226, 228, 235),
    status_bg: Color::Rgb(231, 233, 240),
    band_fg: Color::Rgb(43, 44, 51),
    muted: Color::Rgb(92, 95, 110),
    accent: Color::Rgb(31, 111, 190),
    highlight: Color::Rgb(138, 97, 22),
    err: Color::Rgb(192, 57, 70),
    err_dim: Color::Rgb(158, 74, 82),
    ok: Color::Rgb(46, 125, 79),
    warn: Color::Rgb(138, 106, 0),
    diff_del_fg: Color::Rgb(192, 57, 70),
    diff_del_bg: Color::Rgb(251, 233, 235),
    diff_del_mark_bg: Color::Rgb(243, 201, 206),
    diff_add_fg: Color::Rgb(46, 125, 79),
    diff_add_bg: Color::Rgb(234, 246, 238),
    diff_add_mark_bg: Color::Rgb(197, 231, 211),
};

/// Modus aus der Config (`theme = "dark" | "light" | "auto"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ThemeChoice {
    /// Terminal-Defaultfarben erkennen (OSC-11, Fallback `COLORFGBG`).
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemeChoice {
    /// Parst den Config-String (Groß/Kleinschreibung egal); unbekannt → `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(ThemeChoice::Auto),
            "dark" => Some(ThemeChoice::Dark),
            "light" => Some(ThemeChoice::Light),
            _ => None,
        }
    }

    /// Anzeigename für Rückmeldungen (z. B. `/theme`).
    pub fn name(self) -> &'static str {
        match self {
            ThemeChoice::Auto => "auto",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }
}

/// Aktuell aufgelöstes Theme – bis zur Auflösung beim Start (oder außerhalb
/// davon, z. B. in Tests) das Dunkle. Über ein `RwLock` austauschbar, damit
/// das Theme zur Laufzeit (z. B. `/theme`) umschaltbar ist.
static THEME: std::sync::OnceLock<std::sync::RwLock<Theme>> = std::sync::OnceLock::new();

fn theme_lock() -> &'static std::sync::RwLock<Theme> {
    THEME.get_or_init(|| std::sync::RwLock::new(DARK))
}

/// Aktuell aufgelöstes Theme (Kopie; `Theme` ist `Copy`).
pub(crate) fn theme() -> Theme {
    *theme_lock().read().expect("theme-Lock gelesen")
}

/// Monoton steigender Versionszähler des Themes: wird bei jedem `set_theme`
/// erhöht. Daran hängt die Gültigkeit des `HistoryCache` – das Theme ist damit
/// Teil des Cache-Schlüssels, sodass ein Theme-Wechsel (`/theme`) die bereits
/// umgebrochene Historie beim nächsten Frame sofort mit dem neuen Schema neu
/// einfärbt (statt die alten Farben bis zur nächsten Chat-Änderung zu behalten).
static THEME_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Aktuelle Theme-Version (monoton steigend; 0 vor der ersten Auflösung).
pub(crate) fn theme_version() -> u64 {
    THEME_VERSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// Setzt das Theme (Start-Auflösung und Laufzeit-Umschaltung). Jedes Setzen
/// erhöht die Theme-Version – abhängige Caches (z. B. `HistoryCache`) sehen den
/// Wechsel damit und bauen sich beim nächsten Frame neu auf.
pub(crate) fn set_theme(t: Theme) {
    *theme_lock().write().expect("theme-Lock geschrieben") = t;
    THEME_VERSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Ergebnis der `Auto`-Erkennung, einmalig beim ersten Aufruf berechnet und
/// gecacht. `/theme auto` wechselt damit reproduzierbar auf die
/// Start-Entscheidung zurück, ohne mitten in der Session erneut das TTY zu
/// befragen.
static AUTO_RESULT: std::sync::OnceLock<Theme> = std::sync::OnceLock::new();

/// Gecachtes `Auto`-Ergebnis – `None`, solange niemand `resolve(Auto)`
/// aufgerufen hat (z. B. Start mit explizitem `dark`/`light`).
pub(crate) fn auto_resolved() -> Option<Theme> {
    AUTO_RESULT.get().copied()
}

/// Löst `ThemeChoice::Auto` auf (OSC-Erkennung, gecacht).
fn resolve_auto() -> Theme {
    *AUTO_RESULT.get_or_init(|| if detect_light_terminal() { LIGHT } else { DARK })
}

/// Löst den gewählten Modus zu einem konkreten `Theme` auf. Bei `Auto` wird
/// der Terminal-Default-Hintergrund erkannt (OSC-11-Abfrage, Fallback
/// `COLORFGBG`) und über die Luminanz entschieden; ohne erkennbaren hellen
/// Grund bleibt es beim dunklen Theme.
pub(crate) fn resolve(choice: ThemeChoice) -> Theme {
    match choice {
        ThemeChoice::Dark => DARK,
        ThemeChoice::Light => LIGHT,
        ThemeChoice::Auto => resolve_auto(),
    }
}

/// Erkennt einen hellen Terminal-Default-Hintergrund. Primär die Luminanz der
/// OSC-11-Antwort; in der Grauzone (~0.45–0.55) wird die Default-Vordergrund-
/// Luminanz (OSC-10) als Gegenprobe herangezogen (helles fg ⇒ dunkler Grund).
/// Fallback: `COLORFGBG`; ohne Treffer → dunkel.
fn detect_light_terminal() -> bool {
    if let Some((bg, fg)) = query_default_colors() {
        let Some(bg) = bg else {
            // Nur der Vordergrund ist bekannt: helles fg ⇒ dunkler Grund.
            return match fg {
                Some(fg) => luminance(fg) <= 0.5,
                None => false,
            };
        };
        let bl = luminance(bg);
        if bl > 0.55 {
            return true;
        }
        if bl < 0.45 {
            return false;
        }
        // Grauzone: über den Vordergrund entscheiden (helles fg ⇒ dunkel).
        return match fg {
            Some(fg) => luminance(fg) <= 0.55,
            None => bl > 0.5,
        };
    }
    if let Some(rgb) = colorfgbg_bg_rgb() {
        return luminance(rgb) > 0.5;
    }
    false
}

/// Helligkeit (0..1) nach dem groben sRGB-Gewichtungsmodell.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) / 255.0
}

/// Ein RGB-Tripel (je 0–255).
type Rgb8 = (u8, u8, u8);

/// Ergebnis der OSC-Abfrage: `(hintergrund, vordergrund)`, je optional.
type DefaultColors = (Option<Rgb8>, Option<Rgb8>);

/// Fragt Default-Vordergrund- (OSC 10) und -Hintergrundfarbe (OSC 11) ab:
/// `\x1b]10;?\x1b\\` + `\x1b]11;?\x1b\\`. Vor Raw-Mode aufrufen; die Antworten
/// werden mit kurzzeitigem Termios-Timeout (VTIME) direkt vom TTY-FD gelesen
/// und je Befehl (`ps` = 10/11) als `rgb:` geparst.
#[cfg(unix)]
fn query_default_colors() -> Option<DefaultColors> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let fd = std::io::stdin().as_raw_fd();
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\").ok()?;
    out.flush().ok()?;

    let terminations = |reply: &[u8]| {
        reply.iter().filter(|&&b| b == 0x07).count()
            + reply.windows(2).filter(|w| w == b"\x1b\\").count()
    };

    unsafe {
        let mut old: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut old) != 0 {
            return None;
        }
        let mut raw = old;
        // Ohne Kanonisches-/Echo-Verhalten, damit die Antwort direkt ankommt.
        raw.c_lflag &= !(libc::ICANON as libc::tcflag_t | libc::ECHO as libc::tcflag_t);
        // Blockierendes Lesen begrenzen: `VTIME` = 4 Dezi-Sekunden (~0.4 s).
        // Ein Terminal, das nur eine der beiden Abfragen beantwortet, kostet so
        // höchstens ~0.4 s Startzeit.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 4;
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        let guard = TermiosGuard { fd, old };
        let mut buf = [0u8; 512];
        let mut reply: Vec<u8> = Vec::new();
        while reply.len() < 512 {
            let n = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n <= 0 {
                break;
            }
            reply.extend_from_slice(&buf[..n as usize]);
            // Abbruch, sobald (mindestens) eine Antwort je Abfrage da ist –
            // oder beide Terminatoren gesehen wurden.
            if terminations(&reply) >= 2 {
                break;
            }
        }
        drop(guard);
        let fg = parse_osc_ps(&reply, 10);
        let bg = parse_osc_ps(&reply, 11);
        if fg.is_none() && bg.is_none() {
            None
        } else {
            Some((bg, fg))
        }
    }
}

/// Stellt die Termios-Originalwerte nach der OSC-Abfrage wieder her.
#[cfg(unix)]
struct TermiosGuard {
    fd: std::os::unix::io::RawFd,
    old: libc::termios,
}

#[cfg(unix)]
impl Drop for TermiosGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.old) };
    }
}

/// Nicht-Unix-Fallback: keine Terminal-Abfrage möglich.
#[cfg(not(unix))]
fn query_default_colors() -> Option<DefaultColors> {
    None
}

/// Parst aus einer OSC-Antwort (`ps` = 10 oder 11) den `rgb:`-Teil, z. B.
/// `\x1b]11;rgb:1414/1414/3030\x1b\\` oder `…rgb:1a/1a/2e\x07`.
fn parse_osc_ps(reply: &[u8], ps: u8) -> Option<(u8, u8, u8)> {
    let s = String::from_utf8_lossy(reply);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' {
            let mut j = i + 2;
            let mut num: u32 = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                num = num * 10 + (bytes[j] - b'0') as u32;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b';' && num == ps as u32 {
                let rest = &s[j + 1..];
                let at = rest.find("rgb:")?;
                return parse_rgb(&rest[at + 4..]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Parst `RR/GG/BB` bzw. `RRRR/GGGG/BBBB` (jede Komponente 1–4 Hex-Ziffern,
/// auf 8 Bit skaliert).
fn parse_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let mut parts = s.split(|c: char| c.is_control() || c == '/');
    let mut comps = [0u8; 3];
    for slot in comps.iter_mut() {
        let h = parts.next()?.trim();
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        *slot = ((v * 255 + max / 2) / max) as u8;
    }
    Some((comps[0], comps[1], comps[2]))
}

/// Fallback `COLORFGBG` (rxvt/urxvt/screen): `"<fg>;<bg>"` als 16-Farb-
/// Paletten-Indizes → grobe RGB-Schätzung.
fn colorfgbg_bg_rgb() -> Option<(u8, u8, u8)> {
    let v = std::env::var("COLORFGBG").ok()?;
    let bg = v.rsplit(';').next()?;
    let idx: usize = bg.parse().ok()?;
    const VGA: [(u8, u8, u8); 16] = [
        (0, 0, 0),       // 0  schwarz
        (128, 0, 0),     // 1  rot
        (0, 128, 0),     // 2  grün
        (128, 128, 0),   // 3  gelb
        (0, 0, 128),     // 4  blau
        (128, 0, 128),   // 5  magenta
        (0, 128, 128),   // 6  cyan
        (192, 192, 192), // 7  hellgrau
        (128, 128, 128), // 8  dunkelgrau
        (255, 0, 0),     // 9  hellrot
        (0, 255, 0),     // 10 hellgrün
        (255, 255, 0),   // 11 hellgelb
        (0, 0, 255),     // 12 hellblau
        (255, 0, 255),   // 13 hellmagenta
        (0, 255, 255),   // 14 hellcyan
        (255, 255, 255), // 15 weiß
    ];
    VGA.get(idx).copied()
}

// ---------------------------------------------------------------------------
// Layout-/Abstands-Konstanten (theme-unabhängig)
// ---------------------------------------------------------------------------

/// Horizontaler Abstand des Textes von den Rändern.
pub(crate) const PAD: usize = 2;
/// Rechter Rand: symmetrisch zum linken Einzug (Text endet `PAD_R` Zellen vor
/// dem Bildschirmrand, Blöcke mit Hintergrund lassen diese Zellen frei).
pub(crate) const PAD_R: usize = 2;
/// Zusätzlicher Einzug für das Gedanken-Element.
pub(crate) const THOUGHT_INDENT: usize = PAD + 2;
/// Abstand der Konsolen-Box vom Bildschirmrand (links/rechts).
pub(crate) const BOX_MARGIN: usize = 3;
/// Innenabstand des Texts zur Box-Umrandung (links/rechts).
pub(crate) const BOX_PAD: usize = 2;
/// Maximale Anzahl Ausgabezeilen, die eine zuklappte Run-Konsolen-Box zeigt.
pub(crate) const RUN_PREVIEW_LINES: usize = 5;
/// Innenabstand (links/rechts) des Inhalts im Kanal-Auswahl-Overlay.
pub(crate) const PICKER_PAD: usize = 4;
/// Maximal angezeigte Zeilenpaare einer zugeklappten Diff-Box.
pub(crate) const DIFF_PREVIEW_ROWS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_choice_parse_dunkel_hell_auto() {
        assert_eq!(ThemeChoice::parse("dark"), Some(ThemeChoice::Dark));
        assert_eq!(ThemeChoice::parse("Light"), Some(ThemeChoice::Light));
        assert_eq!(ThemeChoice::parse("AUTO"), Some(ThemeChoice::Auto));
        assert_eq!(ThemeChoice::parse("  auto  "), Some(ThemeChoice::Auto));
        assert_eq!(ThemeChoice::parse("bogus"), None);
        assert_eq!(ThemeChoice::parse(""), None);
    }

    #[test]
    fn dunkel_behaelt_altes_schema_und_signalfarben_bleiben_gleich() {
        // Konsolidierung: rote Signalfarbe = err (ex-DEL_FG), grüne = ok.
        assert_eq!(DARK.err, Color::Rgb(240, 113, 120), "Rot wie zuvor");
        assert_eq!(DARK.diff_del_fg, DARK.err, "diff_del_fg teilt err");
        assert_eq!(DARK.ok, Color::Rgb(129, 201, 149), "Grün wie zuvor");
        assert_eq!(DARK.diff_add_fg, DARK.ok, "diff_add_fg teilt ok");
        assert_eq!(DARK.accent, Color::Rgb(121, 182, 242), "Blau wie zuvor");
        assert_eq!(DARK.muted, Color::Rgb(120, 124, 138), "Grau wie zuvor");
        assert_eq!(DARK.surface_bg, Color::Rgb(11, 12, 17), "BOX/CODE vereint");
    }

    #[test]
    fn kanvas_ist_terminal_default_und_theme_faerb_t_nur_panels() {
        // Der Kanvas ist in beiden Themes gleich (Transparenz/Default).
        assert_eq!(CANVAS_BG, Color::Reset, "Kanvas = Terminal-Default");
        assert_eq!(DARK.surface_bg, Color::Rgb(11, 12, 17), "BOX/CODE vereint");
    }

    #[test]
    fn hell_invertiert_hauptfarben_und_haelt_signalfarbton() {
        let lum = |c: Color| match c {
            Color::Rgb(r, g, b) => luminance((r, g, b)),
            _ => 0.0,
        };
        let rgb = |c: Color| match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        };
        // Panels/Text invertieren (dunkel ↔ hell) – der Kanvas ist dagegen in
        // beiden Themes Terminal-Default (siehe `kanvas_ist_terminal_default…`).
        assert!(lum(LIGHT.surface_fg) < lum(DARK.surface_fg));
        assert!(lum(LIGHT.muted) < lum(DARK.muted));
        // Signalfarben bleiben grün/rot/amber/blau (Hue erhalten), nur dunkler.
        let (lr, lg, lb) = (rgb(LIGHT.err), rgb(LIGHT.ok), rgb(LIGHT.accent));
        assert!(lr.0 > lg.0 && lg.1 > lr.1, "err=rötlich, ok=grünlich");
        assert!(lb.2 > lb.0, "accent bläulich");
        assert!(lum(LIGHT.err) < lum(DARK.err), "Signal rot abgedunkelt");
        assert!(lum(LIGHT.ok) < lum(DARK.ok), "Signal grün abgedunkelt");
    }

    #[test]
    fn osc_antworten_werden_pro_ps_geparst_2er_und_4er_hex() {
        // Nur Hintergrund (OSC 11).
        assert_eq!(
            parse_osc_ps(b"\x1b]11;rgb:1a1a/2b2b/3c3c\x1b\\", 11),
            Some((0x1a, 0x2b, 0x3c))
        );
        assert_eq!(
            parse_osc_ps(b"\x1b]11;rgb:1a/2b/3c\x07", 11),
            Some((0x1a, 0x2b, 0x3c))
        );
        assert_eq!(parse_osc_ps(b"nix", 11), None);
        // Beide Abfragen in einem Puffer, getrennt nach ps.
        let both = b"\x1b]10;rgb:fafa/fafa/fafa\x1b\\\x1b]11;rgb:1414/1414/3030\x1b\\";
        assert_eq!(parse_osc_ps(both, 10), Some((0xfa, 0xfa, 0xfa)));
        assert_eq!(parse_osc_ps(both, 11), Some((0x14, 0x14, 0x30)));
    }

    #[test]
    fn detection_faellt_ohne_terminal_und_env_auf_dunkel() {
        // Ohne TTY-Antwort und ohne COLORFGBG bleibt es beim dunklen Fallback.
        // (die eigentliche OSC-Logik hängt am TTY-IO und wird nicht eingebunden)
        assert!(luminance((200, 200, 200)) > 0.55, "helle Schwelle");
        assert!(luminance((70, 70, 70)) < 0.45, "dunkle Schwelle");
        assert!(!detect_light_terminal(), "kein COLORFGBG → dunkel");
    }

    #[test]
    fn colorfgbg_hintergrund_aus_indizes_ablesbar() {
        // "15;0" → Hintergrund-Index 0 (schwarz) → dunkel.
        assert_eq!(colorfgbg_bg_rgb(), None, "Env fehlt im Test");
        let old = std::env::var_os("COLORFGBG");
        std::env::set_var("COLORFGBG", "15;0");
        assert_eq!(colorfgbg_bg_rgb(), Some((0, 0, 0)));
        std::env::set_var("COLORFGBG", "0;15");
        assert_eq!(colorfgbg_bg_rgb(), Some((255, 255, 255)));
        if let Some(v) = old {
            std::env::set_var("COLORFGBG", v);
        } else {
            std::env::remove_var("COLORFGBG");
        }
    }
}
