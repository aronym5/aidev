//! Datei-Pattern-Suche über `find`: das Modell-Werkzeug `glob`.
//!
//! Wie die Volltextsuche läuft `find` als Host-Prozess im (bereits gegen
//! die Kanal-Wurzel geprüften) Verzeichnis – konsistent zu `rg`/`grep`
//! in [`super::search`]. Symlinks werden nicht gefolgt (`find`-Default),
//! Generator-/Cache-Verzeichnisse werden nachträglich ausgefiltert.

use std::path::Path;
use std::time::Duration;

use super::run::run_with_timeout;
use super::search::is_excluded_path;

/// Liefert alle Dateien und Verzeichnisse unter `dir`, die auf das
/// Glob-Muster passen.
///
/// Muster ohne `/` gelten als Dateiname/Verzeichnisname (`find -name`, jede
/// Tiefe), Muster mit `/` als relativer Pfad (`find -path`; `*` überschreitet
/// dabei `/`). Die Ergebnisse sind kanal-relativ (ohne `./`-Präfix),
/// dedupliziert und alphabetisch sortiert.
pub(super) fn find_glob(
    pattern: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("Muster ist leer.".to_string());
    }
    // Doppelstern-Normalisierung für fnmatch-Semantik (finds „*“ überschreitet
    // bereits „/“): „a/**/b“ muss auch „a/b“ treffen – „/**/“ wird deshalb zu
    // „/*“, ein führendes „**/“ entfällt ganz. Ohne diese Schritt wäre
    // „src/**/*.rs“ strikt an ZWEI Schrägstriche gebunden und verlöre
    // „src/main.rs“.
    let mut pat = pattern.to_string();
    while let Some(pos) = pat.find("/**/") {
        pat.replace_range(pos..pos + 4, "/*");
    }
    if let Some(rest) = pat.strip_prefix("**/") {
        pat = rest.to_string();
    }
    if pat == "**" {
        pat = "*".to_string();
    }
    // Muster mit `/` auf den ganzen (relativen) Pfad anwenden – find sieht
    // Pfade mit führendem `./`, daher ergänzen wir das Präfix. Ohne `/`
    // dagegen nur auf den Dateinamen (-name), sonst träfe `*.rs` nie, weil
    // der Pfad ja mit `./` beginnt.
    let (flag, pat) = if pattern.contains('/') {
        ("-path", format!("./{pat}"))
    } else {
        ("-name", pat)
    };
    // Ohne `-type` liefert find Dateien UND Verzeichnisse (außer dem
    // Startpunkt selbst). Das ist gewollt: `glob` soll beides finden.
    let args: Vec<String> = [".", flag, &pat].iter().map(|s| s.to_string()).collect();

    let out = run_with_timeout("find", &args, dir, timeout)
        .map_err(|e| format!("find not available: {e}"))?;
    if out.exit_code != Some(0) {
        return Err(format!("find error:\n{}", out.stderr));
    }

    // Kanal-relative Form („./src/main.rs“ → „src/main.rs“), Generator- und
    // Cache-Verzeichnisse raus, Duplikate vermeiden, stabil sortieren. Die
    // Ergebnismenge begrenzt der Aufrufer (tools_exec) auf GLOB_RESULT_CAP.
    // Verzeichnisse werden mit abschließendem „/“ markiert (z. B. „src/“), damit
    // das Modell Dateien von Verzeichnissen unterscheiden kann.
    let mut paths: Vec<String> = out
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("./"))
        .filter(|p| !is_excluded_path(p))
        .map(|p| {
            if dir.join(p).is_dir() {
                format!("{p}/")
            } else {
                p.to_string()
            }
        })
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_tree(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "aidev-glob-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(base.join("src/deep")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("target/debug")).unwrap();
        std::fs::write(base.join("README.md"), "").unwrap();
        std::fs::write(base.join("src/main.rs"), "").unwrap();
        std::fs::write(base.join("src/deep/nested.rs"), "").unwrap();
        std::fs::write(base.join(".git/config"), "").unwrap();
        std::fs::write(base.join("target/debug/x.rs"), "").unwrap();
        base
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn name_ohne_slash_findet_jede_tiefe() {
        let dir = temp_tree("name");
        let hits = find_glob("*.rs", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits, vec!["src/deep/nested.rs", "src/main.rs"]);
        assert!(!hits.iter().any(|p| p.starts_with("target")), "Excludes");
        cleanup(&dir);
    }

    #[test]
    fn pfad_mit_slatch_matcht_relativ() {
        let dir = temp_tree("path");
        let hits = find_glob("src/**/*.rs", &dir, Duration::from_secs(10)).unwrap();
        assert!(hits.contains(&"src/main.rs".to_string()), "{hits:?}");
        assert!(hits.contains(&"src/deep/nested.rs".to_string()), "{hits:?}");
        let hits = find_glob("*.md", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits, vec!["README.md"]);
        cleanup(&dir);
    }

    #[test]
    fn findet_auch_verzeichnisse() {
        let dir = temp_tree("dir");
        let hits = find_glob("src", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits, vec!["src/"], "{hits:?}");
        let hits = find_glob("deep", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits, vec!["src/deep/"], "{hits:?}");
        // Dateien bekommen KEIN Schrägstrich.
        let hits = find_glob("README.md", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits, vec!["README.md"]);
        cleanup(&dir);
    }

    #[test]
    fn leeres_muster_wird_abgewiesen() {
        let dir = temp_tree("empty");
        assert!(find_glob("  ", &dir, Duration::from_secs(5)).is_err());
        cleanup(&dir);
    }

    #[test]
    fn ergebnisse_bleiben_an_der_decke_mit_hinweis_moeglich() {
        // Die Decke greift nicht hier im Renderer, sondern in tools_exec;
        // der Test stellt nur sicher, dass find selbst viele Treffer liefert.
        let dir = temp_tree("cap");
        for i in 0..5 {
            std::fs::write(dir.join(format!("src/f{i}.txt")), "").unwrap();
        }
        let hits = find_glob("*.txt", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(hits.len(), 5);
        cleanup(&dir);
    }
}
