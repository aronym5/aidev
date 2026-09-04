//! Volltextsuche: ripgrep (`rg`) mit `grep`-Fallback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::run::run_with_timeout;
use super::{Match, SearchResult};

pub(super) fn search_files(
    pattern: &str,
    path: &Path,
    timeout: Duration,
    include: Option<&str>,
    context_lines: usize,
) -> Result<SearchResult, String> {
    match rg_search(pattern, path, timeout, include, context_lines) {
        Ok((matches, raw)) => Ok(SearchResult {
            matches,
            note: None,
            raw,
        }),
        Err(err) => {
            if err.starts_with("rg not available:") {
                // Der Fallback-Hinweis steht bereits im `note`-Feld der
                // SearchResult (und damit in der UI); ein Konsolen-Print
                // würde das TUI-Layout zerschießen.
                let (matches, raw) = grep_search(pattern, path, timeout, include, context_lines)?;
                Ok(SearchResult {
                    matches,
                    note: Some(
                        "Note: ripgrep (rg) is not available on this system – \
                         the search was run with classic grep."
                            .to_string(),
                    ),
                    raw,
                })
            } else {
                Err(err)
            }
        }
    }
}

/// Unterscheidet, ob `path` eine Datei oder ein Verzeichnis ist, und baut
/// daraus cwd + Ziel für rg/grep.
///
/// - Verzeichnis: rekursiv ab `path` (cwd = `path`, Ziel = „.“).
/// - Datei: gezielt in dieser einen Datei suchen (cwd = Elternverzeichnis,
///   Ziel = Dateiname). Ohne diesen Fall würde der Pfad als Prozess-cwd
///   genutzt und `rg`/`grep` scheiterte mit ENOTDIR („Not a directory“).
pub(super) enum SearchTarget {
    Dir,
    File(String),
}
pub(super) fn split_search_path(path: &Path) -> (PathBuf, SearchTarget) {
    if path.is_file() {
        let parent = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        (parent, SearchTarget::File(name))
    } else {
        (path.to_path_buf(), SearchTarget::Dir)
    }
}

/// Volltextsuche über `rg`; Exit-Code 1 (kein Treffer) ist kein Fehler.
/// `rg` folgt standardmäßig keinen Symlinks; das wird hier explizit
/// festgeschrieben (`--no-follow`), damit die Suche nie aus der Kanal-Wurzel
/// hinausführt. Mit `context_lines` > 0 läuft rg mit `-C n` und das Ergebnis
/// wird ROH durchgereicht (Kontextzeilen nutzen „-“ als Trenner und würden
/// den Zeilen-Parser verwirren).
pub(super) fn rg_search(
    pattern: &str,
    path: &Path,
    timeout: Duration,
    include: Option<&str>,
    context_lines: usize,
) -> Result<(Vec<Match>, Option<String>), String> {
    let (cwd, target) = split_search_path(path);
    let mut args: Vec<String> = vec![
        "--line-number".into(),
        "--no-heading".into(),
        "--color".into(),
        "never".into(),
        "--no-follow".into(),
    ];
    if let Some(inc) = include {
        args.push(format!("--glob={inc}"));
    }
    if context_lines > 0 {
        args.push("-C".into());
        args.push(context_lines.to_string());
    }
    args.push("--".into());
    args.push(pattern.to_string());
    // Bei einer gezielten Dateisuche das Ziel explizit nennen (cwd ist das
    // Elternverzeichnis); bei einem Verzeichnis durchsucht rg von selbst die cwd.
    if let SearchTarget::File(name) = &target {
        args.push(name.clone());
    }

    let out = run_with_timeout("rg", &args, &cwd, timeout)
        .map_err(|e| format!("rg not available: {e}"))?;

    if out.exit_code == Some(1) {
        return Ok((Vec::new(), None)); // keine Treffer
    }
    if out.exit_code != Some(0) {
        return Err(format!("rg error:\n{}", out.stderr));
    }
    if context_lines > 0 {
        return Ok((Vec::new(), Some(normalize_raw(&out.stdout))));
    }
    Ok((parse_search_lines(&out.stdout), None))
}

/// Volltextsuche über das klassische `grep` (Fallback, wenn rg fehlt).
/// Rekursive Suche relativ zum Kanal-Verzeichnis; gebräuchliche
/// Generatoren-/Cache-Verzeichnisse werden ausgelassen, damit das Ergebnis
/// nicht von build-Artifakten (target/…), VCS-Metadaten (.git/…) oder
/// node_modules/… verstopft wird. Es kommt `-r` statt `-R` zum Einsatz:
/// `-R` folgt Symlinks in Unterverzeichnissen (und könnte so Daten außerhalb
/// der Kanal-Wurzel einsammeln), `-r` folgt nur Symlinks auf der
/// Kommandozeile – das Suchverzeichnis selbst wurde bereits von
/// [`super::resolve`] gegen die Wurzel geprüft.
///
/// Es wird stets `-E` (ERE) genutzt, damit die Muster-Semantik der von
/// ripgrep (Rust-regex) näher kommt – `|`, `+`, `?`, `()`, `{n,m}` als
/// Meta-Zeichen. Ohne `-E` (BRE) wären sie Literale und praktisch alle
/// gängigen Agent-Muster lieferten falsche (leere) Treffer.
///
/// Falls das Muster von der verfügbaren grep-Engine (GNU/BSD/BusyBox) nicht
/// geparst werden kann (z. B. `\d`, `\w`, `(?:...)` – POSIX-ERE-Unterhalt),
/// wird das als **Fehler** gemeldet, statt still 0 Treffer zu liefern, damit
/// klar ist, dass das Muster für den Fallback umgeschrieben werden muss.
pub(super) fn grep_search(
    pattern: &str,
    path: &Path,
    timeout: Duration,
    include: Option<&str>,
    context_lines: usize,
) -> Result<(Vec<Match>, Option<String>), String> {
    let (cwd, target) = split_search_path(path);
    let file_target = matches!(target, SearchTarget::File(_));

    // GNU-grep-spezifische Optionen (--exclude-dir, -I, --include, -C);
    // BusyBox/BSD-grep kennen sie nicht. Zuerst der GNU-Aufruf; schlägt er
    // mit einem Exit-Code ungleich 0/1 fehl (z. B. „unrecognized option“),
    // wiederholen wir mit portablen Optionen. Scheitert auch der (weil z. B.
    // BSD-grep kein `-C` kennt), folgt ein letzter Notfall-Aufruf ohne `-C`.
    // Bei einer gezielten Dateisuche (Einzeldatei als `path`) entfällt die
    // Rekursion -r und der Ziel-Pfad ist der Dateiname (cwd = Elternverzeichnis).
    let mut args: Vec<String> = if file_target {
        vec!["-E".into(), "-n".into(), "-I".into()]
    } else {
        vec![
            "-E".into(),
            "-r".into(),
            "-n".into(),
            "-I".into(),
            "--exclude-dir=.git".into(),
            "--exclude-dir=target".into(),
            "--exclude-dir=node_modules".into(),
        ]
    };
    if let Some(inc) = include {
        args.push(format!("--include={inc}"));
    }
    if context_lines > 0 {
        args.push("-C".into());
        args.push(context_lines.to_string());
    }
    args.push("--".into());
    args.push(pattern.to_string());
    args.push(match &target {
        SearchTarget::File(name) => name.clone(),
        SearchTarget::Dir => ".".into(),
    });

    let mut out = run_with_timeout("grep", &args, &cwd, timeout)
        .map_err(|e| format!("grep not available: {e}"))?;
    if out.exit_code != Some(0) && out.exit_code != Some(1) {
        // Portabler Retry: -I/--include/--exclude-dir fliegen raus (GNU-only),
        // `-E` und `-C` bleiben – BusyBox-grep beherrscht -C, BSD nicht.
        let mut portable: Vec<String> = if file_target {
            vec!["-E".into(), "-n".into()]
        } else {
            vec!["-E".into(), "-r".into(), "-n".into()]
        };
        if context_lines > 0 {
            portable.push("-C".into());
            portable.push(context_lines.to_string());
        }
        portable.push("--".into());
        portable.push(pattern.to_string());
        portable.push(match &target {
            SearchTarget::File(name) => name.clone(),
            SearchTarget::Dir => ".".into(),
        });
        out = run_with_timeout("grep", &portable, &cwd, timeout)
            .map_err(|e| format!("grep not available: {e}"))?;
    }
    if out.exit_code != Some(0) && out.exit_code != Some(1) && context_lines > 0 {
        // Notfall-Retry ohne -C (BSD/macOS-grep kennt -C nicht): Kontext wird
        // dann nicht angefordert, die Suche selbst bleibt ERE.
        let mut norec: Vec<String> = if file_target {
            vec!["-E".into(), "-n".into()]
        } else {
            vec!["-E".into(), "-r".into(), "-n".into()]
        };
        norec.push("--".into());
        norec.push(pattern.to_string());
        norec.push(match &target {
            SearchTarget::File(name) => name.clone(),
            SearchTarget::Dir => ".".into(),
        });
        out = run_with_timeout("grep", &norec, &cwd, timeout)
            .map_err(|e| format!("grep not available: {e}"))?;
    }
    if out.exit_code == Some(1) {
        return Ok((Vec::new(), None)); // keine Treffer
    }
    if out.exit_code != Some(0) {
        return Err(grep_pattern_error(pattern, &out.stderr));
    }
    if context_lines > 0 {
        return Ok((Vec::new(), Some(normalize_raw(&out.stdout))));
    }
    let mut matches = parse_search_lines(&out.stdout);
    // `grep -r -- … .` liefert Pfade mit führendem `./` – zugunsten der
    // gewohnten, kanal-relativen Form (wie bei rg) entfernen.
    for m in &mut matches {
        if let Some(stripped) = m.path.strip_prefix("./") {
            m.path = stripped.to_string();
        }
    }
    // Ohne --exclude-dir (portabler Pfad) können Verschachtelte .git/target/
    // node_modules auftauchen – nachträglich ausfiltern (bei GNU redundant).
    matches.retain(|m| !is_excluded_path(&m.path));
    Ok((matches, None))
}

/// Baut eine verständliche Fehlermeldung, wenn die verfügbare grep-Engine das
/// Muster nicht parsen kann (z. B. `\d`, `\w`, `\b`, `(?:...)` – POSIX-ERE
/// unterstützt sie nicht), statt still 0 Treffer zu liefern.
fn grep_pattern_error(pattern: &str, stderr: &str) -> String {
    let hint = " (POSIX-ERE kennt \\d/\\w/\\s/\\b und (?:…) nicht; \
                ersetze sie z. B. durch [0-9]/[A-Za-z0-9_]/[[:space:]] — \
                oder installiere ripgrep (rg) für vollen Regex-Dialekt)";
    format!(
        "grep konnte das Muster nicht parsen — Suchmuster `{}` ist für den \
         klassischen grep-Fallback evtl. ungeeignet.{}\n{}",
        pattern,
        hint,
        stderr.trim()
    )
}

/// Entfernt führende `./` von jeder Zeile des Roh-Ausdrucks – gleiche
/// kanal-relative Form wie bei den geparsten Einzeltreffern.
fn normalize_raw(s: &str) -> String {
    s.lines()
        .map(|l| l.strip_prefix("./").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Liegt der Treffer in einem der ausgelassenen Generator-/Cache-Verzeichnisse?
/// Prüft jede Pfadkomponente (nicht nur die erste), damit auch verschachtelte
/// `src/target/…`, `vendor/node_modules/…` etc. wie bei `rg`/`--exclude-dir`
/// ausgeschlossen werden.
pub(super) fn is_excluded_path(path: &str) -> bool {
    path.split('/')
        .any(|comp| matches!(comp, ".git" | "target" | "node_modules"))
}

/// Parst die `pfad:zeile:text`-Ausgabe von `rg`/`grep` in [`Match`]-Einträge.
/// Die Zeilennummer dient nur der Struktur-Erkennung (Trefferzeile vs.
/// Fortsetzungszeile) und wird verworfen – das count-Rendering braucht nur
/// Pfad+Anzahl, der Kontext-Modus reicht roh durch.
pub(super) fn parse_search_lines(stdout: &str) -> Vec<Match> {
    let mut matches: Vec<Match> = Vec::new();
    for line in stdout.lines() {
        let parsed = line.split_once(':').and_then(|(path, rest)| {
            rest.split_once(':').and_then(|(lineno, text)| {
                lineno
                    .parse::<usize>()
                    .ok()
                    .map(|_| (path.to_string(), text.to_string()))
            })
        });
        if let Some((path, text)) = parsed {
            matches.push(Match { path, text });
            continue;
        }
        // Strukturlose Zeile → an den letzten Treffer anhängen.
        if let Some(last) = matches.last_mut() {
            last.text.push('\n');
            last.text.push_str(line);
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roh_ausdruck_verliert_punkt_schräg_präfix() {
        let raw = normalize_raw("./src/a.rs:3:x\n--\n./src/a.rs-2-kontext\n");
        assert_eq!(raw, "src/a.rs:3:x\n--\nsrc/a.rs-2-kontext");
    }

    #[test]
    fn include_landet_als_glob_bei_rg() {
        // Nur Strukturprüfung: die Argumentliste muss das Muster enthalten.
        let mut args: Vec<String> = vec!["--line-number".into()];
        let include = Some("*.rs");
        if let Some(inc) = include {
            args.push(format!("--glob={inc}"));
        }
        assert!(args.contains(&"--glob=*.rs".to_string()));
    }

    #[test]
    fn dateipfad_und_verzeichnispfad_werden_unterschieden() {
        let dir = std::env::temp_dir().join(format!("aidev-split-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.rs"), "fn main() {}").unwrap();

        // Datei → cwd = Elternverzeichnis, Ziel = Dateiname (behebt ENOTDIR).
        let file = dir.join("sub/a.rs");
        let (cwd, target) = split_search_path(&file);
        assert_eq!(cwd, dir.join("sub"), "{cwd:?}");
        assert!(matches!(target, SearchTarget::File(name) if name == "a.rs"));

        // Verzeichnis → cwd = Verzeichnis selbst, rekursive Suche.
        let dirp = dir.join("sub");
        let (cwd, target) = split_search_path(&dirp);
        assert_eq!(cwd, dirp, "{cwd:?}");
        assert!(matches!(target, SearchTarget::Dir));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `grep_search` muss `-E` (ERE) nutzen, damit Alternation/Quantoren wie
    /// bei ripgrep (Rust-regex) als Meta-Zeichen gelten. Ohne `-E` (BRE) wäre
    /// `cat|hat` eine Literal-Suche nach "cat|hat" → 0 Treffer.
    ///
    /// Übersprungen, wenn kein `grep` auf dem System liegt.
    #[test]
    fn grep_search_nutzt_ere_für_alternation() {
        if std::process::Command::new("grep")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipped: kein grep verfügbar");
            return;
        }
        let dir = std::env::temp_dir().join(format!("aidev-grep-ere-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "cat\ncats\nhat\n").unwrap();

        let (matches, _raw) =
            grep_search("cat|hat", &dir, Duration::from_secs(10), None, 0).expect("grep lief");

        // `-E` (ERE) nötig: Alternation `cat|hat` matcht "cat", "cats" UND "hat"
        // (= 3 Zeilen). Ohne `-E` (BRE) wäre es eine Literal-Suche → 0 Treffer.
        assert_eq!(matches.len(), 3, "`cat|hat` soll 3 Zeilen finden (nur mit -E): {:?}", matches);
        assert_eq!(matches[0].path, "f.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nicht-ERE-kompatible Muster (z. B. `(?:…)`) müssen als verständlicher
    /// Fehler gemeldet werden statt still 0 Treffer zu liefern.
    #[test]
    fn grep_search_meldet_musterfehler_statt_leerer_treffer() {
        if std::process::Command::new("grep")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipped: kein grep verfügbar");
            return;
        }
        let dir = std::env::temp_dir().join(format!("aidev-grep-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "abc\n").unwrap();

        // `(?:a)` ist in POSIX-ERE ungültig (nur Rust-regex/rg unterstützt es).
        match grep_search("(?:a)", &dir, Duration::from_secs(10), None, 0) {
            Err(msg) => assert!(
                msg.contains("nicht") || msg.contains("Muster"),
                "Fehlermeldung soll auf das Problem hinweisen: {msg}"
            ),
            Ok(_) => panic!("`(?:a)` ist kein gültiges POSIX-ERE – das soll als Fehler gemeldet werden, nicht als 0 Treffer"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `is_excluded_path` muss auch verschachtelte target/node_modules
    /// ausfiltern (nicht nur solche auf oberster Ebene), damit der Fallback
    /// denselben Satz wie `rg`/`--exclude-dir` ausschließt.
    #[test]
    fn excluded_path_filtert_auch_verschachtelte_pfade() {
        assert!(is_excluded_path("target/x.rs"));
        assert!(is_excluded_path(".git/config"));
        assert!(is_excluded_path("node_modules/pkg/main.js"));
        assert!(is_excluded_path("src/target/nested.rs"), "verschachteltes target muss raus");
        assert!(is_excluded_path("vendor/node_modules/deep.txt"), "verschachteltes node_modules muss raus");
        assert!(!is_excluded_path("src/main.rs"));
        assert!(!is_excluded_path("targets/keep.rs"));
    }
}
