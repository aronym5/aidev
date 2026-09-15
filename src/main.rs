mod app;
mod channel;
mod chat;
mod config;
mod diff;
mod editor;
mod llm;
mod perm;
mod repo;
mod text;
mod ui;

use std::io::{self, Write};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Help/version need no config (and take precedence over everything else).
    if let Some(code) = probe_cli(&args) {
        std::process::exit(code);
    }

    let config = config::Config::load();

    // Theme auflösen: `auto` fragt die Terminal-Defaultfarben per OSC-11 ab.
    // Muss VOR Raw-Mode/Alt-Screen passieren (siehe theme::resolve).
    let theme_choice = ui::ThemeChoice::parse(&config.theme).unwrap_or_default();
    ui::set_theme(ui::resolve(theme_choice));

    crossterm::terminal::enable_raw_mode()?;
    let guard = TerminalGuard;
    execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;

    // Aktuellen Fenster-Titel auf den xterm-Titel-Stack pushen (`ESC[22;0t`),
    // damit wir ihn beim Beenden wiederherstellen können (`ESC[23;0t` in
    // `TerminalGuard::drop`). Der OSC-0-Titel von aidev ist global für den
    // Tab/Window (nicht am Alternate Screen gebunden) und würde sonst nach
    // dem Exit stehen bleiben. Terminals ohne Titel-Stack (z. B. GNOME
    // Terminal, Windows Terminal) ignorieren die Sequenz harmlos.
    let _ = io::stdout().write_all(b"\x1b[22;0t");
    let _ = io::stdout().flush();

    // Keyboard-Enhancement aktivieren (Kitty-Protokoll), damit Numpad-Tasten
    // (KP_+, KP_-, KP_Enter usw.) als eigenständige KeyCodes erkannt werden
    // und nicht als unbekannte SS3-Sequenzen verloren gehen.
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    // Bracketed Paste einschalten: Damit kommt eingefügter (mehrzeiliger) Text
    // als ein zusammenhängendes `Paste`-Event an statt als einzelne Key-Events
    // (deren `\n` sonst als Enter = Senden interpretiert würde).
    let _ = execute!(io::stdout(), EnableBracketedPaste);

    // Maus-Reporting nur aktivieren, wenn in der Config gewünscht (`mouse = true`).
    // Ohne Maus-Reporting funktioniert Textmarkierung und Einfügen mit der
    // mittleren Maustaste in allen Terminals einwandfrei; PgUp/PgDn scrollen
    // den Chat trotzdem.  Mit `mouse = true` wird das xterm-Protokoll aktiviert
    // (`\x1b[?1000h` = Button-Events inkl. Scroll-Rad, `\x1b[?1006h` = SGR).
    if config.mouse {
        io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h")?;
        io::stdout().flush()?;
    }

    let result = app::run(config);

    // Alt-Screen + Raw-Mode immer aufräumen (via Drop), egal wie `run` endet.
    drop(guard);
    result
}

/// Handles the standard `--help` / `--version` flags. Returns `Some(exit_code)`
/// when one of them was given; `None` otherwise (i.e. the TUI should start).
fn probe_cli(args: &[String]) -> Option<i32> {
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("aidev {}", config::VERSION);
            Some(0)
        }
        Some("-h" | "--help") => {
            print!(
                "\
aidev {} – terminal AI coding agent with a per-session sandbox

USAGE: aidev [OPTIONS]

OPTIONS:
  -h, --help       Print this help message
  -V, --version    Print version information

With no arguments, the interactive TUI starts.
",
                config::VERSION
            );
            Some(0)
        }
        _ => None,
    }
}

/// Deaktiviert Raw-Mode & Alt-Screen beim Verlassen (auch bei `?`-Fehlern).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Maus-Reporting deaktivieren, bevor wir den Alt-Screen verlassen.
        let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l");
        let _ = io::stdout().flush();
        // Vorher gesicherten Fenster-Titel wiederherstellen (`ESC[23;0t`,
        // Gegenstück zum Push in `main`). In Terminals ohne Titel-Stack ein
        // No-Op – dort bleibt der zuletzt gesetzte Titel stehen.
        let _ = io::stdout().write_all(b"\x1b[23;0t");
        let _ = io::stdout().flush();
        // Bracketed Paste wieder ausschalten (sonst bliebe das Terminal im
        // Zustand und z. B. die Shell würde eingefügte Newlines maskieren).
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
