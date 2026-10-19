//! Repo- und Worktree-Verwaltung für aidev-Sessions.
//!
//! Ein [`RepoManager`] erkennt ein Git-Repository aus einem beliebigen Pfad und
//! verwaltet die von aidev angelegten Worktrees (in `<repo>/.aidev/worktrees/`).
//! Beim `/branch`-Command wird ein neuer Branch mit eigenem Worktree angelegt,
//! optional mit Kopie der Build-Verzeichnisse (Hardlink-Strategie).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Typen
// ---------------------------------------------------------------------------

/// Verwaltung von Git-Worktrees für aidev-Sessions.
pub struct RepoManager {
    /// Wurzel des Git-Repos (enthält `.git/`).
    repo_root: PathBuf,
}

/// Metadaten eines angelegten Worktrees.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Kurzname, z.B. `"feature-x"` – auch Verzeichnisname unter `.aidev/worktrees/`.
    pub(crate) name: String,
    /// Git-Branch, z.B. `"feature-x"`.
    pub(crate) branch: String,
    /// Absoluter Host-Pfad zum Worktree.
    pub(crate) path: PathBuf,
    /// Nicht-fatale Rückmeldung zum Anlegen – z.B. ob Submodule ausgecheckt
    /// wurden. `None` = nichts zu berichten. Wird von der UI als Hinweis
    /// angezeigt, nie als harter Fehler.
    pub(crate) notice: Option<String>,
}

/// Plan für das Kopieren von Verzeichnissen beim Worktree-Erstellen.
#[derive(Debug, Clone)]
pub struct CopyPlan {
    pub entries: Vec<CopyEntry>,
}

/// Einzelner Kopiereintrag.
#[derive(Debug, Clone)]
pub struct CopyEntry {
    /// Quellverzeichnis relativ zur Repo-Wurzel, z.B. `"target"`.
    pub source: String,
    /// Kopierstrategie.
    pub strategy: CopyStrategy,
}

/// Kopierstrategie für Build-Verzeichnisse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStrategy {
    /// Hardlinks auf demselben Filesystem, Copies als Fallback.
    /// Sparsam, schnell; Anderungen im neuen Worktree brechen den Link.
    Hardlink,
    /// Vollständige Kopie (alle Dateien). Unabhängig, aber teurer.
    Full,
}

/// Ergebnis einer Kopieraktion.
#[derive(Debug, Clone)]
pub struct CopyStats {
    pub(crate) files_linked: u64,
    pub(crate) files_copied: u64,
    pub(crate) duration: Duration,
}

// ---------------------------------------------------------------------------
// Fehler
// ---------------------------------------------------------------------------

fn err(msg: impl Into<String>) -> String {
    msg.into()
}

// ---------------------------------------------------------------------------
// Git-Helfer
// ---------------------------------------------------------------------------

/// Führt ein `git`-Kommando im angegebenen Verzeichnis aus und liefert stdout.
pub(crate) fn git(run_dir: &Path, args: &[&str]) -> Result<String, String> {
    git_env(run_dir, args, &[])
}

/// Wie [`git`], setzt aber zusätzlich die angegebenen Umgebungsvariablen.
/// Gebraucht für Submodule-Klone: `GIT_TERMINAL_PROMPT=0`, damit eine
/// fehlende oder zu bestätigende Anmeldung nicht das TUI blockiert.
fn git_env(run_dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(run_dir);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd
        .output()
        .map_err(|e| err(format!("run git {}: {e}", args.join(" "))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(err(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Gibt das Top-Level des **Haupt**-Repos zurück.
///
/// In einem normalen Repo ist das identisch zu `git rev-parse --show-toplevel`.
/// In einem Worktree (wo `.git` eine Datei ist) wird der Pointer über
/// `gitdir` → `commondir` zurück zum Haupt-Repo verfolgt.
pub(crate) fn git_toplevel(path: &Path) -> Result<PathBuf, String> {
    let out = git(path, &["rev-parse", "--show-toplevel"])?;
    let toplevel = PathBuf::from(out.trim());

    // In einem Worktree ist `.git` eine Datei (kein Verzeichnis).
    // Sie enthält "gitdir: /pfad/zum/worktrees/<name>".
    let git_file = toplevel.join(".git");
    if !git_file.is_file() {
        return Ok(toplevel);
    }

    // gitdir-Pfad aus der .git-Datei lesen
    let content = std::fs::read_to_string(&git_file)
        .map_err(|e| err(format!("read {}: {e}", git_file.display())))?;
    let gitdir_line = content
        .lines()
        .find(|l| l.starts_with("gitdir:"))
        .ok_or_else(|| err(format!("gitdir not found in {}", git_file.display())))?;
    let gitdir = PathBuf::from(gitdir_line.trim_start_matches("gitdir:").trim());
    let gitdir_abs = if gitdir.is_relative() {
        toplevel.join(&gitdir)
    } else {
        gitdir
    };

    // commondir-Datei enthält den (relativen) Pfad zum gemeinsamen .git-Verzeichnis
    let commondir_file = gitdir_abs.join("commondir");
    let common_git_dir = if commondir_file.exists() {
        let rel = std::fs::read_to_string(&commondir_file)
            .map_err(|e| err(format!("read {}: {e}", commondir_file.display())))?;
        let rel = PathBuf::from(rel.trim());
        let joined = if rel.is_relative() {
            gitdir_abs.join(&rel)
        } else {
            rel
        };
        // Pfade normalisieren – PathBuf löst `..` nicht von selbst auf,
        // daher brauchen wir canonicalize, damit `.parent()` korrekt schneidet.
        std::fs::canonicalize(&joined)
            .map_err(|e| err(format!("resolve {}: {e}", joined.display())))?
    } else {
        // Fallback: Ohne commondir ist der gitdir selbst das gemeinsame Verzeichnis
        gitdir_abs
    };

    // Elternverzeichnis des gemeinsamen .git = Haupt-Repo-Root
    Ok(common_git_dir
        .parent()
        .unwrap_or(&common_git_dir)
        .to_path_buf())
}

/// Prüft ob Working Tree sauber ist (keine uncommitted Änderungen).
/// `.aidev/` wird ausgeschlossen – aidev eigene Verzeichnisse sind kein "Dreck".
pub(crate) fn git_is_clean(repo: &Path) -> Result<bool, String> {
    let status = git(repo, &["status", "--porcelain", "--", ":(exclude).aidev"])?;
    Ok(status.trim().is_empty())
}

/// Top des Stash-Stacks als Commit-Hash (`None` wenn kein Stash existiert).
/// Dient dazu, einen frisch erzeugten Stash-Eintrag eindeutig zu identifizieren,
/// statt ihn blind über seine Stackposition (`stash@{0}`) zu adressieren – so
/// können fremde, ältere Einträge nicht versehentlich gepoppt/gelöscht werden.
fn stash_top(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "-q", "--verify", "refs/stash"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Committet alle uncommitteten Änderungen (nach .gitignore) mit einer
/// generischen Nachricht – **einschließlich der Submodule**, von innen nach
/// außen. `.aidev/` wird ausgeschlossen.
///
/// Die Submodule sind dabei der springende Punkt: `git add -A` im Superprojekt
/// erfasst nur den Gitlink. Änderungen *innerhalb* eines Submodules ändern den
/// Gitlink nicht, stünden also ohne diesen Schritt nicht im Commit – und wären
/// verloren, wenn der Aufrufer den Worktree danach entfernt („Commit &
/// delete"). Committet wird von der tiefsten Ebene nach außen, damit jeder
/// Eltern-Commit den aktualisierten Gitlink seines Kindes schon enthält.
///
/// Gibt `true` zurück wenn mindestens ein Commit erstellt wurde (im
/// Superprojekt oder in einem Submodule).
pub(crate) fn git_commit_all(repo: &Path, message: &str) -> Result<bool, String> {
    let identity = commit_identity(repo);

    // `git submodule foreach` läuft von außen nach innen; für die Gitlinks
    // brauchen wir die umgekehrte Reihenfolge.
    let mut modules = populated_submodule_paths(repo)?;
    modules.reverse();

    let mut committed = false;
    for path in modules {
        if commit_all_in(&repo.join(path), message, &identity)? {
            committed = true;
        }
    }
    if commit_all_in(repo, message, &identity)? {
        committed = true;
    }
    Ok(committed)
}

/// Identität (`user.name`, `user.email`) aus der Config von `repo`.
///
/// Submodule sind eigenständige Klone **ohne** eigene `user.*`-Config; ein
/// Commit darin scheiterte sonst mit „Author identity unknown". Leer, wenn im
/// Repo nichts konfiguriert ist – dann greift wie bisher die globale Config.
fn commit_identity(repo: &Path) -> Vec<(String, String)> {
    ["user.name", "user.email"]
        .iter()
        .filter_map(|key| {
            let value = git(repo, &["config", "--get", key]).ok()?;
            let value = value.trim().to_string();
            (!value.is_empty()).then(|| ((*key).to_string(), value))
        })
        .collect()
}

/// Pfade der ausgecheckten Submodule von `repo`, rekursiv, in der Reihenfolge
/// von außen nach innen. Nicht bevölkerte Module werden übersprungen – dort
/// gibt es nichts zu committen.
fn populated_submodule_paths(repo: &Path) -> Result<Vec<String>, String> {
    // `--quiet` unterdrückt die „Entering …"-Zeilen, übrig bleiben die Pfade.
    let out = git(
        repo,
        &[
            "submodule",
            "foreach",
            "--recursive",
            "--quiet",
            r#"echo "$displaypath""#,
        ],
    )?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Committet die uncommitteten Änderungen in `dir` (`.aidev/` ausgenommen) mit
/// der übergebenen Identität. Committet nur, wenn es etwas zu committen gibt.
fn commit_all_in(dir: &Path, message: &str, identity: &[(String, String)]) -> Result<bool, String> {
    let mut prefix: Vec<String> = Vec::new();
    for (key, value) in identity {
        prefix.push("-c".to_string());
        prefix.push(format!("{key}={value}"));
    }
    let run = |extra: &[&str]| -> Result<String, String> {
        let mut args = prefix.clone();
        args.extend(extra.iter().map(|s| (*s).to_string()));
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        git(dir, &args_ref)
    };

    // Alle Änderungen stagen (respektiert .gitignore, schließt .aidev aus)
    run(&["add", "-A", "--", ":(exclude).aidev"])?;
    // Prüfen ob es tatsächlich was zu committen gibt
    if run(&["diff", "--cached", "--stat"])?.trim().is_empty() {
        return Ok(false);
    }
    // Committen
    run(&["commit", "-m", message])?;
    Ok(true)
}

/// Liefert den aktuellen Branch-Namen (via `git rev-parse --abbrev-ref HEAD`).
pub(crate) fn git_current_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
}

/// Erstellt einen neuen Branch + Worktree.
/// `source` ist das Quell-Repo (oder ein bestehender Worktree), `branch` der
/// neue Branch-Name, `dest` das Zielverzeichnis für den Worktree.
fn git_worktree_add(source: &Path, branch: &str, dest: &Path) -> Result<(), String> {
    // Erst prüfen ob der Branch schon existiert
    let branches = git(source, &["branch", "--list", branch])?;
    if !branches.trim().is_empty() {
        // Branch existiert schon → nur Worktree addieren (ohne -b)
        git(
            source,
            &["worktree", "add", dest.to_str().unwrap_or("."), branch],
        )?;
    } else {
        git(
            source,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                dest.to_str().unwrap_or("."),
            ],
        )?;
    }
    Ok(())
}

/// Prüft ob ein lokaler Branch existiert.
pub(crate) fn git_branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    )
    .is_ok()
}

/// Liefert den Commit-Hash, auf den HEAD zeigt (kurz, 12 Zeichen).
pub(crate) fn git_head_commit(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--short=12", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Liefert den Commit-Hash, auf den ein Branch zeigt.
pub(crate) fn git_branch_commit(repo: &Path, branch: &str) -> Option<String> {
    git(
        repo,
        &["rev-parse", "--short=12", &format!("refs/heads/{branch}")],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// Findet den Worktree-Pfad für einen existierenden Branch (via `git worktree list`).
pub(crate) fn git_worktree_for_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            current_branch = Some(b.trim().to_string());
        } else if line.is_empty() {
            // Ende eines Eintrags
            if current_branch.as_deref() == Some(&format!("refs/heads/{branch}")) {
                return current_path;
            }
            current_path = None;
            current_branch = None;
        }
    }
    // Letzter Eintrag (kein abschließendes Leerzeichen)
    if current_branch.as_deref() == Some(&format!("refs/heads/{branch}")) {
        return current_path;
    }
    None
}

/// Vergleicht die Working-Tree-Status zweier Verzeichnisse (porcelain, .aidev ausgeschlossen).
/// Liefert `true` wenn beide denselben Status haben.
pub(crate) fn git_same_status(a: &Path, b: &Path) -> bool {
    let status_a = git(a, &["status", "--porcelain", "--", ":(exclude).aidev"]).unwrap_or_default();
    let status_b = git(b, &["status", "--porcelain", "--", ":(exclude).aidev"]).unwrap_or_default();
    status_a == status_b
}

/// Verschiebt einen Branch auf einen anderen Commit (`git branch -f`).
pub(crate) fn git_force_branch(repo: &Path, branch: &str, target: &str) -> Result<(), String> {
    git(repo, &["branch", "-f", branch, target]).map(|_| ())
}

/// Entfernt einen Worktree und optionale den zugehörigen Branch.
/// WICHTIG: Muss aus dem Repo-Root (nicht aus dem Worktree) heraus aufgerufen werden.
fn git_worktree_remove(repo_root: &Path, worktree: &Path, force: bool) -> Result<(), String> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(worktree.to_str().unwrap_or("."));
    git(repo_root, &args).map(|_| ())
}

/// Löscht einen lokalen Branch.
fn git_branch_delete(repo: &Path, branch: &str, force: bool) -> Result<(), String> {
    let mut args = vec!["branch", "-D"];
    if !force {
        args = vec!["branch", "-d"];
    }
    args.push(branch);
    git(repo, &args).map(|_| ())
}

// ---------------------------------------------------------------------------
// Submodule
// ---------------------------------------------------------------------------
//
// `git worktree add` checkt KEINE Submodule aus: Die Verzeichnisse bleiben
// leer, `git status` meldet trotzdem „clean“ – der Fehler fällt erst beim
// Bauen auf. aidev holt das deshalb direkt nach dem Anlegen nach.
//
// Bevorzugt wird aus den lokalen Gitdaten der Hauptinstanz geklont
// (`<repo>/.git/modules/<name>`, verschachtelt
// `<repo>/.git/modules/<parent>/modules/<child>`). Dazu wird die URL jedes
// Moduls per `-c submodule.<name>.url=<lokaler Gitdir>` ersetzt – die
// `-c`-Config gilt prozessweit und erreicht deshalb auch verschachtelte
// Ebenen.
//
// Bewusst NICHT `url.<lokal>.insteadOf=<url>`: Das matcht URL-*Text*, den git
// vorher normalisiert bzw. relativ auflöst – bei absoluten Pfaden und
// relativen URLs greift die Regel deshalb nicht (gemessen). Der Name-Override
// ersetzt die URL dagegen vollständig und funktioniert für jede URL-Form.
// Preis dafür: Er gilt pro Name, die Config kennt keine Ebene – gleichnamige
// Module auf verschiedenen Ebenen bleiben deshalb ohne Override (siehe
// [`unambiguous_sources`]).
//
// Ist der lokale Gitdir nicht vorhanden (Hauptinstanz selbst nicht
// bevölkert), läuft der reguläre Weg über die konfigurierte URL – dann
// braucht es den Server. Fehler sind nie fatal: Der Worktree bleibt nutzbar,
// die Meldung landet als Hinweis in [`WorktreeInfo::notice`].

/// Maximale Verschachtelungstiefe beim Einsammeln der Submodule.
const SUBMODULE_MAX_DEPTH: usize = 8;

/// Ein Submodule, dessen Gitdir in der Hauptinstanz bevölkert vorliegt und
/// damit als lokale Klon-Quelle dienen kann.
#[derive(Debug, Clone)]
struct SubmoduleSource {
    /// Modulname aus `.gitmodules` (Schlüssel des `submodule.<name>.url`-Overrides).
    name: String,
    /// Lokaler Gitdir in der Hauptinstanz (Ersatz-URL).
    local: PathBuf,
}

/// Submodule-Bestand der Hauptinstanz.
#[derive(Debug, Default)]
struct SubmoduleInventory {
    /// Anzahl gefundener Submodule (rekursiv).
    total: usize,
    /// Nur die Module, für die es lokal eine bevölkerte Klon-Quelle gibt.
    sources: Vec<SubmoduleSource>,
}

/// Ergebnis des Submodule-Auscheckens nach `git worktree add`.
#[derive(Debug)]
enum SubmoduleOutcome {
    /// Im ausgecheckten Stand gibt es keine Submodule – nichts zu melden.
    Absent,
    /// `count` Submodule ausgecheckt, alle aus den lokalen Gitdaten.
    AllLocal(usize),
    /// `count` Submodule ausgecheckt, (auch) über die konfigurierten URLs.
    FromUrls(usize),
    /// `count` Submodule ausgecheckt – ein Teil lokal, der Rest über die URLs.
    PartlyLocal(usize),
    /// Der Aufruf war erfolgreich, aber Module blieben leer.
    Incomplete { populated: usize, empty: usize },
    /// Fehlgeschlagen – der Worktree bleibt nutzbar, ist dort aber leer.
    Failed(String),
}

impl SubmoduleOutcome {
    /// Hinweistext für die UI (`None` = nichts zu melden).
    fn notice(&self) -> Option<String> {
        match self {
            SubmoduleOutcome::Absent => Option::None,
            SubmoduleOutcome::AllLocal(count) => Some(format!(
                "Submodules checked out: {count} (from the local data of the main worktree)"
            )),
            SubmoduleOutcome::FromUrls(count) => Some(format!(
                "Submodules checked out: {count} (from their configured URLs)"
            )),
            SubmoduleOutcome::PartlyLocal(count) => Some(format!(
                "Submodules checked out: {count} (partly from the local data of the main worktree)"
            )),
            SubmoduleOutcome::Incomplete { populated, empty } => Some(format!(
                "Submodules: {populated} checked out, {empty} stayed empty. Run \
                 'git submodule update --init --recursive' inside the worktree."
            )),
            SubmoduleOutcome::Failed(e) => Some(format!(
                "Submodules were NOT checked out ({e}). Run \
                 'git submodule update --init --recursive' inside the worktree."
            )),
        }
    }
}

/// Liest die Einträge der `.gitmodules` eines Verzeichnisses als
/// `(name, path)`-Paare. Leer, wenn es keine lesbare `.gitmodules` gibt.
fn gitmodules_entries(dir: &Path) -> Vec<(String, String)> {
    let Ok(out) = git(
        dir,
        &[
            "config",
            "-f",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.path$",
        ],
    ) else {
        return Vec::new();
    };

    out.lines()
        .filter_map(|line| {
            // Ausgabeformat ist "<key> <value>" – genau ein Leerzeichen als Trenner.
            let (key, value) = line.split_once(' ')?;
            let name = key.strip_prefix("submodule.")?.strip_suffix(".path")?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Entfernt mehrdeutige Module: Der `submodule.<name>.url`-Override gilt pro
/// Name, die Config kennt keine Verschachtelungsebene. Kommt derselbe Name auf
/// zwei Ebenen mit unterschiedlichem lokalem Gitdir vor, ließe sich nicht
/// entscheiden, welche Quelle gemeint ist – dann lieber ohne Override klonen
/// (Original-URL) als die falsche Quelle zu nehmen.
fn unambiguous_sources(sources: Vec<SubmoduleSource>) -> Vec<SubmoduleSource> {
    let mut out: Vec<SubmoduleSource> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    for src in sources {
        if ambiguous.contains(&src.name) {
            continue;
        }
        match out.iter().find(|s| s.name == src.name) {
            // Gleicher Name, gleicher Gitdir: ein Override genügt.
            Some(existing) if existing.local == src.local => {}
            None => out.push(src),
            Some(_) => {
                ambiguous.push(src.name.clone());
                out.retain(|s| s.name != src.name);
            }
        }
    }
    out
}

/// Lokaler Gitdir eines Submodules (`modules/<name>`) relativ zu `dir`, sofern
/// dort vorhanden.
///
/// Bei verschachtelten Submodulen ist `dir` das Arbeitsverzeichnis des
/// Elternmoduls in der Hauptinstanz; `git rev-parse` löst den Pfad dann gegen
/// dessen Gitdir auf, sodass automatisch
/// `.../modules/<parent>/modules/<child>` herauskommt.
fn local_module_gitdir(dir: &Path, name: &str) -> Option<PathBuf> {
    let rel = format!("modules/{name}");
    let resolve = |args: &[&str]| {
        git(dir, args)
            .ok()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| !p.as_os_str().is_empty())
    };
    // `--path-format=absolute` gibt es erst ab git 2.31; der zweite Versuch
    // kommt ohne aus (dann relativ zum Repo-Verzeichnis auflösen).
    let path = resolve(&["rev-parse", "--path-format=absolute", "--git-path", &rel])
        .or_else(|| resolve(&["rev-parse", "--git-path", &rel]))?;
    let path = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    path.is_dir().then_some(path)
}

/// Zählt die Submodule im tatsächlich ausgecheckten Stand eines Worktrees als
/// `(bevölkert, leer)`.
///
/// Bewusst über den Index (Gitlinks) und das Dateisystem statt über `git
/// submodule status`: Dessen führendes `-` heißt nur „nicht in der Config
/// registriert". Ein Modul, dessen URL per `-c` überschrieben wurde, bleibt
/// ohne Config-Eintrag und gälte dort fälschlich als leer. „Bevölkert" heißt
/// hier: Das Verzeichnis existiert und ist nicht leer – ein Checkout enthält
/// immer mindestens die `.git`-Datei, ein von `git worktree add`
/// liegengelassenes Submodule dagegen gar nichts.
fn submodule_worktree_state(worktree: &Path) -> (usize, usize) {
    let mut populated = 0;
    let mut empty = 0;
    collect_gitlink_state(worktree, &mut populated, &mut empty);
    (populated, empty)
}

fn collect_gitlink_state(dir: &Path, populated: &mut usize, empty: &mut usize) {
    for path in gitlink_paths(dir) {
        let child = dir.join(&path);
        let has_entries = std::fs::read_dir(&child)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if has_entries {
            *populated += 1;
            // Nur bevölkerte Module können überhaupt Unter-Submodule haben.
            collect_gitlink_state(&child, populated, empty);
        } else {
            *empty += 1;
        }
    }
}

/// Pfade der Gitlinks (Mode `160000`) im Index von `dir`.
fn gitlink_paths(dir: &Path) -> Vec<String> {
    let Ok(out) = git(dir, &["ls-files", "--stage", "-z"]) else {
        return Vec::new();
    };
    out.split('\0')
        .filter_map(|entry| {
            // Format je Eintrag: "<mode> <sha> <stage>\t<path>"
            let (meta, path) = entry.split_once('\t')?;
            meta.starts_with("160000 ").then(|| path.to_string())
        })
        .collect()
}

/// Sammelt rekursiv alle Submodule der Hauptinstanz ein und notiert, für
/// welche davon lokal ein bevölkerter Gitdir als Klon-Quelle bereitsteht.
fn submodule_inventory(root: &Path) -> SubmoduleInventory {
    let mut inv = SubmoduleInventory::default();
    // Schneller Ausstieg: Ohne `.gitmodules` gibt es nichts zu tun (spart den
    // git-Aufruf für die allermeisten Repos).
    if !root.join(".gitmodules").is_file() {
        return inv;
    }
    collect_submodules(root, 0, &mut inv);
    inv
}

fn collect_submodules(dir: &Path, depth: usize, inv: &mut SubmoduleInventory) {
    if depth > SUBMODULE_MAX_DEPTH {
        return;
    }
    for (name, path) in gitmodules_entries(dir) {
        inv.total += 1;
        if let Some(local) = local_module_gitdir(dir, &name) {
            inv.sources.push(SubmoduleSource { name, local });
        }

        // Nur in ausgecheckte Submodule absteigen – nur die haben eine
        // `.gitmodules` vorliegen, aus der die Kind-Einträge stammen.
        let child = dir.join(&path);
        if child.join(".git").exists() {
            collect_submodules(&child, depth + 1, inv);
        }
    }
}

// ---------------------------------------------------------------------------
// RepoManager
// ---------------------------------------------------------------------------

impl RepoManager {
    /// Erkennt das Repo aus einem beliebigen Pfad innerhalb.
    /// Auch aus einem Worktree heraus wird korrekt das Haupt-Repo aufgelöst
    /// (indem der `.git`-Worktree-Pointer über `commondir` zurückverfolgt wird).
    pub fn discover(from: &Path) -> Result<Self, String> {
        let toplevel = git_toplevel(from)?;
        Ok(RepoManager {
            repo_root: toplevel,
        })
    }

    /// Wurzel des Git-Repos.
    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Pfad zum `.aidev/worktrees/`-Verzeichnis.
    fn worktrees_base(&self) -> PathBuf {
        self.repo_root.join(".aidev").join("worktrees")
    }

    /// Erzeugt einen neuen Worktree auf einem neuen (oder bestehenden) Branch.
    ///
    /// Funktioniert unabhängig vom Zustand des Quell-Worktrees: `git worktree
    /// add` fasst die Quelle gar nicht an, uncommittete Änderungen bleiben
    /// dort unberührt liegen (der neue Worktree startet vom Branch-Tip).
    /// Sollen die Änderungen zusätzlich in den neuen Worktree übernommen
    /// werden, stattdessen [`RepoManager::create_worktree_with_stash`] nutzen.
    ///
    /// Submodule werden direkt mit ausgecheckt (siehe Abschnitt „Submodule“).
    /// Das Ergebnis steht als [`WorktreeInfo::notice`] bereit und ist nie ein
    /// Fehler: Schlägt es fehl, bleibt der Worktree nutzbar, aber der Aufrufer
    /// muss den Hinweis anzeigen – ein leerer Submodule-Ordner fiele sonst
    /// nicht auf, weil `git status` dort „clean“ meldet.
    pub fn create_worktree(
        &self,
        branch_name: &str,
        source_worktree: Option<&Path>,
    ) -> Result<WorktreeInfo, String> {
        let source = source_worktree.unwrap_or(&self.repo_root);

        // Zielverzeichnis
        let dest = self.worktrees_base().join(branch_name);
        if dest.exists() {
            return Err(err(format!(
                "Worktree '{branch_name}' already exists: {}",
                dest.display()
            )));
        }

        // Elternverzeichnis erzeugen
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| err(format!("create {}: {e}", parent.display())))?;
        }

        // Worktree + Branch anlegen
        git_worktree_add(source, branch_name, &dest)?;

        // `git worktree add` lässt Submodule leer – direkt nachziehen.
        let notice = self.checkout_submodules(&dest).notice();

        Ok(WorktreeInfo {
            name: branch_name.to_string(),
            branch: branch_name.to_string(),
            path: dest,
            notice,
        })
    }

    /// Checkt die Submodule eines frisch angelegten Worktrees aus.
    ///
    /// Klont bevorzugt aus den lokalen Gitdaten der Hauptinstanz (siehe
    /// Abschnitt „Submodule“ oben) und damit ohne Serververbindung; fehlt der
    /// lokale Gitdir, greift der reguläre Weg über die konfigurierte URL.
    /// Nie fatal – das Ergebnis wird als Hinweis gemeldet, nicht als Fehler.
    fn checkout_submodules(&self, worktree: &Path) -> SubmoduleOutcome {
        let inv = submodule_inventory(&self.repo_root);
        if inv.total == 0 {
            return SubmoduleOutcome::Absent;
        }
        let total = inv.total;
        let sources = unambiguous_sources(inv.sources);

        // `protocol.file.allow=always` ist für lokale Klon-Quellen Pflicht,
        // sonst lehnt git sie mit "transport 'file' not allowed" ab.
        let mut args: Vec<String> =
            vec!["-c".to_string(), "protocol.file.allow=always".to_string()];
        for src in &sources {
            args.push("-c".to_string());
            args.push(format!(
                "submodule.{}.url={}",
                src.name,
                src.local.display()
            ));
        }
        for arg in ["submodule", "update", "--init", "--recursive"] {
            args.push(arg.to_string());
        }
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();

        // Ohne `GIT_TERMINAL_PROMPT=0` würde ein fehlendes Credential im
        // Server-Fallback eine Eingabe erwarten und das TUI blockieren.
        if let Err(e) = git_env(worktree, &args_ref, &[("GIT_TERMINAL_PROMPT", "0")]) {
            return SubmoduleOutcome::Failed(e);
        }

        // Verifizieren statt vertrauen: `submodule status` zählt die Module des
        // tatsächlich ausgecheckten Stands. Damit stimmt die Meldung auch dann,
        // wenn der neue Branch gar keine Submodule enthält (dann gibt es nichts
        // zu melden) oder wenn trotz erfolgreichem Aufruf Module leer blieben.
        let (populated, empty) = submodule_worktree_state(worktree);
        if empty > 0 {
            return SubmoduleOutcome::Incomplete { populated, empty };
        }
        if populated == 0 {
            return SubmoduleOutcome::Absent;
        }
        // Herkunft nur so genau angeben, wie sie belegbar ist: Nur wenn jedes
        // gefundene Modul eine lokale Quelle hatte, lief der Klon garantiert
        // ohne Server.
        if sources.len() == total {
            SubmoduleOutcome::AllLocal(populated)
        } else if sources.is_empty() {
            SubmoduleOutcome::FromUrls(populated)
        } else {
            SubmoduleOutcome::PartlyLocal(populated)
        }
    }

    /// Wendet einen gemerkten Stash-Eintrag erneut im Quell-Worktree an und
    /// räumt ihn weg, wenn er noch oben auf dem Stapel liegt.
    ///
    /// Bewusst `apply <hash>` statt blindem `pop`/`drop`: Der Eintrag wird
    /// über seinen Commit-Hash adressiert, damit niemals ein fremder, älterer
    /// Stash-Eintrag erwischt wird, falls zwischenzeitlich (in einem anderen
    /// Worktree) gestasht wurde.
    fn restore_source_stash(&self, source: &Path, hash: Option<&str>) {
        let Some(hash) = hash else { return };
        let _ = git(source, &["stash", "apply", hash]);
        // Nur unser eigener Eintrag darf fallen; liegt er nicht mehr oben,
        // bleibt er liegen (harmlos) statt fremde Einträge zu löschen.
        if stash_top(source).as_deref() == Some(hash) {
            let _ = git(source, &["stash", "drop", "stash@{0}"]);
        }
    }

    /// Erzeugt einen Worktree und übernimmt uncommittete Änderungen.
    ///
    /// Der neue Worktree startet vom Branch-Tip; liegt im Quell-Worktree noch
    /// uncommittetes Arbeiten, werden diese Änderungen (**inklusive**
    /// ungetrackter Dateien) zusätzlich in den neuen Worktree übernommen und
    /// in der Quelle exakt wiederhergestellt – mechanisch via `git stash push
    /// --include-untracked` + `stash apply`. Der eigene Stash-Eintrag wird
    /// über seinen Commit-Hash verfolgt und nur er selbst aufgeräumt –
    /// fremde, ältere Einträge bleiben unangetastet. Scheitert das Stashen
    /// (z. B. bei kryptischen Index-Zuständen), passiert nichts: Es ist vor
    /// dem Anlegen und bricht den ganzen Vorgang ab. Wer die Änderungen NICHT
    /// mitnehmen will, nutzt direkt [`RepoManager::create_worktree`] – das
    /// funktioniert unabhängig vom Zustand der Quelle.
    pub fn create_worktree_with_stash(
        &self,
        branch_name: &str,
        source_worktree: Option<&Path>,
    ) -> Result<WorktreeInfo, String> {
        let source = source_worktree.unwrap_or(&self.repo_root);

        // 1. Änderungen beiseiteräumen – auch ungetrackte Dateien. `.aidev`
        //    wird ausgeschlossen (wie überall): aidevs eigene Verzeichnisse
        //    gehören nicht in den neuen Worktree.
        let mut our_stash: Option<String> = None;
        if !git_is_clean(source)? {
            let before = stash_top(source);
            git(
                source,
                &[
                    "stash",
                    "push",
                    "--include-untracked",
                    "--message",
                    &format!("aidev-worktree-{branch_name}"),
                    "--",
                    ":(exclude).aidev",
                ],
            )?;
            let after = stash_top(source);
            // Neuen Eintrag nur merken, wenn wirklich einer entstanden ist.
            if after.is_some() && after != before {
                our_stash = after;
            }
        }

        // 2. Worktree anlegen (klappt unabhängig vom Quell-Zustand; durch den
        //    Stash in Schritt 1 sind dessen Änderungen aber „weggeräumt“ und
        //    können sauber in den neuen Worktree angewendet werden).
        let result = self.create_worktree(branch_name, Some(source));

        match result {
            Ok(wt) => {
                // 3. Gemerkte Änderungen im neuen Worktree anwenden. Fehler
                //    (z. B. Konflikte) sind nicht fatal – die Quelle wird
                //    ohnehin wiederhergestellt und der User sieht den Stand.
                if let Some(hash) = &our_stash {
                    let _ = git(&wt.path, &["stash", "apply", hash]);
                }
                self.restore_source_stash(source, our_stash.as_deref());
                Ok(wt)
            }
            Err(e) => {
                // Fehlerfall: Quelle unbedingt wiederherstellen, dann den
                // ursprünglichen Fehler reichen.
                self.restore_source_stash(source, our_stash.as_deref());
                Err(e)
            }
        }
    }

    /// Prüft ob ein bestimmter Worktree sauber ist.
    pub fn is_clean(&self, worktree: &Path) -> Result<bool, String> {
        git_is_clean(worktree)
    }

    /// Gibt eine Zusammenfassung des Working-Tree-Status zurück (porcelain output).
    /// `.aidev/` wird ausgeschlossen.
    pub(crate) fn status_summary(&self, worktree: &Path) -> Result<String, String> {
        let status = git(
            worktree,
            &["status", "--porcelain", "--", ":(exclude).aidev"],
        )?;
        let trimmed = status.trim();
        if trimmed.is_empty() {
            return Ok("No uncommitted changes.".to_string());
        }
        let lines: Vec<&str> = trimmed.lines().collect();
        let count = lines.len();
        Ok(format!("{count} uncommitted change(s):\n{trimmed}"))
    }

    /// Löscht einen Worktree und den zugehörigen Branch.
    ///
    /// Prüft vorher auf Sauberkeit, es sei denn `force` ist `true`.
    /// Der Worktree-Branch wird nur gelöscht wenn er nicht der aktuelle
    /// HEAD des Original-Worktrees ist.
    ///
    /// Hinweis: Enthält der Worktree ausgecheckte Submodule (macht
    /// [`RepoManager::create_worktree`] inzwischen selbst), verweigert `git
    /// worktree remove` das Entfernen ohne `--force` – unabhängig davon, wie
    /// sauber der Worktree ist („working trees containing submodules cannot be
    /// moved or removed“). Aufräumpfade müssen deshalb mit `force = true`
    /// arbeiten. `--force` räumt auch die worktree-eigenen Submodule-Gitdirs
    /// mit ab (`<repo>/.git/worktrees/<name>/modules/…`).
    pub fn remove_worktree(&self, name: &str, force: bool) -> Result<(), String> {
        let wt_path = self.worktrees_base().join(name);
        if !wt_path.exists() {
            return Err(err(format!(
                "Worktree '{name}' does not exist: {}",
                wt_path.display()
            )));
        }

        // Sauberkeit prüfen
        if !force && !git_is_clean(&wt_path)? {
            return Err(err(format!(
                "Worktree '{name}' has uncommitted changes. \
                 Confirm with force=true or commit/stash first."
            )));
        }

        // Branch-Name ermitteln
        let branch = git(&wt_path, &["symbolic-ref", "HEAD"])
            .ok()
            .and_then(|s| s.trim().strip_prefix("refs/heads/").map(str::to_string));

        // Worktree entfernen – aus dem Repo-Root heraus (nicht aus dem Worktree)
        git_worktree_remove(&self.repo_root, &wt_path, force)?;

        // Branch löschen (nur wenn nicht main/master) – nur wenn der Commit
        // durch andere Referenzen erreichbar ist (`-d` statt `-D`).
        if let Some(ref b) = branch {
            let is_default = matches!(b.as_str(), "main" | "master" | "develop" | "dev");
            if !is_default {
                let _ = git_branch_delete(&self.repo_root, b, false);
            }
        }

        Ok(())
    }

    /// Führt einen [`CopyPlan`] aus: kopiert die definierten Verzeichnisse
    /// aus dem Quell-Worktree in das Ziel-Worktree.
    pub fn execute_copy_plan(
        &self,
        source: &Path,
        dest: &Path,
        plan: &CopyPlan,
    ) -> Result<CopyStats, String> {
        let start = Instant::now();
        let mut total_files_linked: u64 = 0;
        let mut total_files_copied: u64 = 0;

        for entry in &plan.entries {
            let (src_dir, dst_dir) = if entry.source == "." {
                (source.to_path_buf(), dest.to_path_buf())
            } else {
                (source.join(&entry.source), dest.join(&entry.source))
            };

            if !src_dir.exists() {
                continue;
            }

            std::fs::create_dir_all(&dst_dir)
                .map_err(|e| err(format!("create {}: {e}", dst_dir.display())))?;

            let (linked, copied) = match entry.strategy {
                CopyStrategy::Hardlink => copy_dir_hardlink(&src_dir, &dst_dir)?,
                CopyStrategy::Full => copy_dir_full(&src_dir, &dst_dir)?,
            };
            total_files_linked += linked;
            total_files_copied += copied;
        }

        Ok(CopyStats {
            files_linked: total_files_linked,
            files_copied: total_files_copied,
            duration: start.elapsed(),
        })
    }
}

// ---------------------------------------------------------------------------
// Kopier-Strategien
// ---------------------------------------------------------------------------

/// Kopiert ein Verzeichnis rekursiv mit Hardlinks (Fallback: Kopie).
/// Gibt (linked, copied, bytes) zurück.
fn copy_dir_hardlink(src: &Path, dst: &Path) -> Result<(u64, u64), String> {
    let mut linked = 0u64;
    let mut copied = 0u64;

    copy_dir_recursive(src, dst, CopyStrategy::Hardlink, &mut linked, &mut copied)?;
    Ok((linked, copied))
}

/// Kopiert ein Verzeichnis rekursiv (vollständige Kopie).
fn copy_dir_full(src: &Path, dst: &Path) -> Result<(u64, u64), String> {
    let mut linked = 0u64;
    let mut copied = 0u64;

    copy_dir_recursive(src, dst, CopyStrategy::Full, &mut linked, &mut copied)?;
    Ok((linked, copied))
}

/// Rekursive Kopierfunktion.
fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    strategy: CopyStrategy,
    linked: &mut u64,
    copied: &mut u64,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(src).map_err(|e| err(format!("read {}: {e}", src.display())))?;

    for entry in entries.flatten() {
        // .aidev/ ist aidev-intern – nie kopieren
        if entry.file_name() == ".aidev" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = entry
            .metadata()
            .map_err(|e| err(format!("read {}: {e}", src_path.display())))?;

        if meta.is_dir() {
            std::fs::create_dir_all(&dst_path)
                .map_err(|e| err(format!("create {}: {e}", dst_path.display())))?;
            copy_dir_recursive(&src_path, &dst_path, strategy, linked, copied)?;
        } else if meta.is_file() {
            match strategy {
                CopyStrategy::Hardlink => {
                    // Versuche Hardlink; bei Fehler → Kopie
                    if std::fs::hard_link(&src_path, &dst_path).is_ok() {
                        *linked += 1;
                    } else {
                        std::fs::copy(&src_path, &dst_path)
                            .map_err(|e| err(format!("copy {}: {e}", src_path.display())))?;
                        *copied += 1;
                    }
                }
                CopyStrategy::Full => {
                    std::fs::copy(&src_path, &dst_path)
                        .map_err(|e| err(format!("copy {}: {e}", src_path.display())))?;
                    *copied += 1;
                }
            }
        }
        // Symlinks werden ignoriert (nicht portable)
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Eindeutiges Temp-Verzeichnis pro Aufruf.
    ///
    /// Nötig, weil die Tests parallel im selben Prozess laufen: Die PID allein
    /// unterscheidet sie nicht, und zwei Tests im selben Verzeichnis löschen
    /// sich gegenseitig die Repos weg – was still als „git nicht verfügbar"
    /// durchgeht und die Tests wertlos macht.
    fn unique_temp(prefix: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "aidev-{prefix}-{}-{seq}-{nanos}",
            std::process::id()
        ))
    }

    /// Erzeugt ein temporäres Git-Repo für Tests.
    /// Läuft nur wenn `git` verfügbar ist.
    fn test_repo() -> Option<(PathBuf, PathBuf)> {
        // Prüfe ob git verfügbar ist
        let ok = Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }

        let dir = unique_temp("repo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init"]).ok()?;
        git(&dir, &["config", "user.email", "test@test.de"]).ok()?;
        git(&dir, &["config", "user.name", "Test"]).ok()?;
        std::fs::write(dir.join("README.md"), "# test\n").unwrap();
        git(&dir, &["add", "."]).ok()?;
        git(&dir, &["commit", "-m", "init", "--allow-empty"]).ok()?;
        Some((dir.clone(), dir))
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Legt ein nicht-bare Repo mit Identität an (ohne Commit).
    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]).unwrap();
        git(dir, &["config", "user.email", "test@test.de"]).unwrap();
        git(dir, &["config", "user.name", "Test"]).unwrap();
    }

    /// Staged alles und committet (`--allow-empty`, damit auch leere
    /// Zwischenstände gehen).
    ///
    /// Mit expliziter Identität: Submodule-Klone haben keine eigene
    /// `user.*`-Config, ein Commit darin scheiterte sonst mit „Author identity
    /// unknown". Genau dafür gibt es in der Produktion [`commit_identity`].
    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]).unwrap();
        git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@test.de",
                "commit",
                "-q",
                "-m",
                message,
                "--allow-empty",
            ],
        )
        .unwrap();
    }

    /// Hängt das Repo `origin` als Submodule unter `path` an `super_dir` an.
    /// `protocol.file.allow=always` ist für lokale URLs nötig.
    fn add_submodule(super_dir: &Path, origin: &Path, path: &str) {
        git(
            super_dir,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                origin.to_str().unwrap(),
                path,
            ],
        )
        .unwrap();
    }

    /// Bevölkert ein Submodule inklusive verschachtelter Submodule.
    fn populate_submodules(dir: &Path) {
        git(
            dir,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
        )
        .unwrap();
    }

    /// Ersetzt eine URL in der `.gitmodules` von `dir`.
    fn set_gitmodules_url(dir: &Path, old: &str, new: &str) {
        let file = dir.join(".gitmodules");
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains(old),
            "alte URL nicht in {}",
            file.display()
        );
        std::fs::write(&file, content.replace(old, new)).unwrap();
    }

    /// Haupt-Repo mit einem lokalen Submodule (`sub`) und einem Bare-Repo als
    /// „Server". Liefert `(root, base)`: `root` ist das Haupt-Repo, `base` das
    /// gemeinsame Temp-Verzeichnis (zum Aufräumen, enthält `origin.git`).
    fn test_repo_with_submodule() -> Option<(PathBuf, PathBuf)> {
        let (root, _) = test_repo()?;
        let base = unique_temp("submodule");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // „Server"-Repo mit einem Commit.
        let src = base.join("src");
        init_repo(&src);
        std::fs::write(src.join("lib.txt"), "v1\n").unwrap();
        commit_all(&src, "v1");
        let origin = base.join("origin.git");
        git(&base, &["init", "-q", "--bare", origin.to_str().unwrap()]).unwrap();
        git(
            &src,
            &["push", "-q", origin.to_str().unwrap(), "HEAD:master"],
        )
        .unwrap();

        add_submodule(&root, &origin, "sub");
        commit_all(&root, "add submodule");
        populate_submodules(&root);
        Some((root, base))
    }

    /// Nicht existierender Pfad als „toter Server".
    fn dead_path(base: &Path, name: &str) -> String {
        base.join(name).to_str().unwrap().to_string()
    }

    /// Schaltet den „Server" ab: `.gitmodules` UND die effektive Config-URL
    /// zeigen auf `dead` – einen nicht existierenden Pfad, eine unerreichbare
    /// URL oder eine relative URL. Damit kann ein Klon nur noch aus den lokalen
    /// Gitdaten der Hauptinstanz gelingen.
    fn break_submodule_url(dir: &Path, name: &str, dead: &str) {
        let old = git(dir, &["config", "--get", &format!("submodule.{name}.url")]).unwrap();
        let old = old.trim().to_string();
        set_gitmodules_url(dir, &old, dead);
        git(dir, &["config", &format!("submodule.{name}.url"), dead]).unwrap();
        commit_all(dir, "submodule url -> dead");
    }

    macro_rules! skip_if_no_git {
        ($repo:expr) => {
            match $repo {
                Some(v) => v,
                None => {
                    eprintln!("git nicht verfügbar – Test übersprungen");
                    return;
                }
            }
        };
    }

    #[test]
    fn discover_findet_repo() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        let rm = RepoManager::discover(&sub).expect("discover");
        assert_eq!(rm.repo_root(), root);
        cleanup(&dir);
    }

    #[test]
    fn discover_aus_worktree_findet_hauptrepo() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();

        // Worktree anlegen
        let wt = rm
            .create_worktree("feat-discover", None)
            .expect("create_worktree");
        assert!(wt.path.exists());

        // Discover aus dem Worktree heraus sollte das Haupt-Repo finden
        let rm2 = RepoManager::discover(&wt.path).expect("discover aus worktree");
        assert_eq!(
            rm2.repo_root(),
            root,
            "discover aus einem Worktree muss das Haupt-Repo zurückgeben"
        );

        rm.remove_worktree("feat-discover", true).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn discover_fehlt_fehler() {
        let dir = std::env::temp_dir().join(format!("aidev-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(RepoManager::discover(&dir).is_err());
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_legt_branch_und_verzeichnis_an() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm.create_worktree("feat-x", None).expect("create_worktree");

        assert!(wt.path.exists(), "Worktree-Verzeichnis existiert");
        assert_eq!(wt.branch, "feat-x");

        let branches = git(&root, &["branch", "--list", "feat-x"]).unwrap();
        assert!(branches.contains("feat-x"));

        rm.remove_worktree("feat-x", true).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_klappt_auch_bei_schmutzigem_tree() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();

        // Ungetrackte Datei = dreckiger Tree. `git worktree add` fasst die
        // Quelle nicht an → Anlegen muss trotzdem klappen.
        std::fs::write(root.join("dirty.txt"), "x").unwrap();

        let wt = rm
            .create_worktree("feat-dirty", None)
            .expect("create_worktree trotz schmutzigem Tree");

        // Der neue Worktree startet vom Branch-Tip OHNE die Änderungen der
        // Quelle (keine Kopie – dafür gibt es create_worktree_with_stash).
        assert!(
            !wt.path.join("dirty.txt").exists(),
            "Schmutz wird nicht in den neuen Worktree kopiert"
        );
        // Die Quelle behält ihre Änderungen unverändert.
        assert!(
            root.join("dirty.txt").exists(),
            "Quelle bleibt unangetastet"
        );
        assert!(!rm.is_clean(&root).unwrap());

        rm.remove_worktree("feat-dirty", true).unwrap();
        std::fs::remove_file(root.join("dirty.txt")).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_with_stash_funktioniert() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();

        // Getrackte Änderung UND ungetrackte Datei – genau die Kombination,
        // an der ein plain `git stash` scheiterte: ungetrackte Dateien blieben
        // liegen und die Sauberkeitsprüfung schlug trotzdem fehl.
        std::fs::write(root.join("README.md"), "# geändert\n").unwrap();
        std::fs::write(root.join("neu.txt"), "ungetrackt\n").unwrap();

        let wt = rm
            .create_worktree_with_stash("feat-stash", None)
            .expect("create_worktree_with_stash");

        // Beide Änderungen sind im neuen Worktree angekommen …
        assert_eq!(
            std::fs::read_to_string(wt.path.join("README.md")).unwrap(),
            "# geändert\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.path.join("neu.txt")).unwrap(),
            "ungetrackt\n"
        );
        // … und die Quelle wurde vollständig wiederhergestellt (inkl. der
        // ungetrackten Datei).
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "# geändert\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("neu.txt")).unwrap(),
            "ungetrackt\n"
        );
        // Kein Rückstand auf dem Stash-Stack.
        assert!(stash_top(&root).is_none(), "Stash-Stack muss leer sein");

        rm.remove_worktree("feat-stash", true).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_with_stash_erasst_fremde_stash_eintraege() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();

        // Vorbereitung: ein älterer, „fremder“ Stash-Eintrag des Users.
        std::fs::write(root.join("README.md"), "# alt\n").unwrap();
        git(&root, &["stash", "push", "--message", "user-alt"]).unwrap();
        assert!(
            stash_top(&root).is_some(),
            "Vorbereitung: fremder Stash-Eintrag existiert"
        );

        // Neue Arbeit, dann Worktree-Erzeugung über aidev.
        std::fs::write(root.join("README.md"), "# neu\n").unwrap();
        let wt = rm
            .create_worktree_with_stash("feat-stash2", None)
            .expect("create_worktree_with_stash");

        // Die neue Änderung landet im neuen Worktree und wird in der Quelle
        // wiederhergestellt – ohne den fremden Eintrag anzufassen.
        assert_eq!(
            std::fs::read_to_string(wt.path.join("README.md")).unwrap(),
            "# neu\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("README.md")).unwrap(),
            "# neu\n"
        );
        let list = git(&root, &["stash", "list"]).unwrap();
        assert!(
            list.contains("user-alt"),
            "fremder Stash-Eintrag muss erhalten bleiben: {list}"
        );
        assert!(
            !list.contains("aidev-worktree-feat-stash2"),
            "eigener Eintrag muss aufgeräumt sein: {list}"
        );

        rm.remove_worktree("feat-stash2", true).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn is_clean_erkennt_sauberen_tree() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();
        assert!(rm.is_clean(&root).unwrap());

        std::fs::write(root.join("dirty.txt"), "x").unwrap();
        assert!(!rm.is_clean(&root).unwrap());

        std::fs::remove_file(root.join("dirty.txt")).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn remove_worktree_loescht_verzeichnis_und_branch() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();
        rm.create_worktree("feat-del", None).unwrap();
        assert!(rm.worktrees_base().join("feat-del").exists());

        rm.remove_worktree("feat-del", true).unwrap();
        assert!(!rm.worktrees_base().join("feat-del").exists());

        let branches = git(&root, &["branch", "--list", "feat-del"]).unwrap();
        assert!(branches.trim().is_empty());
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_ohne_submodule_meldet_keinen_hinweis() {
        let (dir, root) = skip_if_no_git!(test_repo());
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm.create_worktree("feat-plain", None).unwrap();
        assert!(
            wt.notice.is_none(),
            "Repo ohne Submodule darf nichts melden: {:?}",
            wt.notice
        );
        rm.remove_worktree("feat-plain", true).unwrap();
        cleanup(&dir);
    }

    #[test]
    fn create_worktree_checkt_submodule_lokal_aus() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // „Server" abschalten: der Klon kann nur noch aus den lokalen Gitdaten
        // der Hauptinstanz gelingen.
        break_submodule_url(&root, "sub", &dead_path(&base, "dead.git"));
        let rm = RepoManager::discover(&root).unwrap();

        let wt = rm
            .create_worktree("feat-sub", None)
            .expect("create_worktree");

        // Kern der Sache: `git worktree add` ließe `sub/` leer, `git status`
        // meldete trotzdem sauber – hier muss der Inhalt da sein.
        assert!(
            wt.path.join("sub/lib.txt").exists(),
            "Submodule muss im neuen Worktree ausgecheckt sein"
        );
        assert_eq!(
            git(&wt.path.join("sub"), &["rev-parse", "--short", "HEAD"])
                .unwrap()
                .trim()
                .len(),
            7,
            "Submodule-Repo im Worktree muss funktionieren"
        );
        // Sauber UND bevölkert – genau der Zustand, der vorher still kaputt war.
        assert!(rm.is_clean(&wt.path).unwrap());

        let notice = wt.notice.clone().unwrap_or_default();
        assert!(notice.contains("1"), "Anzahl fehlt: {notice}");
        assert!(
            notice.contains("local data of the main worktree"),
            "Hinweis muss den lokalen Klon ausweisen: {notice}"
        );

        // Der Gitdir liegt pro Worktree (getrennt von dem der Hauptinstanz).
        assert!(
            root.join(".git/worktrees/feat-sub/modules/sub").is_dir(),
            "Submodule-Gitdir muss worktree-eigen sein"
        );

        rm.remove_worktree("feat-sub", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    /// Wie [`test_repo_with_submodule`], hängt aber in `sub` ein weiteres
    /// Submodule `nested` an – also eine echte zweite Ebene.
    fn test_repo_with_nested_submodule() -> Option<(PathBuf, PathBuf)> {
        let (root, base) = test_repo_with_submodule()?;

        let nested_origin = base.join("nested.git");
        let nested_src = base.join("nested-src");
        init_repo(&nested_src);
        std::fs::write(nested_src.join("deep.txt"), "v1\n").unwrap();
        commit_all(&nested_src, "deep v1");
        git(
            &base,
            &["init", "-q", "--bare", nested_origin.to_str().unwrap()],
        )
        .unwrap();
        git(
            &nested_src,
            &["push", "-q", nested_origin.to_str().unwrap(), "HEAD:master"],
        )
        .unwrap();

        let parent_wt = root.join("sub");
        add_submodule(&parent_wt, &nested_origin, "nested");
        commit_all(&parent_wt, "add nested");
        // Neuen Parent-Commit als Submodule-Version im Haupt-Repo übernehmen.
        commit_all(&root, "bump sub");
        Some((root, base))
    }

    /// `git worktree remove` ohne `--force` – genau das, was beim Schließen
    /// passiert, wenn der Worktree bevölkerte Submodule hat.
    #[test]
    fn worktree_remove_ohne_force_scheitert_an_bevoelkerten_submodulen() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm
            .create_worktree("feat-rm", None)
            .expect("create_worktree");
        assert!(wt.path.join("sub/lib.txt").exists());

        // Git verweigert das Entfernen grundsätzlich, sobald Submodule
        // bevölkert sind – unabhängig davon, ob sie sauber sind.
        let err = git_worktree_remove(&root, &wt.path, false).unwrap_err();
        assert!(
            err.contains("submodules"),
            "Git muss sich über Submodule beschweren: {err}"
        );
        // Mit `--force` (so rufen alle aidev-Aufräumpfade auf) klappt es,
        // inklusive der worktree-eigenen Submodule-Gitdirs.
        git_worktree_remove(&root, &wt.path, true).unwrap();
        assert!(!wt.path.exists());
        assert!(
            !root.join(".git/worktrees/feat-rm").exists(),
            "worktree-eigener Submodule-Gitdir muss mit entfernt werden"
        );

        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn git_commit_all_erfasst_aenderungen_im_submodule() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm
            .create_worktree("feat-snap", None)
            .expect("create_worktree");

        // Änderung im Superprojekt UND im Submodule. Die Submodule-Änderung
        // lässt den Gitlink unverändert – ohne den rekursiven Snapshot stünde
        // sie in keinem Commit und wäre beim Löschen verloren.
        std::fs::write(wt.path.join("README.md"), "# neu\n").unwrap();
        std::fs::write(wt.path.join("sub/lib.txt"), "v2\n").unwrap();

        assert!(git_commit_all(&wt.path, "Last Snapshot").unwrap());

        // Nichts bleibt uncommittet …
        assert_eq!(
            git(&wt.path, &["status", "--porcelain"]).unwrap().trim(),
            "",
            "nach dem Snapshot darf nichts offen sein"
        );
        assert!(rm.is_clean(&wt.path).unwrap());
        // … der Inhalt ist aus dem Submodule-Commit rekonstruierbar …
        assert_eq!(
            git(&wt.path.join("sub"), &["show", "HEAD:lib.txt"]).unwrap(),
            "v2\n"
        );
        // … und der Gitlink des Superprojekts zeigt auf den neuen Commit.
        let sub_head = git(&wt.path.join("sub"), &["rev-parse", "HEAD"]).unwrap();
        let gitlink = git(&wt.path, &["ls-tree", "HEAD", "sub"]).unwrap();
        assert!(
            gitlink.contains(sub_head.trim()),
            "Gitlink muss den neuen Submodule-Commit nennen: {gitlink}"
        );

        rm.remove_worktree("feat-snap", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn git_commit_all_erfasst_verschachtelte_submodule() {
        let Some((root, base)) = test_repo_with_nested_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm
            .create_worktree("feat-snap-nested", None)
            .expect("create_worktree");

        // Nur die TIEFSTE Ebene anfassen: Nur wenn von innen nach außen
        // committet wird, ziehen die Gitlinks der äußeren Ebenen mit.
        std::fs::write(wt.path.join("sub/nested/deep.txt"), "v2\n").unwrap();

        assert!(git_commit_all(&wt.path, "Last Snapshot").unwrap());
        assert!(
            rm.is_clean(&wt.path).unwrap(),
            "alle Ebenen müssen sauber sein: {}",
            rm.status_summary(&wt.path).unwrap()
        );

        // Beide Gitlinks zeigen auf die jeweils neue Commit-Version.
        let nested_head = git(&wt.path.join("sub/nested"), &["rev-parse", "HEAD"]).unwrap();
        let inner_gitlink = git(&wt.path.join("sub"), &["ls-tree", "HEAD", "nested"]).unwrap();
        assert!(
            inner_gitlink.contains(nested_head.trim()),
            "innerer Gitlink veraltet: {inner_gitlink}"
        );
        let sub_head = git(&wt.path.join("sub"), &["rev-parse", "HEAD"]).unwrap();
        let outer_gitlink = git(&wt.path, &["ls-tree", "HEAD", "sub"]).unwrap();
        assert!(
            outer_gitlink.contains(sub_head.trim()),
            "äußerer Gitlink veraltet: {outer_gitlink}"
        );

        rm.remove_worktree("feat-snap-nested", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn git_commit_all_ohne_aenderungen_meldet_nichts_zu_tun() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // Frischer Worktree ist sauber → kein Commit, auch nicht in Submodulen.
        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm
            .create_worktree("feat-clean", None)
            .expect("create_worktree");
        assert!(!git_commit_all(&wt.path, "Snapshot").unwrap());

        rm.remove_worktree("feat-clean", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn create_worktree_checkt_submodule_bei_toter_pfad_url_aus() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // Regressionstest: Die Quell-URL ist ein *absoluter Pfad*. Der frühere
        // `url.<lokal>.insteadOf=<url>`-Mechanismus griff hier NICHT – git
        // normalisiert pfad-artige URLs vor dem Text-Match, der Klon ging
        // deshalb auf die tote URL. Der Name-Override ersetzt die URL
        // vollständig und funktioniert für jede URL-Form.
        break_submodule_url(&root, "sub", &dead_path(&base, "dead.git"));
        let rm = RepoManager::discover(&root).unwrap();

        let wt = rm
            .create_worktree("feat-path", None)
            .expect("create_worktree");

        assert!(
            wt.path.join("sub/lib.txt").exists(),
            "trotz Pfad-URL muss lokal geklont werden"
        );
        let notice = wt.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("local data of the main worktree"),
            "Hinweis muss den lokalen Klon ausweisen: {notice}"
        );

        rm.remove_worktree("feat-path", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn create_worktree_checkt_submodule_bei_relativer_url_aus() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // Regressionstest: relative URL (`../…`) – in der Praxis der Normalfall.
        // Git löst sie gegen die Origin-URL des Superprojekts auf, ein Klon
        // bräuchte also den Server; für den Text-Match von `insteadOf` war sie
        // ohnehin unbrauchbar.
        break_submodule_url(&root, "sub", "../dead.git");
        let rm = RepoManager::discover(&root).unwrap();

        let wt = rm
            .create_worktree("feat-relative", None)
            .expect("create_worktree");

        assert!(
            wt.path.join("sub/lib.txt").exists(),
            "trotz relativer URL muss lokal geklont werden"
        );
        assert!(rm.is_clean(&wt.path).unwrap());

        rm.remove_worktree("feat-relative", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn mehrdeutige_submodulnamen_bekommen_keinen_override() {
        let src = |name: &str, local: &str| SubmoduleSource {
            name: name.to_string(),
            local: PathBuf::from(local),
        };

        // Eindeutige Namen bleiben unangetastet.
        let out = unambiguous_sources(vec![src("a", "/a"), src("b", "/b")]);
        assert_eq!(out.len(), 2);

        // Gleicher Name, gleicher Gitdir: ein Override genügt.
        let out = unambiguous_sources(vec![src("a", "/a"), src("a", "/a")]);
        assert_eq!(out.len(), 1);

        // Gleicher Name auf zwei Ebenen mit verschiedenen Gitdirs: Die Config
        // kennt keine Ebene, also lieber gar kein Override (Original-URL) als
        // die falsche Quelle.
        let out = unambiguous_sources(vec![src("a", "/a"), src("a", "/anders")]);
        assert!(
            out.is_empty(),
            "mehrdeutiger Name darf keinen Override bekommen"
        );
    }

    #[test]
    fn create_worktree_checkt_verschachtelte_submodule_aus() {
        let Some((root, base)) = test_repo_with_nested_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        let parent_wt = root.join("sub");

        // Beide Ebenen auf tote URLs umbiegen (je Ebene eine eigene, sonst
        // kollidieren die `insteadOf`-Regeln).
        break_submodule_url(&parent_wt, "nested", &dead_path(&base, "dead-nested.git"));
        break_submodule_url(&root, "sub", &dead_path(&base, "dead-sub.git"));

        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm
            .create_worktree("feat-nested", None)
            .expect("create_worktree");

        assert!(
            wt.path.join("sub/lib.txt").exists(),
            "erste Ebene muss ausgecheckt sein"
        );
        assert!(
            wt.path.join("sub/nested/deep.txt").exists(),
            "verschachteltes Submodule muss ebenfalls ausgecheckt sein"
        );

        let notice = wt.notice.clone().unwrap_or_default();
        assert!(notice.contains('2'), "beide Module erwartet: {notice}");
        assert!(
            notice.contains("local data of the main worktree"),
            "Hinweis muss den lokalen Klon ausweisen: {notice}"
        );

        rm.remove_worktree("feat-nested", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn create_worktree_ohne_submodule_im_stand_meldet_nichts() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // Branch auf den Stand VOR dem Submodule zeigen lassen: Die
        // Hauptinstanz hat eine `.gitmodules` und einen bevölkerten Gitdir,
        // der ausgecheckte Stand aber keine Submodule. Dann darf nichts
        // gemeldet werden – die Zählung orientiert sich am Stand, nicht am
        // Bestand der Hauptinstanz.
        git(&root, &["branch", "nosub", "HEAD~1"]).unwrap();
        assert!(
            root.join(".gitmodules").is_file(),
            "Voraussetzung: Bestand da"
        );

        let rm = RepoManager::discover(&root).unwrap();
        let wt = rm.create_worktree("nosub", None).expect("create_worktree");

        assert!(!wt.path.join("sub").exists());
        assert!(
            wt.notice.is_none(),
            "ohne Submodule im Stand darf nichts gemeldet werden: {:?}",
            wt.notice
        );

        rm.remove_worktree("nosub", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn create_worktree_holt_submodule_bei_fehlendem_lokalen_bestand_vom_server() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        // Hauptinstanz „nicht bevölkert": kein lokaler Gitdir als Klon-Quelle.
        // Die URL ist erreichbar → der reguläre Weg über den Server muss
        // greifen und die Herkunft auch so gemeldet werden.
        cleanup(&root.join(".git/modules/sub"));
        cleanup(&root.join("sub"));
        let rm = RepoManager::discover(&root).unwrap();

        let wt = rm
            .create_worktree("feat-server", None)
            .expect("create_worktree");

        assert!(
            wt.path.join("sub/lib.txt").exists(),
            "ohne lokalen Bestand muss über die URL geklont werden"
        );
        let notice = wt.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("configured URLs"),
            "Herkunft muss als Server-Klon ausgewiesen werden: {notice}"
        );

        rm.remove_worktree("feat-server", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn create_worktree_meldet_fehlgeschlagene_submodule_ohne_abbruch() {
        let Some((root, base)) = test_repo_with_submodule() else {
            eprintln!("git nicht verfügbar – Test übersprungen");
            return;
        };
        break_submodule_url(&root, "sub", &dead_path(&base, "dead.git"));
        // Hauptinstanz NICHT bevölkert: kein lokaler Gitdir als Klon-Quelle,
        // die tote URL macht auch den Server-Weg unmöglich.
        cleanup(&root.join(".git/modules/sub"));
        cleanup(&root.join("sub"));
        let rm = RepoManager::discover(&root).unwrap();

        let wt = rm
            .create_worktree("feat-broken", None)
            .expect("Worktree muss trotz fehlender Submodule angelegt werden");

        assert!(
            !wt.path.join("sub/lib.txt").exists(),
            "Submodule bleibt in diesem Fall leer"
        );
        let notice = wt.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("NOT checked out"),
            "leerer Submodule-Ordner muss gemeldet werden: {notice}"
        );
        assert!(
            notice.contains("submodule update --init"),
            "Hinweis muss den Ausweg nennen: {notice}"
        );

        rm.remove_worktree("feat-broken", true).unwrap();
        cleanup(&root);
        cleanup(&base);
    }

    #[test]
    fn copy_plan_hardlink_funktioniert() {
        let src_dir = std::env::temp_dir().join(format!("aidev-cpsrc-{}", std::process::id()));
        let dst_dir = std::env::temp_dir().join(format!("aidev-cpdst-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dst_dir);
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), "inhalt-a").unwrap();
        std::fs::create_dir_all(src_dir.join("sub")).unwrap();
        std::fs::write(src_dir.join("sub/b.txt"), "inhalt-b").unwrap();

        let plan = CopyPlan {
            entries: vec![CopyEntry {
                source: ".".to_string(),
                strategy: CopyStrategy::Hardlink,
            }],
        };

        let rm = RepoManager {
            repo_root: PathBuf::from("/nonexistent"),
        };
        let stats = rm
            .execute_copy_plan(&src_dir, &dst_dir, &plan)
            .expect("copy");

        assert!(dst_dir.join("a.txt").exists());
        assert!(dst_dir.join("sub/b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dst_dir.join("a.txt")).unwrap(),
            "inhalt-a"
        );
        assert!(stats.files_linked + stats.files_copied > 0);

        cleanup(&src_dir);
        cleanup(&dst_dir);
    }

    #[test]
    fn copy_plan_full_funktioniert() {
        let src_dir = std::env::temp_dir().join(format!("aidev-cpsrc2-{}", std::process::id()));
        let dst_dir = std::env::temp_dir().join(format!("aidev-cpdst2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("data.bin"), "x".repeat(1000)).unwrap();

        let plan = CopyPlan {
            entries: vec![CopyEntry {
                source: ".".to_string(),
                strategy: CopyStrategy::Full,
            }],
        };

        let rm = RepoManager {
            repo_root: PathBuf::from("/nonexistent"),
        };
        let stats = rm
            .execute_copy_plan(&src_dir, &dst_dir, &plan)
            .expect("copy");

        assert!(dst_dir.join("data.bin").exists());
        assert_eq!(stats.files_linked, 0);
        assert_eq!(stats.files_copied, 1);

        cleanup(&src_dir);
        cleanup(&dst_dir);
    }

    #[test]
    fn copy_plan_ueberspringt_fehlende_verzeichnisse() {
        let src_dir = std::env::temp_dir().join(format!("aidev-cpsrc3-{}", std::process::id()));
        let dst_dir = std::env::temp_dir().join(format!("aidev-cpdst3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&dst_dir);
        std::fs::create_dir_all(&src_dir).unwrap();

        let plan = CopyPlan {
            entries: vec![CopyEntry {
                source: "target".to_string(),
                strategy: CopyStrategy::Hardlink,
            }],
        };

        let rm = RepoManager {
            repo_root: PathBuf::from("/nonexistent"),
        };
        let stats = rm
            .execute_copy_plan(&src_dir, &dst_dir, &plan)
            .expect("copy");

        assert_eq!(stats.files_linked, 0);
        assert_eq!(stats.files_copied, 0);

        cleanup(&src_dir);
        cleanup(&dst_dir);
    }
}
