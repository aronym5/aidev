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
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

fn main() -> io::Result<()> {
    let config = config::Config::load();

    // Kanal-Probe ohne TUI: --channel-ls / --channel-run beenden direkt.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = probe_channel_cli(&args, &config) {
        std::process::exit(code);
    }

    crossterm::terminal::enable_raw_mode()?;
    let guard = TerminalGuard;
    execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;

    // Keyboard-Enhancement aktivieren (Kitty-Protokoll), damit Numpad-Tasten
    // (KP_+, KP_-, KP_Enter usw.) als eigenständige KeyCodes erkannt werden
    // und nicht als unbekannte SS3-Sequenzen verloren gehen.
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

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

/// Deaktiviert Raw-Mode & Alt-Screen beim Verlassen (auch bei `?`-Fehlern).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Maus-Reporting deaktivieren, bevor wir den Alt-Screen verlassen.
        let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l");
        let _ = io::stdout().flush();
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Verarbeitet `--channel-ls` / `--channel-run`: Ergebnis ausgeben und Exit-Code
/// 0/1 liefern; `None`, wenn die Argumente zur TUI gehören (oder leer sind).
fn probe_channel_cli(args: &[String], cfg: &config::Config) -> Option<i32> {
    let flag = args.first()?;
    let registry = channel::ChannelRegistry::new(cfg);
    let out = match flag.as_str() {
        "--channel-ls" => channel::cli_ls(&registry, args.get(1).map(String::as_str)),
        "--channel-run" => {
            // Optionaler Kanalname direkt vor dem Befehl (nur wenn er existiert).
            let named = args.get(1).is_some_and(|t| registry.get(t).is_some());
            let start = if named { 2 } else { 1 };
            match args.get(start) {
                Some(cmd) => channel::cli_run(
                    &registry,
                    (named).then(|| args[1].as_str()),
                    cmd,
                    &args[start + 1..],
                ),
                None => {
                    Err("Nutzung: aidev --channel-run [<kanal>] <befehl> [argumente…]".to_owned())
                }
            }
        }
        "-h" | "--help" => {
            print!(
                "Nutzung: aidev [--channel-ls [<kanal>]] [--channel-run [<kanal>] <befehl> <arg>…]\n\n\
                 --channel-ls    listet die Wurzel eines Kanals\n\
                 --channel-run   führt ein Kommando im Kanal aus\n\
                 (ohne Argumente startet die TUI)\n"
            );
            return Some(0);
        }
        _ => return None,
    };
    match out {
        Ok(text) => {
            print!("{text}");
            Some(0)
        }
        Err(err) => {
            eprintln!("[aidev] {err}");
            Some(1)
        }
    }
}
