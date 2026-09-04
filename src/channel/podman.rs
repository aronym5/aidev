//! Podman-Container-Kanal (Attach- und Run-Modus).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::container::{
    container_layer_changes, git_worktree_changes, inspect_container_state, podman_probe_status,
    reuse_container, ContainerState,
};
use super::fsops;
use super::glob;
use super::resolve::join_workdir;
use super::resolve::resolve;
use super::run::{host_uid_gid, run_with_timeout, run_with_timeout_live, sanitize};
use super::search::search_files;
use super::{
    Channel, ChannelKind, ChannelStatus, Managed, PodmanMode, RunOut, SearchResult,
};
use crate::config::{ChannelConfig, PodmanUserMapping};
use std::sync::atomic::AtomicBool;

/// Podman-Kanal. Befehle laufen per `podman exec …` (argv, ohne Shell).
/// Datei-Operationen laufen über den Host-Mount (`host_root`, sofern gesetzt).
/// Run-Container werden mit `--init` gestartet; das UID-/GID-Mapping wird über
/// `PodmanUserMapping` gewählt (Default `keep-id`, alternativ explizite
/// `--uidmap`/`--gidmap`).
///
/// Als exec-Identität (und als Anker des UID-Mappings) dient bei `keep-id` die
/// Host-UID/-GID des Aufrufers, bei `uidmap` die im Image konfigurierte
/// Gast-UID/-GID (beim Kanal-Aufbau per `podman run --rm <image> id` erfragt und
/// in den Channel-Membern `uid`/`gid` gemerkt). Beide laufen per `--user <uid>:<gid>`
/// als Nicht-Root; die gemountete Arbeitskopie behält dieselben Rechte wie auf dem
/// Host, und ein Init als PID 1 sammelt verwaiste Kindprozesse sofort ein (keine
/// liegenbleibenden Zombies).
pub struct PodmanChannel {
    pub(crate) name: String,
    pub(crate) mode: PodmanMode,
    pub(crate) container: String,
    pub(crate) workdir: String,
    pub(crate) host_root: Option<PathBuf>,
    pub(crate) image: Option<String>,
    pub(crate) timeout: Duration,
    /// Für exec und UID-Mapping verwendete Identität (Run-Modus: `--user` beim
    /// exec): bei `keep-id` die Host-UID des Aufrufers, bei `uidmap` die im Image
    /// erfragte Gast-UID – im Kanal gemerkt für spätere Container- und exec-Aufrufe.
    pub(crate) uid: u32,
    /// Gegenstück zu `uid` für die Gruppen-ID (GID).
    pub(crate) gid: u32,
    /// Schreibbarer `$HOME` für Werkzeug-Prozesse (Run-Modus).
    pub(crate) home: String,
    /// UID/GID-Mapping beim Container-Start (Run-Modus; aus der Top-Level-Config).
    pub(crate) usermapping: PodmanUserMapping,
    /// Falls dieser Channel einen aidev-eigenen Worktree verwaltet.
    pub(crate) worktree: Option<crate::repo::WorktreeInfo>,
    /// Zähler für Duplikate (eigene Container-/Worktree-Namen).
    pub(crate) seq: AtomicUsize,
    /// Geteilter Zustand für den `⬢`-Indikator (beschrieben von
    /// `ensure_running`/`run`/`probe`; gelesen vom UI auf dem Hauptfaden).
    pub(crate) status: Arc<Mutex<ChannelStatus>>,
    pub(crate) managed: Managed,
    /// Gecachter Shell-Name (`bash`, falls im Container verfügbar, sonst `sh`)
    /// – einmalig beim ersten Shell-Einsatz im Container ermittelt.
    pub(crate) shell: Mutex<Option<String>>,
}

pub(super) fn podman_from_config(
    name: &str,
    cfg: &ChannelConfig,
    timeout_secs: u64,
    managed: Managed,
    worktree: Option<crate::repo::WorktreeInfo>,
    usermapping: PodmanUserMapping,
) -> Result<PodmanChannel, String> {
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let host_root = cfg.host_root.as_ref().map(PathBuf::from);
    let (uid, gid) = if cfg.image.is_some() {
        // Run-Modus: exec-Identität nach Top-Level-Mapping – `keep-id` → Host,
        // `uidmap` → Gast-UID/-GID (beim Kanal-Aufbau aus dem Image erfragt).
        running_uid_gid(cfg.image.as_deref(), usermapping)?
    } else {
        (0, 0)
    };

    let mut channel = if let Some(image) = &cfg.image {
        // Run-Modus: eigener Container (Name abgeleitet), Wurzel ist ein
        // Host-Verzeichnis, das in den Container gemountet wird.
        let host = host_root.ok_or("Run channel needs host_root")?;
        PodmanChannel {
            name: name.to_string(),
            mode: PodmanMode::Run,
            container: cfg
                .run_container
                .clone()
                .unwrap_or_else(|| format!("aidev-{}", sanitize(name))),
            workdir: cfg.workdir.clone(),
            host_root: Some(host),
            image: Some(image.clone()),
            timeout,
            uid,
            gid,
            home: cfg.run_home(),
            usermapping,
            worktree: None,
            seq: AtomicUsize::new(1),
            status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
            managed,
            shell: Mutex::new(None),
        }
    } else if let Some(container) = &cfg.container {
        // Attach-Modus: an bestehende Verbindung andocken (geteilt).
        PodmanChannel {
            name: name.to_string(),
            mode: PodmanMode::Attach,
            container: container.clone(),
            workdir: cfg.workdir.clone(),
            host_root,
            image: None,
            timeout,
            uid: 0,
            gid: 0,
            home: String::new(),
            // Attach-Kanäle starten keinen Container; `KeepId` hält das
            // bestehende Probe-Verhalten (grün nur bei passendem keep-id-NS).
            usermapping: PodmanUserMapping::KeepId,
            worktree: None,
            seq: AtomicUsize::new(1),
            status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
            managed,
            shell: Mutex::new(None),
        }
    } else {
        return Err("Podman-Kanal braucht image (run) oder container (attach)".into());
    };
    if let Some(wt) = worktree {
        channel = channel.with_worktree(wt);
    }
    Ok(channel)
}

/// Für exec-`--user` und UID-Mapping zu verwendende Identität:
/// - `KeepId`: Host-UID/-GID des Aufrufers (bisheriges Verhalten),
/// - `Uidmap`: die im Image konfigurierte Standard-UID/-GID (Gast), die beim
///   Kanal-Aufbau per `podman run --rm <image> id` erfragt wird; das Ergebnis
///   wird als `uid`/`gid` im Channel-Member für spätere Nutzung gemerkt.
pub(crate) fn running_uid_gid(
    image: Option<&str>,
    usermapping: PodmanUserMapping,
) -> Result<(u32, u32), String> {
    match usermapping {
        PodmanUserMapping::KeepId => host_uid_gid(),
        PodmanUserMapping::Uidmap => {
            let img = image.ok_or("uidmap needs an image (run mode)")?;
            image_default_uid_gid(img)
        }
    }
}

/// Ermittelt die Default-UID/-GID des Image-Users über
/// `podman run --rm <image> id` und wertet `uid=`/`gid=` aus.
fn image_default_uid_gid(image: &str) -> Result<(u32, u32), String> {
    let st = run_with_timeout(
        "podman",
        &[
            "run".into(),
            "--rm".into(),
            image.into(),
            "id".into(),
        ],
        Path::new("."),
        Duration::from_secs(120),
    )?;
    if st.exit_code != Some(0) {
        return Err(format!(
            "podman run --rm {image} id failed:\n{}\n{}",
            st.stderr, st.stdout
        ));
    }
    parse_uid_gid_fields(&st.stdout).ok_or_else(|| {
        format!(
            "keine uid=/gid= in der Ausgabe von \"{image} id\": {}",
            st.stdout.trim()
        )
    })
}

/// Liest `uid=`/`gid=` aus der Ausgabe von `id` (z. B. `uid=1000(node) gid=…`).
pub(super) fn parse_uid_gid_fields(out: &str) -> Option<(u32, u32)> {
    fn num(field: &str, out: &str) -> Option<u32> {
        let key = format!("{field}=");
        let start = out.find(&key)? + key.len();
        let digits: String = out[start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    }
    Some((num("uid", out)?, num("gid", out)?))
}

/// Explizite `--uidmap`/`--gidmap`-Flags für die Gast-Identität `(u, g)`: die
/// Gast-UID/-GID wird auf den Host-User gemappt (rootless: „Host 0“), der Rest
/// identisch aufgefüllt (gesamter 16-Bit-UID-Raum). Für `u == 0` (Root-Image)
/// entfällt der vordere Verschiebe-Teilbereich.
pub(super) fn uidmap_args(u: u32, g: u32) -> Vec<String> {
    fn uid_args(v: u32, flag: &str) -> Vec<String> {
        let mut args = Vec::new();
        if v > 0 {
            args.push(format!("{flag}=0:1:{v}"));
        }
        args.push(format!("{flag}={v}:0:1"));
        args.push(format!(
            "{flag}={}:{}:{}",
            v.wrapping_add(1),
            v.wrapping_add(1),
            65535u32.saturating_sub(v)
        ));
        args
    }
    let mut args = uid_args(u, "--uidmap");
    args.extend(uid_args(g, "--gidmap"));
    args
}

impl PodmanChannel {
    pub fn with_worktree(mut self, wt: crate::repo::WorktreeInfo) -> Self {
        self.worktree = Some(wt);
        self
    }

    /// Erzeugt einen PodmanChannel im Run-Modus mit gegebenen Parametern.
    // Konstruktor mit vollständiger Parameterliste – die Aufteilung in ein
    // Struct brächte hier keinen Gewinn (zwei Aufrufstellen, feste Reihenfolge).
    #[allow(clippy::too_many_arguments)]
    pub fn new_run(
        name: &str,
        container: &str,
        host_root: &Path,
        image: &str,
        workdir: &str,
        timeout: Duration,
        uid: u32,
        gid: u32,
        home: String,
        usermapping: PodmanUserMapping,
    ) -> Self {
        PodmanChannel {
            name: name.to_string(),
            mode: PodmanMode::Run,
            container: container.to_string(),
            workdir: workdir.to_string(),
            host_root: Some(host_root.to_path_buf()),
            image: Some(image.to_string()),
            timeout,
            uid,
            gid,
            home,
            usermapping,
            worktree: None,
            seq: AtomicUsize::new(1),
            status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
            managed: Arc::new(Mutex::new(Vec::new())),
            shell: Mutex::new(None),
        }
    }

    fn file_path(&self, rel: &Path) -> Result<PathBuf, String> {
        let host = self
            .host_root
            .as_ref()
            .ok_or("File access not available – host_root missing")?;
        resolve(host, rel)
    }

    /// Stellt sicher, dass `container` läuft und den Host-Pfad `host_dir`
    /// unter `workdir` gemountet hat. Da das User-Namespace (`--userns` bzw.
    /// `--uidmap`/`--gidmap`) bei der Erzeugung fixiert ist, wird nur dann neu
    /// aufgesetzt, wenn nötig: ein bereits laufender Container mit passendem
    /// UID-Mapping (siehe `PodmanUserMapping`), Mount und Image wird
    /// **wiederverwendet** (kein `rm`/`run`). Ein gestoppter, fremder oder
    /// abweichend aufgesetzter Container (z. B. nach einem Crash mit anderem
    /// Mapping erzeugt) wird entfernt und frisch mit `--init` aufgesetzt – nur
    /// so gilt das gewählte Mapping garantiert für alle späteren execs, und
    /// der Init als PID 1 räumt verwaiste Kindprozesse sofort auf.
    /// Der gemeldete Status wechselt währenddessen `Starting` → `Running`
    /// bzw. `Problem` bei Fehlern.
    fn ensure_running(&self, container: &str, host_dir: &Path) -> Result<(), String> {
        if self.read_status() == ChannelStatus::Running {
            return Ok(());
        }
        self.set_status(ChannelStatus::Starting);
        let bind = format!("{}:{}", host_dir.display(), self.workdir);
        let state = inspect_container_state(container, Duration::from_secs(30))
            .inspect_err(|_| self.set_status(ChannelStatus::Problem))?;
        if reuse_container(&state, &bind, self.image.as_deref(), self.usermapping) {
            // Läuft bereits korrekt aufgesetzt → weiterverwenden.
            self.set_status(ChannelStatus::Running);
            return Ok(());
        }
        if !matches!(state, ContainerState::Missing) {
            let _ = run_with_timeout(
                "podman",
                &["rm".into(), "-f".into(), container.into()],
                Path::new("."),
                Duration::from_secs(60),
            );
        }

        let image = self.image.as_deref().ok_or("Run channel without image")?;
        let wd = self.workdir.clone();
        // Basis-Argumente, unabhängig vom Mapping.
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            container.into(),
        ];
        // UID/GID-Mapping nach Top-Level-Config. `self.uid`/`self.gid` sind die
        // exec-Identität (`--user`), die zugleich den Anker des Mappings bildet:
        // `keep-id` → Host-Identität, `uidmap` → Gast-UID/-GID aus dem Image.
        match self.usermapping {
            PodmanUserMapping::KeepId => {
                args.push("--userns".into());
                args.push("keep-id".into());
            }
            PodmanUserMapping::Uidmap => {
                // Explizite Ranges: Gast-UID/-GID auf den Host-User mappen,
                // Rest identisch auffüllen (16-Bit-Raum).
                args.extend(uidmap_args(self.uid, self.gid));
            }
        }
        args.push("-v".into());
        args.push(format!("{}:{wd}", host_dir.display()));
        args.push("-w".into());
        args.push(wd.clone());
        args.push(image.into());
        args.push("sleep".into());
        args.push("infinity".into());
        let st = run_with_timeout("podman", &args, Path::new("."), Duration::from_secs(120))?;
        if st.exit_code != Some(0) {
            self.set_status(ChannelStatus::Problem);
            return Err(format!(
                "podman run \"{container}\" failed:\n{}\n{}",
                st.stderr, st.stdout
            ));
        }

        {
            let mut g = self
                .managed
                .lock()
                .map_err(|_| "managed-Lock verloren".to_string())?;
            if !g.iter().any(|n| n == container) {
                g.push(container.to_string());
            }
        }
        self.set_status(ChannelStatus::Running);
        Ok(())
    }
}

impl PodmanChannel {
    /// Liefert den geteilten Kanal-Zustand (Default: grau, falls gesperrt).
    fn read_status(&self) -> ChannelStatus {
        self.status
            .lock()
            .map(|g| *g)
            .unwrap_or(ChannelStatus::Unknown)
    }

    fn set_status(&self, s: ChannelStatus) {
        if let Ok(mut g) = self.status.lock() {
            *g = s;
        }
    }
}

impl Channel for PodmanChannel {
    fn kind(&self) -> ChannelKind {
        match self.mode {
            PodmanMode::Attach => ChannelKind::PodmanAttach,
            PodmanMode::Run => ChannelKind::PodmanRun,
        }
    }

    fn root(&self) -> String {
        match self.mode {
            // Run-Kanäle: Der Name enthält bereits Host-Pfad bzw. Kontext
            // ("imagename worktreepfad") – kein zusätzlicher Gast-Pfad.
            PodmanMode::Run => format!("Podman:{}", self.name),
            // Attach-Kanäle: kurzer Config-Name + Arbeitsverzeichnis im Container.
            PodmanMode::Attach => format!("Podman:{}:{}", self.name, self.workdir),
        }
    }

    fn label(&self) -> String {
        self.name.clone()
    }

    /// Startet den Run-Container (falls nötig) im Hintergrund; Attach-Kanäle
    /// sind extern geteilt und können hier nicht gestartet werden (kein Op).
    fn warmup(&self) {
        if self.mode == PodmanMode::Run {
            if let Some(host) = self.host_root.as_deref() {
                let _ = self.ensure_running(&self.container, host);
            }
        }
    }

    fn read(&self, rel: &Path) -> Result<String, String> {
        fsops::read_at(&self.file_path(rel)?)
    }

    fn write(&self, rel: &Path, content: &str) -> Result<(), String> {
        fsops::write_at(&self.file_path(rel)?, content)
    }

    fn grep(
        &self,
        pattern: &str,
        rel: &Path,
        include: Option<&str>,
        context_lines: usize,
    ) -> Result<SearchResult, String> {
        let dir = self.file_path(rel)?;
        search_files(pattern, &dir, self.timeout, include, context_lines)
    }

    fn glob(&self, pattern: &str, rel: &Path) -> Result<Vec<String>, String> {
        let dir = self.file_path(rel)?;
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
        cancel: Option<&AtomicBool>,
    ) -> Result<RunOut, String> {
        self.ensure_running_for_rel(rel_cwd)?;
        let wd = join_workdir(&self.workdir, rel_cwd)?;
        let argv = self.exec_argv(&wd, cmd, args);
        let out = run_with_timeout_live(
            "podman",
            &argv,
            Path::new("."),
            self.timeout,
            Some(live),
            cancel,
        )?;
        // Container zwischenzeitlich weg (z. B. nach OOM/Crash)? Im Run-Modus
        // einmal neu aufsetzen und wiederholen (Selbstheilung), sonst den
        // Problem-Status melden.
        let container_lost =
            out.stdout.contains("no such container") || out.stderr.contains("no such container");
        if container_lost && self.mode == PodmanMode::Run {
            self.set_status(ChannelStatus::Starting);
            // ensure setzt bei Fehlern bereits `Problem` und gibt es zurück.
            self.ensure_running_for_rel(rel_cwd)?;
            return run_with_timeout_live(
                "podman",
                &argv,
                Path::new("."),
                self.timeout,
                Some(live),
                cancel,
            );
        }
        if container_lost {
            self.set_status(ChannelStatus::Problem);
        }
        Ok(out)
    }

    fn shell(&self) -> Result<String, String> {
        if let Some(shell) = self.shell.lock().map(|g| g.clone()).unwrap_or(None) {
            return Ok(shell);
        }
        // Einmalige Probe IM Container: `bash -c "command -v bash"` bevorzugt –
        // ist bash im Image vorhanden, wird es genutzt; sonst fällt es auf sh
        // zurück (z. B. alpine ohne bash).
        let mut discard = |_: &str| {};
        let shell = if self
            .run_live(
                "bash",
                &["-c".to_string(), "command -v bash".to_string()],
                Path::new("."),
                &mut discard,
                None,
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

    fn status(&self) -> ChannelStatus {
        self.read_status()
    }

    /// Einmalige Zustandsprobe beim Binden (Alt+O) bzw. App-Start: prüft den
    /// Container ohne Befehle auszulösen und setzt den gemeldeten Status
    /// (grün wenn er passt und läuft; grau wenn er noch gar nicht gestartet
    /// ist; rot bei Problemen). Der Aufrufer läuft in einem Hintergrundfaden.
    fn probe(&self) {
        match inspect_container_state(&self.container, Duration::from_secs(30)) {
            Ok(state) => {
                let status = if self.mode == PodmanMode::Run {
                    let bind = self
                        .host_root
                        .as_ref()
                        .map(|h| format!("{}:{}", h.display(), self.workdir))
                        .unwrap_or_default();
                    podman_probe_status(
                        &state,
                        PodmanMode::Run,
                        &bind,
                        self.image.as_deref(),
                        self.usermapping,
                    )
                } else {
                    podman_probe_status(&state, PodmanMode::Attach, "", None, self.usermapping)
                };
                self.set_status(status);
            }
            Err(_) => self.set_status(ChannelStatus::Problem),
        }
    }

    /// Prüft vor dem Stoppen (Programmende, letzte Session endet), ob dieser
    /// selbst gestartete Run-Container wesentliche, ungesicherte Änderungen
    /// enthält – und meldet sie als Kurzbeschreibung:
    /// - Dateien im Container-Dateisystem außerhalb der gemounteten
    ///   Arbeitskopie (würden beim Stoppen des `--rm`-Containers verworfen),
    /// - nicht committete Änderungen in der Git-Arbeitskopie (`host_root`).
    ///
    /// Nur für tatsächlich verwaltete (beim Beenden stoppt aidev nur diese)
    /// und laufende Container; Attach-/Local- und nie gestartete Run-Kanäle
    /// melden nichts.
    fn essential_changes(&self) -> Option<Vec<String>> {
        if self.mode != PodmanMode::Run {
            return None;
        }
        // Nur Container, die aidev selbst verwaltet und beim Beenden stoppen
        // würde – kein Hinweis auf extern geteilte/lediglich wiederverwendete
        // Verbindungen, die gar nicht gestoppt werden.
        let managed = self
            .managed
            .lock()
            .map(|g| g.iter().any(|n| n == &self.container))
            .unwrap_or(false);
        if !managed {
            return None;
        }
        // Nur ein tatsächlich laufender Container kann beim Stoppen etwas
        // verlieren; Zustandsprobe schlägt fehl (z. B. kein podman) → melden.
        if !matches!(
            inspect_container_state(&self.container, Duration::from_secs(30)),
            Ok(ContainerState::Running { .. })
        ) {
            return None;
        }

        let mut notes: Vec<String> = Vec::new();
        if let Some(layer) = container_layer_changes(&self.container, &self.workdir, &self.home) {
            notes.push(layer);
        }
        if let Some(host) = self.host_root.as_deref() {
            if let Some(git) = git_worktree_changes(host) {
                notes.push(git);
            }
        }
        if notes.is_empty() {
            None
        } else {
            Some(notes)
        }
    }

    fn container_name(&self) -> Option<String> {
        // Nur eigene Run-Container werden beim Beenden gestoppt; deren Name
        // ist für den Hinweis-Dialog maßgeblich. Attach-Kanäle teilen einen
        // fremden Container und werden hier nicht aufgeführt.
        match self.mode {
            PodmanMode::Run => Some(self.container.clone()),
            PodmanMode::Attach => None,
        }
    }

    fn podman_run_spec(&self) -> Option<(String, String, String)> {
        // Die eigenen Laufzeit-Parameter liefern – so entsteht ein
        // gleich konfigurierter Folge-Kanal, egal ob dieser Kanal aus der
        // statischen Konfiguration oder vom ChannelBuilder stammt.
        // Geteilte (Attach-)Kanäle haben kein eigenes image → `None`.
        self.image
            .as_ref()
            .filter(|img| !img.is_empty())
            .map(|img| (img.clone(), self.workdir.clone(), self.home.clone()))
    }

    fn dup(&self) -> Result<Arc<dyn Channel>, String> {
        match self.mode {
            PodmanMode::Attach => {
                Err("Dieser Kanal ist geteilt und kann nicht dupliziert werden.".to_string())
            }
            PodmanMode::Run => {
                let host = self
                    .host_root
                    .clone()
                    .ok_or("Run channel without host_root")?;
                self.ensure_running(&self.container, &host)?;

                let seq = self.seq.fetch_add(1, Ordering::Relaxed);
                let base = sanitize(&self.name);
                let branch = sanitize(&format!("aidev/{base}-d{seq}"));
                let worktree = host
                    .parent()
                    .ok_or("host_root without parent directory")?
                    .join(format!("{base}-wt{seq}"));

                let st = run_with_timeout(
                    "git",
                    &[
                        "-C".into(),
                        host.display().to_string(),
                        "worktree".into(),
                        "add".into(),
                        "-b".into(),
                        branch.clone(),
                        worktree.display().to_string(),
                    ],
                    Path::new("."),
                    self.timeout,
                )
                .map_err(|e| format!("git not available: {e}"))?;
                if st.exit_code != Some(0) {
                    return Err(format!(
                        "git worktree add failed:\n{}\n{}",
                        st.stderr, st.stdout
                    ));
                }

                let new = PodmanChannel {
                    name: format!("{base}-d{seq}"),
                    mode: PodmanMode::Run,
                    container: format!("{}-d{seq}", self.container),
                    workdir: self.workdir.clone(),
                    host_root: Some(worktree),
                    image: self.image.clone(),
                    timeout: self.timeout,
                    uid: self.uid,
                    gid: self.gid,
                    home: self.home.clone(),
                    usermapping: self.usermapping,
                    worktree: None,
                    seq: AtomicUsize::new(1),
                    status: Arc::new(Mutex::new(ChannelStatus::Unknown)),
                    managed: self.managed.clone(),
                    shell: Mutex::new(None),
                };
                new.ensure_running(&new.container, new.host_root.as_deref().unwrap())?;
                Ok(Arc::new(new))
            }
        }
    }

    fn host_root(&self) -> Option<PathBuf> {
        self.host_root.clone()
    }

    fn owned_worktree(&self) -> Option<&crate::repo::WorktreeInfo> {
        self.worktree.as_ref()
    }
}

impl PodmanChannel {
    /// Baut die `podman exec`-argv. Im Run-Modus wird immer als genau die
    /// gemerkte Identität gearbeitet (`--user <uid>:<gid>` = `self.uid`/`self.gid`;
    /// bei `keep-id` die Host-, bei `uidmap` die Gast-Identität aus dem Image –
    /// passend zum Mapping, mit dem der Container erzeugt wurde) und mit
    /// schreibbarem `$HOME` – damit laufen Root- und Non-Root-Images identisch
    /// als Nicht-Root. Im Attach-Modus gilt der Default-User des bestehenden
    /// Containers.
    pub(crate) fn exec_argv(&self, wd: &str, cmd: &str, args: &[String]) -> Vec<String> {
        let mut argv = vec!["exec".to_string(), "--workdir".to_string(), wd.to_string()];
        if self.mode == PodmanMode::Run {
            argv.push("--user".into());
            argv.push(format!("{}:{}", self.uid, self.gid));
            if !self.home.is_empty() {
                argv.push("--env".into());
                argv.push(format!("HOME={}", self.home));
            }
        }
        argv.push(self.container.clone());
        argv.push(cmd.to_string());
        argv.extend(args.iter().cloned());
        argv
    }

    /// Läuft noch nicht? Dann Run-Container (gemäß Mode) sicherstellen.
    fn ensure_running_for_rel(&self, _rel: &Path) -> Result<(), String> {
        if self.mode == PodmanMode::Run {
            let host = self
                .host_root
                .as_ref()
                .ok_or("Run channel without host_root")?;
            self.ensure_running(&self.container, host)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn podman_run_spec_liefert_image_workdir_home() {
        // Ein vom ChannelBuilder erzeugter Podman-Run-Kanal trägt seine
        // Laufzeit-Parameter selbst – genau die soll /branch übernehmen.
        let ch = PodmanChannel::new_run(
            "feature",
            "aidev-feature",
            Path::new("/work/proj/.aidev/wt-feature"),
            "docker.io/library/node:22",
            "/usr/src/app",
            Duration::from_secs(30),
            1000,
            1000,
            "/home/dev".to_string(),
            PodmanUserMapping::KeepId,
        );
        assert_eq!(
            ch.podman_run_spec(),
            Some((
                "docker.io/library/node:22".to_string(),
                "/usr/src/app".to_string(),
                "/home/dev".to_string(),
            ))
        );
    }

    #[test]
    fn podman_run_spec_ohne_image_ist_none() {
        // Attach-Kanäle haben kein eigenes image → kein Spec.
        let mut ch = PodmanChannel::new_run(
            "shared",
            "aidev-shared",
            Path::new("/work/proj"),
            "",
            "/work",
            Duration::from_secs(30),
            0,
            0,
            String::new(),
            PodmanUserMapping::KeepId,
        );
        ch.mode = PodmanMode::Attach;
        assert_eq!(ch.podman_run_spec(), None);
    }
}
