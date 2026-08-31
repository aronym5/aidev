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
    // wiederholen wir mit rein portablen Optionen – die Verzeichnis-
    // Ausschlüsse übernimmt dann der Pfadfilter unten, include/Kontext
    // entfallen in dem Notfall-Pfad.
    // Bei einer gezielten Dateisuche (Einzeldatei als `path`) entfällt die
    // Rekursion -r und der Ziel-Pfad ist der Dateiname (cwd = Elternverzeichnis).
    let mut gnu: Vec<String> = if file_target {
        vec!["-n".into(), "-I".into()]
    } else {
        vec![
            "-r".into(),
            "-n".into(),
            "-I".into(),
            "--exclude-dir=.git".into(),
            "--exclude-dir=target".into(),
            "--exclude-dir=node_modules".into(),
        ]
    };
    if let Some(inc) = include {
        gnu.push(format!("--include={inc}"));
    }
    if context_lines > 0 {
        gnu.push("-C".into());
        gnu.push(context_lines.to_string());
    }
    gnu.push("--".into());
    gnu.push(pattern.to_string());
    gnu.push(match &target {
        SearchTarget::File(name) => name.clone(),
        SearchTarget::Dir => ".".into(),
    });

    let mut out = run_with_timeout("grep", &gnu, &cwd, timeout)
        .map_err(|e| format!("grep not available: {e}"))?;
    if out.exit_code != Some(0) && out.exit_code != Some(1) {
        // Portabler Retry: -I/--include/--exclude-dir fliegen raus (GNU-only),
        // der Kontext (-C) bleibt – BusyBox- und BSD-grep beherrschen ihn.
        let mut portable: Vec<String> = if file_target {
            vec!["-n".into()]
        } else {
            vec!["-r".into(), "-n".into()]
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
    if out.exit_code == Some(1) {
        return Ok((Vec::new(), None)); // keine Treffer
    }
    if out.exit_code != Some(0) {
        return Err(format!("grep error:\n{}", out.stderr));
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
    // Ohne --exclude-dir (portabler Pfad) können .git/target/node_modules
    // auftauchen – nachträglich ausfiltern (bei GNU redundant, schadet nicht).
    matches.retain(|m| !is_excluded_path(&m.path));
    Ok((matches, None))
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
pub(super) fn is_excluded_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    matches!(first, ".git" | "target" | "node_modules")
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
}
