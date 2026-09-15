//! Gitignore-ähnliche Filterregeln für den `grep`-Fallback.
//!
//! Wenn ripgrep (`rg`) verfügbar ist, übernimmt dessen `ignore`-Crate die
//! korrekte Ausfilterung ignorierteter Pfade. Für den klassischen `grep`-
//! Fallback, der keine Kenntnis von `.gitignore` hat, liest dieses Modul
//! die `ignore`-Dateien aus dem Such-Verzeichnis und wendet sie als
//! Nachfilter auf Treffer an.
//!
//! Die implementierte Semantik ist eine **pragmatische Annäherung**:
//!
//! * **Versteckte Pfade** (jeder Pfad-Component mit führendem `.`) werden
//!   generell ausgeschlossen – das spiegelt das Standardverhalten von `rg`
//!   wider (`--no-hidden`).
//! * **Regeln aus Dateien** (`.gitignore`, `.ignore`, `.rgignore`) werden
//!   in Reihenfolge der Datei ausgewertet; die **letzte zutreffende
//!   Regel gewinnt**.
//! * **Negation** (`!`) wird unterstützt.
//! * **Glob-Elemente** (`*`, `?`, `**`) werden für datei- und
//!   komponentenbasiertes Matching unterstützt.
//! * **Anker** (Pattern enthält `/`, nicht nur am Ende) -Regeln
//!   werden gegen den gesamten Pfad geprüft.
//!
//! Für volle Gitignore-Semantik (z. B. `.git/info/exclude`,
//! globales Exclude, spezielle `.gitignore`-Logik in Unterordnern)
//! ist `rg` die bevorzugte Wahl; dieses Modul dient ausschließlich
//! alsróbster, aber brauchbarer Fallback.

use std::path::Path;

// ────────────────────────────────────────────────────────────────
// Strukturen
// ────────────────────────────────────────────────────────────────

/// Ein einzelner Ignore-Eintrag, parsed aus einer `.gitignore`-Datei.
#[derive(Debug, Clone)]
struct Rule {
    /// Das Pattern (bereinigt um `!`, `/`-Suffix, Whitespace)
    pattern: String,
    /// Negation: Pfad wird wieder aufgenommen
    negated: bool,
    /// Pattern enthält `/` (nicht nur am Ende) → angewandt auf den ganzen Pfad
    anchored: bool,
}

/// Dynamischer Ignore-Filter, aufgebaut aus `.gitignore` / `.ignore` /
/// `.rgignore`-Dateien im Arbeitsverzeichnis und seinen Eltern.
pub(crate) struct IgnoreFilter {
    rules: Vec<Rule>,
}

// ────────────────────────────────────────────────────────────────
// öffentliche Schnittstelle
// ────────────────────────────────────────────────────────────────

impl IgnoreFilter {
    /// Erzeugt einen Filter, indem ab `start` rekursiv aufwärts bis
    /// (einschließlich) `stop_at` nach Ignore-Dateien gesucht wird.
    /// Dateien, die näher am Suchverzeichnis liegen, haben Vorrang.
    pub(crate) fn load(start: &Path, stop_at: &Path) -> Self {
        let mut rules = Vec::new();
        let mut dir = Some(start.to_path_buf());

        while let Some(current) = dir {
            for name in &[".gitignore", ".ignore", ".rgignore"] {
                let file = current.join(name);
                if file.is_file() {
                    if let Ok(content) = std::fs::read_to_string(&file) {
                        rules.extend(parse_ignore_content(&content));
                    }
                }
            }
            if current == stop_at {
                break;
            }
            dir = current.parent().map(|p| p.to_path_buf());
        }

        Self { rules }
    }

    /// Prüft, ob `path` (relativ zur Filter-Wurzel) ausgeschlossen werden soll.
    ///
    /// Zuerst wird die **Hidden-Regel** geprüft (jeder Pfad-Component mit
    /// führendem `.` → ausgeschlossen, analog zu `rg` Standard). Danach
    /// werden die Datei-Regeln in Reihenfolge ausgewertet; die letzte
    /// Regel, die matcht, bestimmt das Ergebnis.
    pub(crate) fn is_ignored(&self, path: &str) -> bool {
        // 1) Hidden-Regel: rg schließt alle versteckten Pfade (.) standardmäßig aus
        if has_hidden_component(path) {
            return true;
        }

        // 2) Gitignore-Regeln: letzter Treffer gewinnt
        let mut ignored = false;
        for rule in &self.rules {
            let matches = if rule.anchored {
                path_matches_pattern(&rule.pattern, path)
            } else {
                // Unanchored: prüfe jede Pfad-Komponente einzeln
                path.split('/')
                    .any(|comp| component_matches_pattern(&rule.pattern, comp))
            };
            if matches {
                ignored = !rule.negated;
            }
        }
        ignored
    }

    /// Anzahl der geladenen Regeln (für Tests/Diagnose).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.rules.len()
    }
}

// ────────────────────────────────────────────────────────────────
// Parsing
// ────────────────────────────────────────────────────────────────

/// Parst den Inhalt einer Ignore-Datei in eine Liste von Regeln.
fn parse_ignore_content(content: &str) -> Vec<Rule> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (negated, rest) = match line.strip_prefix('!') {
                Some(r) => (true, r),
                None => (false, line),
            };
            let dir_only = rest.ends_with('/');
            let pattern = if dir_only {
                &rest[..rest.len() - 1]
            } else {
                rest
            };
            if pattern.is_empty() {
                return None;
            }
            // Anker-Semantik (Gitignore v2):
            // - führendes '/' → explizit an die Wurzel gebunden
            // - '/' an beliebiger anderer Stelle → relativ zur Wurzel (geankert)
            // - führendes '**/' → matcht in beliebiger Tiefe (nicht geankert)
            let mut anchored = pattern.contains('/');
            let mut pattern = pattern;
            if let Some(r) = pattern.strip_prefix('/') {
                pattern = r;
                anchored = true;
            }
            if let Some(r) = pattern.strip_prefix("**/") {
                pattern = r;
                anchored = pattern.contains('/'); // * */src/foo → wieder relativ
            }
            if pattern.is_empty() {
                return None;
            }
            Some(Rule {
                pattern: pattern.to_string(),
                negated,
                anchored,
            })
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────
// Matching
// ────────────────────────────────────────────────────────────────

/// Prüft, ob `path` einen versteckten Pfad-Component enthält
/// (jeder Abschnitt mit führendem `.` schließt aus, inkl. `.git`).
fn has_hidden_component(path: &str) -> bool {
    path.split('/')
        .any(|c| c.starts_with('.') && c != "." && c != "..")
}

/// Anker-Pattern: matcht als **Pfad-Präfix** auf Komponenten-Ebene gegen den
/// Pfad. Ein Treffer auf einer Komponente ignoriert auch alle Nachkommen
/// (Gitignore-Semantik: ignoriert man ein Verzeichnis, sind dessen Inhalte weg).
fn path_matches_pattern(pattern: &str, path: &str) -> bool {
    let pat_comps: Vec<&str> = pattern.split('/').collect();
    let path_comps: Vec<&str> = path.split('/').collect();
    if pat_comps.len() > path_comps.len() {
        return false;
    }
    pat_comps
        .iter()
        .enumerate()
        .all(|(i, comp)| component_matches_pattern(comp, path_comps[i]))
}

/// Unanchored-Pattern: matcht gegen eine einzelne Pfad-Komponente.
fn component_matches_pattern(pattern: &str, component: &str) -> bool {
    glob_match(pattern, component)
}

// ────────────────────────────────────────────────────────────────
// Glob-Engine (Minimal: *, ?, **, Literal)
// ────────────────────────────────────────────────────────────────

/// Minimales Glob-Matching: `*` ( beliebige Zeichen außer `/` ),
/// `?` ( ein Zeichen außer `/` ), `**` ( beliebig inkl. `/` ),
/// Literal, `\`-Escape.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_inner(pattern.as_bytes(), 0, text.as_bytes(), 0)
}

fn glob_inner(p: &[u8], mut pi: usize, t: &[u8], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            b'*' if pi + 1 < p.len() && p[pi + 1] == b'*' => {
                // ** – Minimum ein /-Separator (.sonst gleicher Effekt wie *)
                let mut next = pi + 2;
                // Mehrere **-Sternpaare komprimieren
                while next + 1 < p.len() && p[next] == b'*' && p[next + 1] == b'*' {
                    next += 2;
                }
                // **/ am Anfang oder nach / → nullem oder mehreren Verzeichnissen
                if next < p.len() && p[next] == b'/' {
                    next += 1;
                    // Null Verzeichnisse: direkt danach matchen
                    if glob_inner(p, next, t, ti) {
                        return true;
                    }
                    // Mindestens ein Verzeichnis: nächstes / suchen
                    let mut i = ti;
                    while i < t.len() {
                        if t[i] == b'/' && glob_inner(p, next, t, i + 1) {
                            return true;
                        }
                        i += 1;
                    }
                    return false;
                }
                // ** (nicht vor /) → beliebig langer Rest
                for i in ti..=t.len() {
                    if glob_inner(p, next, t, i) {
                        return true;
                    }
                }
                return false;
            }
            b'*' => {
                // * → beliebig, aber keinen / überschreiten
                for i in ti..=t.len() {
                    if i > ti && t[i - 1] == b'/' {
                        break;
                    }
                    if glob_inner(p, pi + 1, t, i) {
                        return true;
                    }
                }
                return false;
            }
            b'?' if ti < t.len() && t[ti] != b'/' => {
                pi += 1;
                ti += 1;
            }
            b'\\' if pi + 1 < p.len() && ti < t.len() => {
                pi += 1;
                if p[pi] == t[ti] {
                    pi += 1;
                    ti += 1;
                } else {
                    return false;
                }
            }
            c if ti < t.len() && t[ti] == c => {
                pi += 1;
                ti += 1;
            }
            _ => return false,
        }
    }
    ti == t.len()
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── has_hidden_component ────────────────────────────────────

    #[test]
    fn hidden_erkennung() {
        assert!(has_hidden_component(".git/config"));
        assert!(has_hidden_component(".gitignore")); // Root-Component
        assert!(has_hidden_component("src/.hidden/file"));
        assert!(!has_hidden_component("src/main.rs"));
        assert!(!has_hidden_component("target/debug"));
        assert!(!has_hidden_component(".")); // Eigenes Verzeichnis
        assert!(!has_hidden_component("..")); // Elternverzeichnis
    }

    // ── parse_ignore_content ────────────────────────────────────

    #[test]
    fn parses_standard_gitignore() {
        let content = r#"
# Kommentar
target
build/

!build/dist

/src/.cache
**/temp
"#;
        let rules = parse_ignore_content(content);
        assert_eq!(rules.len(), 5, "rules={rules:?}");
        assert!(!rules[0].negated && !rules[0].anchored && rules[0].pattern == "target");
        assert!(!rules[1].negated && !rules[1].anchored && rules[1].pattern == "build");
        // `build/dist` enthält '/' → relativ zur Wurzel geankert
        assert!(rules[2].negated && rules[2].anchored && rules[2].pattern == "build/dist");
        assert!(!rules[3].negated && rules[3].anchored && rules[3].pattern == "src/.cache");
        assert!(!rules[4].negated && !rules[4].anchored && rules[4].pattern == "temp");
    }

    #[test]
    fn ignores_blank_and_comment_lines() {
        let content = "  \n# foo\nbar\n";
        let rules = parse_ignore_content(content);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern, "bar");
    }

    // ── Glob-Matching ───────────────────────────────────────────

    #[test]
    fn glob_literal() {
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "bar"));
        assert!(!glob_match("foo", "foo/"));
    }

    #[test]
    fn glob_star() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "main.js"));
        assert!(!glob_match("*.rs", "dir/main.rs")); // * kreuzt /
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "src/deep/main.rs")); // * kreuzt /
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("f?.rs", "fo.rs"));
        assert!(!glob_match("f?.rs", "foo.rs"));
        assert!(!glob_match("f?", "f/")); // ? kreuzt /
    }

    #[test]
    fn glob_double_star() {
        assert!(glob_match("**/target", "target"));
        assert!(glob_match("**/target", "src/target"));
        assert!(glob_match("**/target", "a/b/c/target"));
        assert!(!glob_match("**/target", "targets"));
        assert!(glob_match("src/**", "src/"));
        assert!(glob_match("src/**", "src/main.rs"));
        assert!(glob_match("src/**", "src/deep/nested.rs"));
        assert!(!glob_match("src/**", "other/file"));
        assert!(glob_match("**", "any/path"));
    }

    #[test]
    fn glob_escape() {
        assert!(glob_match("a\\*b", "a*b"));
        assert!(!glob_match("a\\*b", "aXb"));
    }

    // ── is_ignored (integriert) ────────────────────────────────

    #[test]
    fn ignore_filter_ohne_datei_falls_hidden() {
        let filter = IgnoreFilter { rules: Vec::new() };
        assert!(filter.is_ignored(".git/config"));
        assert!(filter.is_ignored("src/.cache/data"));
        assert!(!filter.is_ignored("src/main.rs"));
        assert!(!filter.is_ignored("target/debug"));
    }

    #[test]
    fn ignore_filter_mit_gitignore_regeln() {
        // Negation re-include: Für Dateien unter einem re-inkludierten Verzeichnis
        // braucht es `!dir/**` (Gitignore-Semantik; entspricht rg/ignore-Crate).
        let filter = IgnoreFilter {
            rules: parse_ignore_content("target\nbuild/\n!build/dist/**"),
        };
        assert!(filter.is_ignored("target/debug/aidev"));
        assert!(filter.is_ignored("build/output"));
        assert!(
            !filter.is_ignored("build/dist/release"),
            "Negation re-include"
        );
        assert!(
            !filter.is_ignored("build/dist/deep/x"),
            "Negation deckt Tiefe ab"
        );
        assert!(!filter.is_ignored("src/main.rs"));
    }

    #[test]
    fn ignore_filter_anchored_pattern() {
        let filter = IgnoreFilter {
            rules: parse_ignore_content("/target"),
        };
        assert!(filter.is_ignored("target/debug"));
        assert!(!filter.is_ignored("src/target/nested.rs"), "Anchored");
    }

    #[test]
    fn ignore_filter_negation_letzter_schlag() {
        let content = "*\n!src/\n*.md";
        let filter = IgnoreFilter {
            rules: parse_ignore_content(content),
        };
        // Zuerst alles ignoriert (*), dann src/ wieder aufgenommen, dann *.md wieder ignoriert
        assert!(filter.is_ignored("target/debug"));
        assert!(!filter.is_ignored("src/main.rs"), "Negation");
        assert!(filter.is_ignored("README.md"), "Letzte Regel gewinnt");
    }

    // ── load (Dateisystem-Integration) ──────────────────────────

    #[test]
    fn load_liest_dateien_aufsteigend() {
        let base = std::env::temp_dir().join(format!(
            "aidev-ignore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let sub = base.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join(".gitignore"), "target\n").unwrap();
        std::fs::write(sub.join(".gitignore"), "*.log\n").unwrap();

        let filter = IgnoreFilter::load(&sub, &base);
        assert_eq!(filter.len(), 2, "2 Dateien, je 1 Regel");
        assert!(filter.is_ignored("target/debug"));
        assert!(filter.is_ignored("deep/app.log"));
        assert!(!filter.is_ignored("main.rs"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn load_stopp_am_stop_at() {
        let base = std::env::temp_dir().join(format!("aidev-ignore-stop-{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join(".gitignore"), "foo\n").unwrap();
        std::fs::write(sub.join(".gitignore"), "bar\n").unwrap();

        let filter = IgnoreFilter::load(&sub, &sub); // Stop in sub → .gitignore unten
        assert_eq!(filter.len(), 1); // nur bar
        assert!(filter.is_ignored("bar/file"));
        assert!(!filter.is_ignored("foo/file"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
