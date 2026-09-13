//! Referenz-/Test-Implementierung: Läuft direkt auf dem Host.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::fsops;
use super::glob;
use super::resolve::resolve;
use super::run::run_with_timeout_live;
use super::search::search_files;
use super::{Channel, ChannelKind, ChannelStatus, RunOut, SearchResult};

/// Läuft direkt auf dem Host. Zugleich Reference-Implementierung des
/// [`Channel`]-Traits und realer Kanal (`type = "local"` in der Config).
pub struct Local {
    root: PathBuf,
    timeout: Duration,
    shell: Mutex<Option<String>>,
    worktree: Option<crate::repo::WorktreeInfo>,
    /// Ob der gemountete Host-Ordner ein von aidev angelegter Git-Worktree ist
    /// (Picker-Branch ohne Worktree, `/branch`, `Alt+D`). In der Regel `false`;
    /// nur beim Erzeugen des Kanals gesetzt, über die Lebensdauer unverändert.
    managed_worktree: bool,
}

impl Local {
    pub fn new(root: PathBuf) -> Self {
        Local {
            root,
            timeout: Duration::from_secs(60),
            shell: Mutex::new(None),
            worktree: None,
            managed_worktree: false,
        }
    }

    pub fn with_worktree(mut self, wt: crate::repo::WorktreeInfo) -> Self {
        // Ein gebundener Worktree wurde von aidev angelegt → als verwaltet
        // markieren (aufräumen beim Schließen).
        self.worktree = Some(wt);
        self.managed_worktree = true;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Channel for Local {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Local
    }

    fn root(&self) -> String {
        format!("Local:{}", self.root.display())
    }

    fn read(&self, rel: &Path) -> Result<String, String> {
        fsops::read_at(&resolve(&self.root, rel)?)
    }

    fn write(&self, rel: &Path, content: &str) -> Result<(), String> {
        fsops::write_at(&resolve(&self.root, rel)?, content)
    }

    fn grep(
        &self,
        pattern: &str,
        rel: &Path,
        include: Option<&str>,
        context_lines: usize,
    ) -> Result<SearchResult, String> {
        let dir = resolve(&self.root, rel)?;
        search_files(pattern, &dir, self.timeout, include, context_lines)
    }

    fn glob(&self, pattern: &str, rel: &Path) -> Result<Vec<String>, String> {
        let dir = resolve(&self.root, rel)?;
        glob::find_glob(pattern, &dir, self.timeout)
    }

    fn run(&self, cmd: &str, args: &[String], rel_cwd: &Path) -> Result<RunOut, String> {
        let mut discard = |_: &str| {};
        self.run_live(cmd, args, rel_cwd, &mut discard, None)
    }

    fn run_live(
        &self,
        cmd: &str,
        args: &[String],
        rel_cwd: &Path,
        live: &mut dyn FnMut(&str),
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<RunOut, String> {
        let cwd = resolve(&self.root, rel_cwd)?;
        run_with_timeout_live(cmd, args, &cwd, self.timeout, Some(live), cancel)
    }

    fn shell(&self) -> Result<String, String> {
        if let Some(shell) = self.shell.lock().map(|g| g.clone()).unwrap_or(None) {
            return Ok(shell);
        }
        let shell = if self
            .run(
                "bash",
                &["-c".to_string(), "command -v bash".to_string()],
                Path::new("."),
            )
            .is_ok_and(|p| p.exit_code == Some(0))
        {
            "bash".to_string()
        } else {
            "sh".to_string()
        };
        if let Ok(mut g) = self.shell.lock() {
            *g = Some(shell.clone());
        }
        Ok(shell)
    }

    fn dup(&self) -> Result<Arc<dyn Channel>, String> {
        // Lokales Duplikat (Alt+D): gleicher Ordner, kein eigener Container und
        // – anders als beim Podman-Duplikat – KEIN neuer Worktree angelegt. Der
        // Duplikat-Kanal übernimmt daher die Worktree-Verwaltung nicht: Schließt
        // er, darf der (vom Original genutzte) Ordner nicht aufgeräumt werden.
        Ok(Arc::new(Local {
            root: self.root.clone(),
            timeout: self.timeout,
            shell: Mutex::new(None),
            worktree: None,
            managed_worktree: false,
        }))
    }

    fn host_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn owned_worktree(&self) -> Option<&crate::repo::WorktreeInfo> {
        self.worktree.as_ref()
    }

    fn managed_worktree(&self) -> bool {
        self.managed_worktree
    }

    fn status(&self) -> ChannelStatus {
        ChannelStatus::Running
    }
}
