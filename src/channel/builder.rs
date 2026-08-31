//! Channel Builder – Helfer für den dreispaltigen Kanal-Erzeugungs-Dialog.
//!
//! Liefert die Rohdaten für die drei Spalten:
//! * **Tunnel**: `local` + benannte Podman-Images
//! * **Host/Repo**: Config-[path]-Einträge + cwd + argv-Pfad
//! * **Worktrees**: Git-Worktrees + alle lokalen Branches

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::run::run_with_timeout;

// ---------------------------------------------------------------------------
// Tunnel (linke Spalte)
// ---------------------------------------------------------------------------

/// Tunnel-Typ: lokale Ausführung oder Podman-Container mit einem bestimmten Image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tunnel {
    Local,
    Image {
        name: String,
        /// Image-eigenes WorkingDir (aus `podman inspect`), `None` wenn noch nicht ermittelt.
        working_dir: Option<String>,
    },
}

impl Tunnel {
    /// Anzeige-Name in der linken Spalte.
    /// `docker.io/library/node:22` → `node`, `rust:latest` → `rust`
    pub fn label(&self) -> String {
        match self {
            Tunnel::Local => "local".to_string(),
            Tunnel::Image { name, .. } => image_short_name(name).to_string(),
        }
    }

    /// Voller Image-Name (nur bei `Image`, sonst `None`).
    pub fn image_name(&self) -> Option<&str> {
        match self {
            Tunnel::Local => None,
            Tunnel::Image { name, .. } => Some(name),
        }
    }
}

/// Kürzester Anzeige-Name eines Images: letztes Pfad-Segment ohne Tag.
/// `docker.io/library/node:22` → `node`, `rust:latest` → `rust`
pub fn image_short_name(name: &str) -> &str {
    let bare = name.rsplit('/').next().unwrap_or(name);
    match bare.find(':') {
        Some(pos) => &bare[..pos],
        None => bare,
    }
}

/// Vereinheitlicht eine Image-Referenz für Vergleiche:
/// * `docker.io/library/…`-, `docker.io/…`- und `localhost/`-Prefix entfernen
/// * `:latest`-Suffix entfernen
///
/// `docker.io/library/alpine:latest` → `alpine`, `node:22` bleibt `node:22`,
/// `ghcr.io/org/repo:latest` → `ghcr.io/org/repo`.
pub fn normalize_image_name(name: &str) -> String {
    let s = name.trim();
    let s = s.strip_prefix("docker.io/library/").unwrap_or(s);
    let s = s.strip_prefix("docker.io/").unwrap_or(s);
    let s = s.strip_prefix("localhost/").unwrap_or(s);
    let s = s.strip_suffix(":latest").unwrap_or(s);
    s.to_string()
}

/// Wahr, wenn zwei Image-Referenzen dasselbe Image bezeichnen
/// (Vergleich über [`normalize_image_name`]).
/// `alpine` == `docker.io/library/alpine:latest`, aber `alpine` != `alpine:3.20`.
pub fn image_names_equal(a: &str, b: &str) -> bool {
    normalize_image_name(a) == normalize_image_name(b)
}

/// Fragt `podman images` ab und liefert eine sortierte Liste benannter Images
/// in normalisierter Form (ohne `<none>`, ohne `docker.io[/library]/`- und
/// `localhost/`-Prefix, ohne `:latest`-Suffix).
/// Bei jedem Fehler wird eine leere Liste zurückgegeben.
pub fn podman_list_images() -> Vec<String> {
    let out = run_with_timeout(
        "podman",
        &[
            "images".into(),
            "--format".into(),
            "{{.Repository}}:{{.Tag}}".into(),
        ],
        Path::new("."),
        Duration::from_secs(5),
    );
    let stdout = match out {
        Ok(o) if o.exit_code == Some(0) => o.stdout,
        _ => return Vec::new(),
    };
    let mut images: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with("<none>"))
        .map(|l| normalize_image_name(&l))
        .filter(|l| !l.is_empty() && l != "<none>")
        .collect();
    images.sort();
    images.dedup();
    images
}

/// Prüft ob `podman` grundlegend verfügbar ist (nicht ob es läuft).
pub fn podman_available() -> bool {
    run_with_timeout(
        "podman",
        &["info".into()],
        Path::new("."),
        Duration::from_secs(5),
    )
    .is_ok_and(|o| o.exit_code == Some(0))
}

/// Prüft ob ein Podman-Image `alpine` existiert.
pub fn podman_has_alpine() -> bool {
    let out = run_with_timeout(
        "podman",
        &[
            "images".into(),
            "--format".into(),
            "{{.Repository}}:{{.Tag}}".into(),
            "alpine".into(),
        ],
        Path::new("."),
        Duration::from_secs(5),
    );
    out.is_ok_and(|o| {
        o.exit_code == Some(0)
            && o.stdout.lines().any(|l| {
                let t = l.trim();
                !t.contains("<none>") && image_short_name(t) == "alpine"
            })
    })
}

/// Liest das WorkingDir eines Podman-Images aus (`podman inspect`).
/// Gibt `None` zurück, wenn der Image nicht vorhanden ist oder der Aufruf fehlschlägt.
pub fn podman_inspect_working_dir(image: &str) -> Option<String> {
    let out = run_with_timeout(
        "podman",
        &[
            "inspect".into(),
            "--format".into(),
            "{{.Config.WorkingDir}}".into(),
            image.to_string(),
        ],
        Path::new("."),
        Duration::from_secs(5),
    );
    out.ok()
        .filter(|o| o.exit_code == Some(0))
        .map(|o| o.stdout.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Container-Erkennung (Statuszeile im Dialog)
// ---------------------------------------------------------------------------

/// Kurz-Info über einen gefundenen Container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Container-Name
    pub name: String,
    /// Status: "läuft", "gestoppt", "als Kanal aktiv"
    pub status: String,
    /// Tatsächlicher Mount-Destination-Pfad (z.B. "/usr/src/app")
    pub mount_destination: Option<String>,
}

/// Sucht einen laufenden oder gestoppten Container, dessen Mount auf `mount_path`
/// zeigt und (falls angegeben) dessen Image übereinstimmt.
pub fn find_container_for(mount_path: &Path, image: Option<&str>) -> Option<ContainerInfo> {
    let path_str = mount_path.to_string_lossy();
    // Alle Container (laufend + gestoppt) auflisten
    let out = run_with_timeout(
        "podman",
        &[
            "ps".into(),
            "-a".into(),
            "--format".into(),
            "{{.Names}}|{{.Status}}".into(),
        ],
        Path::new("."),
        Duration::from_secs(5),
    );
    let stdout = match out {
        Ok(o) if o.exit_code == Some(0) => o.stdout,
        _ => return None,
    };
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, '|').collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts[0].trim();
        let status_line = parts[1].trim();
        // Mount- UND Image-Info des Containers prüfen (ein einziger Inspect)
        let inspect = run_with_timeout(
            "podman",
            &[
                "inspect".into(),
                "--format".into(),
                "{{.Config.Image}}|{{range .Mounts}}{{.Source}}:{{.Destination}} {{end}}".into(),
                name.to_string(),
            ],
            Path::new("."),
            Duration::from_secs(5),
        );
        if let Ok(o) = inspect {
            if o.exit_code != Some(0) {
                continue;
            }
            let mut parts = o.stdout.splitn(2, '|');
            let container_image = parts.next().unwrap_or("").trim();
            let mounts = parts.next().unwrap_or("");

            // Mount-Pfad prüfen und Destination extrahieren
            let mut matched_destination: Option<String> = None;
            let mut mount_found = false;
            // Pfad normalisieren für Vergleich (trailing slash entfernen)
            let path_normalized = path_str.trim_end_matches('/');
            for mount_pair in mounts.split_whitespace() {
                if let Some((src, dst)) = mount_pair.split_once(':') {
                    let src_normalized = src.trim_end_matches('/');
                    if src_normalized == path_normalized {
                        mount_found = true;
                        matched_destination = Some(dst.to_string());
                        break;
                    }
                }
            }
            if !mount_found {
                continue;
            }
            // Image prüfen (falls angegeben)
            // Podman kann das Image-Referenzformat ändern (registry, :latest etc.)
            if let Some(expected_img) = image {
                if !image_names_equal(container_image, expected_img) {
                    continue;
                }
            }

            let status = if status_line.starts_with("Up") {
                "running"
            } else {
                "stopped"
            };
            return Some(ContainerInfo {
                name: name.to_string(),
                status: status.to_string(),
                mount_destination: matched_destination,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Namensbildung für Builder-erzeugte Run-Kanäle
// ---------------------------------------------------------------------------

/// Anzeigename in der Statusleiste: `"<imagename> <worktreepfad>"`
/// (z. B. `alpine /home/work/proj`). Imagename in Kurzform, Pfad
/// vollständig auf dem Host (Repo-Dir oder Worktree-Dir).
pub fn run_channel_name(image: &str, effective_root: &Path) -> String {
    format!("{} {}", image_short_name(image), effective_root.display())
}

/// Container-Name für Builder-erzeugte Run-Kanäle:
/// * Repo mit Branch: `aidev-<reponame>-<branchname>-<imagename>`
/// * ohne Repo/Branch: `aidev-<hostfoldername>-<imagename>`
///
/// Imagename in Kurzform (`alpine`), Repo-/Hostfolder als Ordnername;
/// das Ergebnis wird gesamt sanitisiert (lowercase,
/// Nicht-Alphanumerik → `-`).
pub fn run_container_name(repo_folder: &str, branch: Option<&str>, image: &str) -> String {
    let img = image_short_name(image);
    let raw = match branch {
        Some(b) => format!("aidev-{repo_folder}-{b}-{img}"),
        None => format!("aidev-{repo_folder}-{img}"),
    };
    super::run::sanitize(&raw)
}

// ---------------------------------------------------------------------------
// Host-Pfade (mittlere Spalte)
// ---------------------------------------------------------------------------

/// Ein Host/Repo-Pfad für den Channel Builder.
#[derive(Debug, Clone)]
pub struct HostPath {
    /// Absoluter Pfad
    pub path: PathBuf,
}

impl HostPath {
    pub fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// Sammelt die Host-Pfade: Config + cwd + argv, dedupliziert.
pub fn collect_host_paths(
    config_paths: &std::collections::HashMap<String, crate::config::PathEntry>,
    argv_path: Option<&str>,
) -> Vec<HostPath> {
    let mut paths: Vec<HostPath> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. Config-[path]-Einträge
    for p in config_paths.keys() {
        let pb = PathBuf::from(p);
        if seen.insert(pb.clone()) {
            paths.push(HostPath { path: pb });
        }
    }

    // 2. CWD
    if let Ok(cwd) = std::env::current_dir() {
        if seen.insert(cwd.clone()) {
            paths.push(HostPath { path: cwd });
        }
    }

    // 3. argv-Pfad
    if let Some(argv) = argv_path {
        let pb = PathBuf::from(argv);
        if seen.insert(pb.clone()) {
            paths.push(HostPath { path: pb });
        }
    }

    paths
}

// ---------------------------------------------------------------------------
// Worktrees + Branches (rechte Spalte)
// ---------------------------------------------------------------------------

/// Ein Eintrag in der Worktree/Branch-Liste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Anzeige: "/pfad/to/worktree (branch)" oder "branch (Kein Worktree)"
    pub label: String,
    /// Absoluter Pfad zum Worktree (leer bei Branch ohne Worktree)
    pub path: PathBuf,
    /// Branch-Name
    pub branch: String,
    /// Ob es sich um den Haupt-Worktree handelt (HEAD des Repos)
    pub is_main: bool,
    /// Hat dieser Branch ein Worktree?
    pub has_worktree: bool,
}

/// Hilfsfunktion: Pfad relativ zu einer Basis berechnen.
/// Die Basis ist der im Host/Repo-Mittelfeld gezeigte Pfad – nicht zwingend
/// der Git-toplevel. Funktioniert auch wenn der Pfad außerhalb der Basis
/// liegt (z.B. `../../andere`).
fn relative_to_base(path: &Path, base: &Path) -> String {
    // Zuerst strip_prefix versuchen (Worktree unterhalb der Basis)
    if let Ok(rel) = path.strip_prefix(base) {
        let s = rel.display().to_string();
        return if s.is_empty() { ".".to_string() } else { s };
    }
    // Außerhalb: gemeinsamen Vorfahren finden
    let path_components: Vec<_> = path.components().collect();
    let base_components: Vec<_> = base.components().collect();
    // Wie viele Komponenten stimmen von vorne überein?
    let common = path_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    // ".." für jeden verbleibenden Basis-Komponenten
    let ups = base_components.len() - common;
    let mut result: Vec<String> = vec!["..".to_string(); ups];
    // Rest des Ziel-Pfads
    for c in &path_components[common..] {
        result.push(c.as_os_str().to_string_lossy().into_owned());
    }
    result.join("/").replace("//", "/")
}

/// Liefert die Worktrees und lokalen Branches eines Git-Repos.
/// Worktrees werden zuerst angezeigt, dann die verbleibenden Branches (dezenter).
pub fn git_list_worktrees_and_branches(repo: &Path) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut seen_branches = std::collections::HashSet::new();

    // Worktrees via `git worktree list --porcelain`
    if let Ok(out) = crate::repo::git(repo, &["worktree", "list", "--porcelain"]) {
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;

        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch ") {
                current_branch = Some(b.trim().to_string());
            } else if line.is_empty() || line.starts_with("bare") {
                // Ende eines Eintrags
                if let (Some(path), Some(branch_ref)) = (&current_path, &current_branch) {
                    let branch = branch_ref
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch_ref)
                        .to_string();
                    // `is_main` markiert jenen Worktree, dessen Pfad mit dem im
                    // Mittelfeld gewählten Repo-Verzeichnis (`repo`) übereinstimmt –
                    // nicht zwingend das Git-Top-Level.
                    let is_main = repo == path.as_path();
                    // Relative Pfade beziehen sich auf den im Mittelfeld
                    // gezeigten Pfad (`repo`), nicht auf den Git-toplevel –
                    // sonst stimmen sie nicht mit der Host/Repo-Spalte überein,
                    // wenn diese auf ein Worktree zeigt.
                    let rel = relative_to_base(path, repo);
                    entries.push(WorktreeEntry {
                        label: format!("{} ({})", rel, branch),
                        path: path.clone(),
                        branch: branch.clone(),
                        is_main,
                        has_worktree: true,
                    });
                    seen_branches.insert(branch);
                }
                current_path = None;
                current_branch = None;
            }
        }
        // Letzter Eintrag (kein abschließendes Leerzeichen)
        if let (Some(path), Some(branch_ref)) = (&current_path, &current_branch) {
            let branch = branch_ref
                .strip_prefix("refs/heads/")
                .unwrap_or(branch_ref)
                .to_string();
            // `is_main` markiert jenen Worktree, dessen Pfad mit dem im
            // Mittelfeld gewählten Repo-Verzeichnis (`repo`) übereinstimmt –
            // nicht zwingend das Git-Top-Level.
            let is_main = repo == path.as_path();
            // Relative Pfade beziehen sich auf den im Mittelfeld gezeigten
            // Pfad (`repo`), nicht auf den Git-toplevel – sonst stimmen sie
            // nicht mit der Host/Repo-Spalte überein, wenn diese auf ein
            // Worktree zeigt.
            let rel = relative_to_base(path, repo);
            entries.push(WorktreeEntry {
                label: format!("{} ({})", rel, branch),
                path: path.clone(),
                branch: branch.clone(),
                is_main,
                has_worktree: true,
            });
            seen_branches.insert(branch);
        }
    }

    // Lokale Branches, die noch kein Worktree haben
    if let Ok(out) = crate::repo::git(repo, &["branch", "--format=%(refname:short)"]) {
        for line in out.lines() {
            let branch = line.trim().to_string();
            if branch.is_empty() || seen_branches.contains(&branch) {
                continue;
            }
            entries.push(WorktreeEntry {
                label: format!("{} (Kein Worktree)", branch),
                path: PathBuf::new(),
                branch: branch.clone(),
                is_main: false,
                has_worktree: false,
            });
            seen_branches.insert(branch);
        }
    }

    // Haupt-Worktree zuerst sortieren
    entries.sort_by(|a, b| match (a.is_main, b.is_main) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.branch.cmp(&b.branch),
    });

    entries
}

/// Prüft ob ein Verzeichnis **direkt** die Wurzel eines Git-Repositories ist:
/// entweder ein Hauptverzeichnis (enthält `.git/` als Verzeichnis) oder ein
/// Git-Worktree (enthält `.git` als Datei mit Repo-Pointer, dessen Ziel
/// existiert).
///
/// Ein bloßes *Unter*verzeichnis eines Repos zählt bewusst NICHT – es wird im
/// Channel Builder als repo-freies Host-Verzeichnis behandelt (keine
/// Worktree-Spalte, keine Branch-/Worktree-Logik beim Kanal-Erstellen).
///
/// Ein Worktree mit defektem Pointer (`.git`-Datei zeigt auf nicht
/// existierenden `gitdir`) wird ebenfalls als repo-freier Ordner behandelt.
pub fn is_repo_root(path: &Path) -> bool {
    let git_marker = path.join(".git");
    if !git_marker.exists() {
        return false;
    }
    // Bei einem Git-Worktree ist `.git` eine Datei mit einem `gitdir:`-Pointer.
    // Prüfe ob der referenzierte Pfad existiert – ein defekter Worktree-Pointer
    // wird wie ein repo-freier Ordner behandelt.
    if git_marker.is_file() {
        let content = match std::fs::read_to_string(&git_marker) {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(line) = content.lines().find(|l| l.starts_with("gitdir:")) {
            let gitdir = PathBuf::from(line.trim_start_matches("gitdir:").trim());
            let gitdir_abs = if gitdir.is_relative() {
                path.join(&gitdir)
            } else {
                gitdir
            };
            return gitdir_abs.exists();
        }
        return false;
    }
    true
}

/// Liefert das laut Config für `path` konfigurierte Default-Image.
///
/// Zusätzlich zur exakten Pfad-Übereinstimmung (bisheriges Verhalten) greift
/// hier auch ein `[path]`-Eintrag, dessen Git-Top-Level (Haupt-Repo-Root) mit
/// dem von `path` übereinstimmt. Damit gilt ein für das Haupt-Repo (oder einen
/// seiner Worktrees / Unterordner) konfiguriertes Image automatisch für
/// *alle* anderen Worktrees desselben Repos – egal ob `path` auf das
/// Top-Level, einen anderen Worktree oder ein Unterverzeichnis zeigt und
/// unabhängig davon, ob der konfigurierte Pfad selbst das Top-Level ist.
///
/// Voraussetzung für den Worktree-Match: sowohl der konfigurierte Pfad als auch
/// `path` müssen in einem Git-Repo liegen und dasselbe `git_topdir` haben.
pub fn default_image_for_path(config: &crate::config::Config, path: &Path) -> Option<String> {
    // 1. Exakte Pfad-Übereinstimmung (wie vorher) – höchste Priorität.
    let key = path.display().to_string();
    if let Some(img) = config.paths.get(&key).and_then(|e| e.image.clone()) {
        return Some(img);
    }

    // 2. Worktree-/Repo-Übereinstimmung: gleiches Git-Top-Level.
    //    Nur sinnvoll, wenn `path` selbst in einem Repo liegt.
    let top = crate::repo::git_toplevel(path).ok()?;
    // Stabile Reihenfolge, damit bei mehreren Treffern das Ergebnis
    // deterministisch ist (HashMap-Ordnung ist pro Lauf zufällig).
    let mut keys: Vec<&String> = config.paths.keys().collect();
    keys.sort();
    for k in keys {
        let Some(entry) = config.paths.get(k) else {
            continue;
        };
        let Some(img) = &entry.image else {
            continue;
        };
        if let Ok(cfg_top) = crate::repo::git_toplevel(Path::new(k)) {
            if cfg_top == top {
                return Some(img.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_label() {
        assert_eq!(Tunnel::Local.label(), "local");
        assert_eq!(
            Tunnel::Image {
                name: "node:22".into(),
                working_dir: None
            }
            .label(),
            "node"
        );
        assert_eq!(
            Tunnel::Image {
                name: "docker.io/library/node:22".into(),
                working_dir: None
            }
            .label(),
            "node"
        );
        assert_eq!(
            Tunnel::Image {
                name: "ghcr.io/org/repo:latest".into(),
                working_dir: None
            }
            .label(),
            "repo"
        );
    }

    #[test]
    fn normalize_image_name_variants() {
        // Kurzform bleibt unverändert
        assert_eq!(normalize_image_name("alpine"), "alpine");
        assert_eq!(normalize_image_name("node:22"), "node:22");
        // Voll qualifizierte Referenzen werden verkürzt
        assert_eq!(
            normalize_image_name("docker.io/library/alpine:latest"),
            "alpine"
        );
        assert_eq!(normalize_image_name("docker.io/library/node:22"), "node:22");
        assert_eq!(normalize_image_name("localhost/foo:latest"), "foo");
        assert_eq!(
            normalize_image_name("ghcr.io/org/repo:latest"),
            "ghcr.io/org/repo"
        );
    }

    #[test]
    fn image_names_equal_variants() {
        // Config-Kurzform vs. podman-Vollform muss matchen
        assert!(image_names_equal(
            "alpine",
            "docker.io/library/alpine:latest"
        ));
        assert!(image_names_equal("node:22", "docker.io/library/node:22"));
        assert!(image_names_equal("foo", "localhost/foo"));
        // Unterschiedliche Tags/Registries sind NICHT gleich
        assert!(!image_names_equal("alpine", "alpine:3.20"));
        assert!(!image_names_equal("alpine", "ghcr.io/x/alpine"));
    }

    #[test]
    fn image_short_name_variants() {
        assert_eq!(image_short_name("alpine"), "alpine");
        assert_eq!(
            image_short_name("docker.io/library/alpine:latest"),
            "alpine"
        );
        assert_eq!(image_short_name("ghcr.io/org/repo:v2"), "repo");
    }

    #[test]
    fn run_channel_name_ist_imagename_und_hostpfad() {
        let p = Path::new("/home/work/proj/.aidev/wt-feature");
        assert_eq!(
            run_channel_name("docker.io/library/alpine:latest", p),
            "alpine /home/work/proj/.aidev/wt-feature"
        );
        assert_eq!(
            run_channel_name("node:22", Path::new("/repo")),
            "node /repo"
        );
    }

    #[test]
    fn run_container_name_mit_und_ohne_branch() {
        // Repo + Branch → aidev-reponame-branchname-imagename
        assert_eq!(
            run_container_name("proj", Some("feature/x"), "docker.io/library/alpine:latest"),
            "aidev-proj-feature-x-alpine"
        );
        // Kein Repo → aidev-hostfoldername-imagename
        assert_eq!(
            run_container_name("mein-tool", None, "alpine"),
            "aidev-mein-tool-alpine"
        );
        // Sanitizing: Großbuchstaben und Sonderzeichen werden normalisiert
        assert_eq!(
            run_container_name("Mein Proj", Some("Feature/X"), "Node:22"),
            "aidev-mein-proj-feature-x-node"
        );
    }

    #[test]
    fn host_path_label() {
        let hp = HostPath {
            path: PathBuf::from("/home/test"),
        };
        assert_eq!(hp.label(), "/home/test");
    }

    #[test]
    fn is_repo_root_nur_bei_git_marker() {
        // Reine Dateisystem-Prüfung – kein git-Binary nötig.
        let base = std::env::temp_dir().join(format!("aidev-root-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // Hauptverzeichnis eines Repos: `.git/` als Verzeichnis.
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // Unterverzeichnis desselben Repos (kein eigenes .git).
        let sub = repo.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        // Ordner ganz ohne Repo.
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        assert!(is_repo_root(&repo), ".git/-Verzeichnis → Repo-Wurzel");
        assert!(
            !is_repo_root(&sub),
            "Unterverzeichnis eines Repos ist KEINE Repo-Wurzel"
        );
        assert!(!is_repo_root(&plain), "Ordner ohne .git → kein Repo");

        // Git-Worktree mit GÜLTIGEM Pointer: `.git` als Datei, deren
        // `gitdir:`-Ziel existiert → Repo-Wurzel.
        let gitdir_dir = base.join("main-repo/.git/worktrees/wt");
        std::fs::create_dir_all(&gitdir_dir).unwrap();
        let wt = base.join("wt-valid");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", gitdir_dir.display()),
        )
        .unwrap();
        assert!(
            is_repo_root(&wt),
            "Worktree mit gültigem gitdir → Repo-Wurzel"
        );

        // Git-Worktree mit DEFEKTEM Pointer: `gitdir:` zeigt auf
        // nicht existierenden Pfad → wird wie repo-freier Ordner behandelt.
        let wt_broken = base.join("wt-broken");
        std::fs::create_dir_all(&wt_broken).unwrap();
        std::fs::write(
            wt_broken.join(".git"),
            "gitdir: /irgendwo/.git/worktrees/wt\n",
        )
        .unwrap();
        assert!(
            !is_repo_root(&wt_broken),
            "Worktree mit defektem gitdir → KEIN Repo-Wurzel"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn collect_deduplicates() {
        let mut config_paths = std::collections::HashMap::new();
        config_paths.insert(
            "/tmp/test".to_string(),
            crate::config::PathEntry {
                image: Some("rust:latest".into()),
            },
        );
        // CWD ist der Test-Dir, nicht /tmp/test → kein Duplikat
        let paths = collect_host_paths(&config_paths, None);
        assert!(!paths.is_empty(), "mindestens Config + CWD");
    }

    #[test]
    fn relative_to_base_unter_basis() {
        // Worktree unterhalb der Basis → relativer Pfad
        assert_eq!(
            relative_to_base(Path::new("/home/proj/.aidev/wt-a"), Path::new("/home/proj")),
            ".aidev/wt-a"
        );
    }

    #[test]
    fn relative_to_base_auf_basis() {
        // Pfad == Basis → "."
        assert_eq!(
            relative_to_base(Path::new("/home/proj"), Path::new("/home/proj")),
            "."
        );
    }

    #[test]
    fn relative_to_base_geschwaestertes_worktree() {
        // Mittelfeld zeigt auf ein Worktree, Ziel ist ein Geschwister-Worktree
        // (das ist der Fall, den die Korrektur adressiert).
        assert_eq!(
            relative_to_base(
                Path::new("/home/proj/.aidev/wt-b"),
                Path::new("/home/proj/.aidev/wt-a")
            ),
            "../wt-b"
        );
    }

    #[test]
    fn relative_to_base_ueber_basis_hinaus() {
        // Mittelfeld zeigt auf ein Worktree, Ziel ist der toplevel (darüber)
        assert_eq!(
            relative_to_base(Path::new("/home/proj"), Path::new("/home/proj/.aidev/wt-a")),
            "../.."
        );
    }

    #[test]
    fn default_image_erreicht_andere_worktrees() {
        // Nur sinnvoll wenn git verfügbar ist.
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !git_ok {
            return;
        }

        let base = std::env::temp_dir().join(format!("aidev-wt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let top = base.join("repo");
        std::fs::create_dir_all(&top).unwrap();

        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} fehlgeschlagen: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let top_s = top.to_str().unwrap();
        run(&["-C", top_s, "init"]);
        run(&["-C", top_s, "config", "user.email", "t@t.de"]);
        run(&["-C", top_s, "config", "user.name", "T"]);
        std::fs::write(top.join("README.md"), "x").unwrap();
        run(&["-C", top_s, "add", "."]);
        run(&["-C", top_s, "commit", "-m", "init", "--allow-empty"]);
        run(&["-C", top_s, "branch", "feature"]);
        let wt = base.join("wt");
        run(&[
            "-C",
            top_s,
            "worktree",
            "add",
            wt.to_str().unwrap(),
            "feature",
        ]);

        // Config-Bild auf das Top-Level (Haupt-Repo) gesetzt …
        let mut paths = std::collections::HashMap::new();
        paths.insert(
            top.display().to_string(),
            crate::config::PathEntry {
                image: Some("node:22".into()),
            },
        );
        let config = crate::config::Config {
            paths,
            ..crate::config::Config::default()
        };

        // … gilt exakt für das Top-Level …
        assert_eq!(
            default_image_for_path(&config, &top),
            Some("node:22".to_string())
        );
        // … und für einen anderen Worktree desselben Repos.
        assert_eq!(
            default_image_for_path(&config, &wt),
            Some("node:22".to_string())
        );

        // Umgekehrt: Config-Bild auf den Worktree gesetzt …
        let mut paths2 = std::collections::HashMap::new();
        paths2.insert(
            wt.display().to_string(),
            crate::config::PathEntry {
                image: Some("rust:1".into()),
            },
        );
        let config2 = crate::config::Config {
            paths: paths2,
            ..crate::config::Config::default()
        };
        // … gilt auch für das Top-Level (gleiches git_topdir).
        assert_eq!(
            default_image_for_path(&config2, &top),
            Some("rust:1".to_string())
        );

        // Außerhalb eines Repos (nicht-git Pfad) ohne exakte Config → keins.
        assert_eq!(default_image_for_path(&config, Path::new("/tmp")), None);

        let _ = std::fs::remove_dir_all(&base);
    }
}
