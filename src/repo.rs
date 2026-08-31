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
    let out = Command::new("git")
        .args(args)
        .current_dir(run_dir)
        .output()
        .map_err(|e| err(format!("run git {}: {e}", args[0])))?;
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
/// generischen Nachricht. `.aidev/` wird ausgeschlossen.
/// Gibt `true` zurück wenn ein Commit erstellt wurde, `false` wenn nichts zu committen war.
pub(crate) fn git_commit_all(repo: &Path, message: &str) -> Result<bool, String> {
    // Alle Änderungen stagen (respektiert .gitignore, schließt .aidev aus)
    git(repo, &["add", "-A", "--", ":(exclude).aidev"])?;
    // Prüfen ob es tatsächlich was zu committen gibt
    let diff = git(repo, &["diff", "--cached", "--stat"])?;
    if diff.trim().is_empty() {
        return Ok(false);
    }
    // Committen
    git(repo, &["commit", "-m", message])?;
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

        Ok(WorktreeInfo {
            name: branch_name.to_string(),
            branch: branch_name.to_string(),
            path: dest,
        })
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

        let dir = std::env::temp_dir().join(format!("aidev-repo-{}", std::process::id()));
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
