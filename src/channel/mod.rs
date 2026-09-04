//! Kanal-Schnittstelle zum Dateisystem/der Shell.
//!
//! Ein **Channel** kapselt, wo und wie Datei-Operationen und Shell-Befehle
//! ausgeführt werden – die Schnittstelle, über die das LLM
//! Suchen/Lesen/Editieren/Befehle erledigt. Alle Pfade sind relativ zur
//! Kanal-Wurzel; Ausbrechen nach oben (`..`) wird abgewiesen, und auch
//! Symlinks dürfen die Wurzel nicht verlassen.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{ChannelConfig, PodmanUserMapping};

pub(crate) mod builder;
mod container;
mod fsops;
mod glob;
pub(crate) mod local;
pub(crate) mod podman;
mod resolve;
pub(crate) mod run;
mod search;

pub use local::Local;
// Nur die Tests in dieser Datei nutzen den Namen direkt; app.rs greift über
// `crate::channel::podman::PodmanChannel` zu.
#[cfg(test)]
pub use podman::PodmanChannel;

/// Art bzw. Modus eines Kanals – für die UI (Status, Auswahl).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    Local,
    PodmanAttach,
    PodmanRun,
}

/// Modus eines Podman-Kanals (Attach vs. Run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodmanMode {
    Attach,
    Run,
}

/// Treffer der Volltextsuche (Kanal-relativer Pfad + Treffertext; die
/// Zeilennummer steckt im Kontext-Modus im rohen Passthrough).
#[derive(Debug, Clone)]
pub struct Match {
    pub path: String,
    pub text: String,
}

/// Ergebnis der Volltextsuche.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub matches: Vec<Match>,
    pub note: Option<String>,
    /// Bei Kontext-Suche (`grep` mit `content` > 0): der unverarbeitete
    /// Ausdruck von rg/grep – Trefferzeilen (`pfad:zeile:text`), Kontextzeilen
    /// (`pfad-zeile-text`) und „--“-Gruppentrenner. In diesem Modus ist
    /// `matches` leer; das Rendering reicht `raw` direkt durch.
    pub raw: Option<String>,
}

/// Ergebnis eines Kommando-Laufs.
#[derive(Debug, Clone)]
pub struct RunOut {
    /// `None` bedeutet Timeout oder abgebrochen.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Die Kanal-Schnittstelle.
pub trait Channel: Send + Sync {
    fn kind(&self) -> ChannelKind;
    fn root(&self) -> String;
    fn read(&self, rel: &Path) -> Result<String, String>;
    fn write(&self, rel: &Path, content: &str) -> Result<(), String>;
    /// Datei-Pattern-Suche (`glob`-Werkzeug) über `find`.
    fn glob(&self, pattern: &str, rel: &Path) -> Result<Vec<String>, String>;
    /// Volltextsuche (`grep`-Werkzeug). `include` filtert nach Glob-Muster
    /// (z. B. `*.rs`), `context_lines` > 0 liefert rohe Kontextblöcke
    /// (`SearchResult::raw`) statt Einzeltreffer.
    fn grep(
        &self,
        pattern: &str,
        rel: &Path,
        include: Option<&str>,
        context_lines: usize,
    ) -> Result<SearchResult, String>;
    fn run(&self, cmd: &str, args: &[String], rel_cwd: &Path) -> Result<RunOut, String>;
    fn run_live(
        &self,
        cmd: &str,
        args: &[String],
        rel_cwd: &Path,
        live: &mut dyn FnMut(&str),
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<RunOut, String> {
        let _ = live;
        let _ = cancel;
        self.run(cmd, args, rel_cwd)
    }
    fn shell(&self) -> Result<String, String> {
        Ok("sh".to_string())
    }
    fn dup(&self) -> Result<Arc<dyn Channel>, String>;
    /// Host-Pfad des Workspace (Local: Dateisystem-Pfad; Podman Run: gemountetes
    /// Verzeichnis). Attach-Modus liefert `None`.
    fn host_root(&self) -> Option<PathBuf> {
        None
    }
    /// Falls dieser Channel einen aidev-eigenen Worktree verwaltet, liefert
    /// die Metadaten – wird beim Schließen des Channels zum Aufräumen genutzt.
    fn owned_worktree(&self) -> Option<&crate::repo::WorktreeInfo> {
        None
    }
    fn status(&self) -> ChannelStatus {
        ChannelStatus::Unknown
    }
    fn probe(&self) {}
    fn warmup(&self) {}
    fn label(&self) -> String {
        self.root()
    }
    fn essential_changes(&self) -> Option<Vec<String>> {
        None
    }
    /// Podman-Containername dieses Kanals – falls aidev dafür einen eigenen
    /// Container startet/verwaltet (`Run`-Modus). Dient der Anzeige im
    /// Beenden-Dialog, damit der betroffene Container direkt benannt wird.
    /// Kanäle ohne eigenen Container (Local, Attach) liefern `None`.
    fn container_name(&self) -> Option<String> {
        None
    }
    /// Liefert `(image, workdir, home)` falls dieser Kanal ein Podman-`Run`-
    /// Kanal ist. Wird von `/branch` genutzt, um einen gleich konfigurierten
    /// Folge-Kanal zu erzeugen – auch für vom ChannelBuilder erzeugte Kanäle,
    /// die nicht in der statischen Konfiguration (`Config.channels`) stehen.
    /// Alle anderen Kanäle liefern `None`.
    fn podman_run_spec(&self) -> Option<(String, String, String)> {
        None
    }
}

/// Zustand eines Kanals für den `⬢`-Indikator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStatus {
    Unknown,
    Starting,
    Running,
    Problem,
}

/// Geteilter Zustand für verwaltete Container.
type Managed = Arc<Mutex<Vec<String>>>;

/// Zentrale Verwaltung der konfigurierten Kanäle.
pub struct ChannelRegistry {
    pub(crate) default: Option<String>,
    pub(crate) map: HashMap<String, Arc<dyn Channel>>,
    pub(crate) managed: Managed,
}

impl ChannelRegistry {
    pub fn new(cfg: &crate::config::Config) -> Self {
        let managed: Managed = Arc::new(Mutex::new(Vec::new()));
        let mut map: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        for (name, cc) in &cfg.channels {
            match channel_from_config(name, cc, cfg.timeout_secs, managed.clone(), None, cfg.podman.usermapping) {
                Ok(ch) => {
                    map.insert(name.clone(), ch);
                }
                Err(err) => eprintln!("[aidev] Channel \"{name}\" not available: {err}"),
            }
        }
        if let Some(d) = &cfg.default_channel {
            if !map.contains_key(d) {
                eprintln!("[aidev] default_channel \"{d}\" not available.");
            }
        }
        let default = cfg.default_channel.clone().filter(|d| map.contains_key(d));
        ChannelRegistry {
            default,
            map,
            managed,
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.map.get(name).cloned()
    }

    /// Sucht einen registrierten Kanal, der den genannten Podman-Container
    /// verwendet (eigener Run-Container). Vom Channel Builder genutzt, um
    /// einen bereits bestehenden Kanal wiederzuverwenden, statt einen
    /// Duplikat-Kanal anzulegen, wenn ein passender Container schon läuft.
    /// Liefert `(Kanalname, Kanal)` des ersten Treffers.
    pub fn find_by_container(&self, container: &str) -> Option<(String, Arc<dyn Channel>)> {
        for (name, ch) in &self.map {
            if ch.container_name().as_deref() == Some(container) {
                return Some((name.clone(), ch.clone()));
            }
        }
        None
    }

    pub fn default_channel_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    pub fn register(&mut self, mut name: String, ch: Arc<dyn Channel>) -> String {
        if self.map.contains_key(&name) {
            let mut n = 2;
            loop {
                let candidate = format!("{name}-{n}");
                if !self.map.contains_key(&candidate) {
                    name = candidate;
                    break;
                }
                n += 1;
            }
        }
        self.map.insert(name.clone(), ch);
        name
    }

    /// Findet den Registry-Namen für einen gegebenen Channel-Arc.
    /// Wird genutzt, um den Namen eines Kanals zu ermitteln, der an einer
    /// Session hängt, ohne den Namen separat an der Session speichern zu
    /// müssen (O(n), aber Kanäle sind wenige).
    pub fn find_name(&self, needle: &Arc<dyn Channel>) -> Option<String> {
        for (name, ch) in &self.map {
            // Arc-Pointer-Vergleich: beide sind dieselbe zugewiesene Arc.
            if Arc::ptr_eq(ch, needle) {
                return Some(name.clone());
            }
        }
        None
    }

    pub fn unregister(&mut self, name: &str) {
        self.map.remove(name);
    }

    pub fn stop_managed(&self) {
        let names: std::collections::HashSet<String> = self
            .managed
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default();
        let handles: Vec<_> = names
            .into_iter()
            .map(|name| {
                std::thread::spawn(move || {
                    let _ = run::run_with_timeout(
                        "podman",
                        &["stop".into(), "-t".into(), "0".into(), name],
                        Path::new("."),
                        Duration::from_secs(30),
                    );
                })
            })
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// Stoppt einen einzelnen, selbst verwalteten Container sofort: Name aus
    /// der `managed`-Liste entfernen und `podman stop -t 0` ausführen. Wird
    /// beim expliziten Schließen eines Kanals genutzt (`finalize_close_channel`);
    /// `stop_managed` räumt am Programmende den Rest auf.
    pub fn stop_one(&self, name: &str) {
        {
            let mut g = self.managed.lock().unwrap_or_else(|p| p.into_inner());
            g.retain(|n| n != name);
        }
        let _ = run::run_with_timeout(
            "podman",
            &["stop".into(), "-t".into(), "0".into(), name.to_string()],
            Path::new("."),
            Duration::from_secs(30),
        );
    }
}

// ---------------------------------------------------------------------------
// Config-Dispatch
// ---------------------------------------------------------------------------

pub fn channel_from_config(
    name: &str,
    cfg: &ChannelConfig,
    timeout_secs: u64,
    managed: Managed,
    worktree: Option<crate::repo::WorktreeInfo>,
    usermapping: PodmanUserMapping,
) -> Result<Arc<dyn Channel>, String> {
    match cfg.kind.as_str() {
        "podman" => Ok(Arc::new(podman::podman_from_config(
            name,
            cfg,
            timeout_secs,
            managed,
            worktree,
            usermapping,
        )?)),
        "local" => local_from_config(name, cfg, timeout_secs, worktree),
        other => Err(format!("Unknown channel type: {other}")),
    }
}

pub fn local_from_config(
    _name: &str,
    cfg: &ChannelConfig,
    timeout_secs: u64,
    worktree: Option<crate::repo::WorktreeInfo>,
) -> Result<Arc<dyn Channel>, String> {
    let root = cfg
        .host_root
        .as_ref()
        .ok_or("local channel needs host_root")?;
    if cfg.image.is_some() || cfg.container.is_some() {
        return Err("local channel must not set image/container (only host_root).".into());
    }
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut ch = Local::new(PathBuf::from(root)).with_timeout(timeout);
    if let Some(wt) = worktree {
        ch = ch.with_worktree(wt);
    }
    Ok(Arc::new(ch))
}

#[cfg(test)]
pub(crate) fn test_registry(
    name: &str,
    root: PathBuf,
    default: Option<&str>,
) -> (ChannelRegistry, Arc<dyn Channel>) {
    let ch: Arc<dyn Channel> = Arc::new(Local::new(root));
    let mut map = HashMap::new();
    map.insert(name.to_string(), ch.clone());
    let registry = ChannelRegistry {
        default: default.map(str::to_string),
        map,
        managed: Arc::new(Mutex::new(Vec::new())),
    };
    (registry, ch)
}

#[cfg(test)]
mod tests;
