//! Container-Zustandsverwaltung (Podman inspect, Reuse-Logik, Layer-Änderungen).

use std::path::Path;
use std::time::Duration;

use super::run::run_with_timeout;
use super::{ChannelStatus, PodmanMode};
use crate::config::PodmanUserMapping;

/// Zustand eines Podman-Containers aus dem `inspect`-Check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ContainerState {
    /// Existiert nicht (inspect schlug fehl / „no such object“).
    Missing,
    /// Existiert, läuft aber nicht (`--rm`-Container sind danach weg → neu).
    Stopped,
    /// Läuft; Mapping-, Mount- und Image-Details für die Wiederverwendungs-
    /// Entscheidung.
    Running {
        userns: String,
        binds: String,
        image: String,
    },
}

/// Interpretiert `podman inspect --format`-Ausgabe
/// (`{{.State.Running}}|{{.HostConfig.UsernsMode}}|{{.HostConfig.Binds}}|{{.Config.Image}}`)
/// in einen [`ContainerState`]. Bei Exit-Code ≠ 0 gilt der Container als
/// nicht vorhanden.
pub(super) fn parse_inspect_state(stdout: &str, ok: bool) -> ContainerState {
    if !ok {
        return ContainerState::Missing;
    }
    let mut parts = stdout.trim().splitn(4, '|');
    let running = parts.next().unwrap_or("").trim() == "true";
    let userns = parts.next().unwrap_or("").to_string();
    let binds = parts.next().unwrap_or("").to_string();
    let image = parts.next().unwrap_or("").to_string();
    if running {
        ContainerState::Running {
            userns,
            binds,
            image,
        }
    } else {
        ContainerState::Stopped
    }
}

/// Fragt den Zustand von `container` über `podman inspect` ab.
pub(super) fn inspect_container_state(
    container: &str,
    timeout: Duration,
) -> Result<ContainerState, String> {
    let out = run_with_timeout(
        "podman",
        &[
            "inspect".into(),
            "--format".into(),
            "{{.State.Running}}|{{.HostConfig.UsernsMode}}|{{.HostConfig.Binds}}|{{.Config.Image}}"
                .into(),
            container.into(),
        ],
        Path::new("."),
        timeout,
    )
    .map_err(|e| format!("podman not available: {e}"))?;
    Ok(parse_inspect_state(&out.stdout, out.exit_code == Some(0)))
}

/// Wiederverwendungs-Entscheidung: Der laufende Container ist nur dann
/// brauchbar, wenn Mapping, Mount (`host:workdir`) und Image zum Kanal passen.
/// Alles andere gilt als stale und wird neu aufgesetzt.
///
/// Das Mapping wird über [`PodmanUserMapping`] spezifiziert:
/// - `KeepId`: `UsernsMode` muss `keep-id` enthalten.
/// - `Uidmap`: der Container darf **kein** `keep-id`-Container sein (explizite
///   `--uidmap`/`--gidmap` erzeugen ein privates Userns ohne den `keep-id`-Marker).
///
/// So wird bei einem Moduswechsel garantiert neu aufgesetzt.
pub(super) fn reuse_container(
    state: &ContainerState,
    bind: &str,
    image: Option<&str>,
    usermapping: PodmanUserMapping,
) -> bool {
    let ContainerState::Running {
        userns,
        binds,
        image: img,
    } = state
    else {
        return false;
    };
    let userns_ok = match usermapping {
        PodmanUserMapping::KeepId => userns.contains("keep-id"),
        PodmanUserMapping::Uidmap => !userns.contains("keep-id"),
    };
    userns_ok && binds.contains(bind) && image.is_none_or(|i| img == i)
}

/// Status-Entscheidung der einmaligen Probe (ohne Befehle auszulösen):
/// - Run-Modus: läuft er und passt → grün; fehlt/gestoppt oder läuft falsch
///   → grau (startet bzw. wird beim ersten Befehl neu aufgesetzt).
/// - Attach-Modus: läuft → grün; fehlt/gestoppt → rot (er muss existieren).
pub(super) fn podman_probe_status(
    state: &ContainerState,
    mode: PodmanMode,
    bind: &str,
    image: Option<&str>,
    usermapping: PodmanUserMapping,
) -> ChannelStatus {
    match state {
        ContainerState::Missing | ContainerState::Stopped => match mode {
            PodmanMode::Run => ChannelStatus::Unknown,
            PodmanMode::Attach => ChannelStatus::Problem,
        },
        ContainerState::Running { .. }
            if reuse_container(state, bind, image, usermapping) =>
        {
            ChannelStatus::Running
        }
        ContainerState::Running { .. } => ChannelStatus::Unknown,
    }
}

/// Wesentliche Änderungen im Container-Dateisystem (Writable-Layer) eines
/// laufenden Run-Containers – also Zustand, der beim Stoppen des
/// `--rm`-Containers verworfen würde. Basis ist `podman diff`; ausgefiltert
/// werden die gemountete Arbeitskopie (`workdir`, lebt ohnehin auf dem Host),
/// der konfigurierte `home` (Tool-Caches) und flüchtige Verzeichnisse
/// (`/tmp`, `/var/tmp`, `/run`, …). Liefert eine Kurzbeschreibung samt
/// Beispiel-Pfaden, `None` wenn nichts Wesentliches vorliegt oder das
/// Werkzeug nicht läuft.
pub(super) fn container_layer_changes(
    container: &str,
    workdir: &str,
    home: &str,
) -> Option<String> {
    let out = match run_with_timeout(
        "podman",
        &["diff".into(), container.into()],
        Path::new("."),
        Duration::from_secs(60),
    ) {
        Ok(o) if o.exit_code == Some(0) => o,
        _ => return None, // podman diff nicht verfügbar/fehlerhaft → nicht blockieren
    };
    let paths = essential_diff_paths(&out.stdout, workdir, home);
    if paths.is_empty() {
        return None;
    }
    // Mehr Beispielpfade nennen, damit die Liste der geänderten Dateien im
    // Beenden-Dialog nicht zu stark gekürzt wirkt.
    const SHOW: usize = 5;
    let examples: Vec<String> = paths[..SHOW.min(paths.len())]
        .iter()
        .map(|p| format!("/{p}"))
        .collect();
    let examples = examples.join(", ");
    let text = if paths.len() <= SHOW {
        format!("Container filesystem changes: {examples}")
    } else {
        format!(
            "Container filesystem changes: {examples} – and {} more",
            paths.len() - SHOW
        )
    };
    Some(text)
}

/// Zerlegt die `podman diff`-Ausgabe (`<A|C|D|M> <pfad>`-Zeilen) und filtert
/// die Pfade heraus, die beim Stoppen eines `--rm`-Containers **nicht**
/// verloren gehen: die gemountete Arbeitskopie (`workdir`), der konfigurierte
/// `home` sowie flüchtige Verzeichnisse. Liefert die übrigen „wesentlichen“
/// Pfade – dedupliziert und sortiert. Reine Funktion, damit sie ohne podman
/// testbar ist.
pub(super) fn essential_diff_paths(diff_out: &str, workdir: &str, home: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in diff_out.lines() {
        let Some(path) = line
            .strip_prefix('A')
            .or_else(|| line.strip_prefix('C'))
            .or_else(|| line.strip_prefix('D'))
            .or_else(|| line.strip_prefix('M'))
        else {
            continue;
        };
        let path = path.trim().trim_start_matches('/');
        if path.is_empty()
            || path_at_or_under(path, workdir)
            || path_at_or_under(path, home)
            || volatile_diff_path(path)
        {
            continue;
        }
        paths.push(path.to_string());
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Liegt `path` (ohne führenden `/`) unter `base` (Container-Pfad, mit oder
/// ohne führendem `/`) bzw. ist es `base` selbst?
pub(super) fn path_at_or_under(path: &str, base: &str) -> bool {
    let base = base.trim_matches('/');
    if base.is_empty() {
        return true;
    }
    path == base || {
        path.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Gehört der Pfad (ohne führenden `/`) zu einem flüchtigen Verzeichnis,
/// dessen Inhalt beim Neustart ohnehin verloren geht (kein wesentlicher
/// Container-Zustand)?
pub(super) fn volatile_diff_path(path: &str) -> bool {
    const VOLATILE: [&str; 6] = ["tmp", "var/tmp", "run", "proc", "sys", "dev"];
    VOLATILE.iter().any(|v| {
        path == *v
            || path
                .strip_prefix(v)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Zählt die nicht committeten Änderungen einer Git-Arbeitskopie und nennt
/// einen Beispiel-Pfad. Keine gültige Git-Arbeitskopie (oder kein `git` auf
/// dem Host) → `None`, damit nichts blockiert.
pub(super) fn git_worktree_changes(host: &Path) -> Option<String> {
    let out = match run_with_timeout(
        "git",
        &[
            "-C".into(),
            host.display().to_string(),
            "status".into(),
            "--porcelain".into(),
        ],
        Path::new("."),
        Duration::from_secs(30),
    ) {
        Ok(o) if o.exit_code == Some(0) => o,
        _ => return None,
    };
    git_porcelain_note(&out.stdout)
}

/// Baut aus `git status --porcelain`-Ausgabe die Kurznotiz: zählt die Einträge
/// und nennt bis zu wenige Beispiel-Pfade. Leere Ausgabe → `None`. Reine
/// Funktion für die Tests (ohne `git`-Aufruf).
pub(super) fn git_porcelain_note(stdout: &str) -> Option<String> {
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.is_empty() {
        return None;
    }
    // „XY <pfad>“ → ohne die zwei Status-Spalten und das Trenn-Leerzeichen.
    // Bis zu `SHOW` Pfade als Beispiele aufführen, Rest nur zählen.
    const SHOW: usize = 4;
    let paths: Vec<String> = lines
        .iter()
        .map(|l| l.chars().skip(3).collect::<String>().trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let examples: Vec<String> = paths.iter().take(SHOW).cloned().collect();
    let suffix = if examples.is_empty() {
        String::new()
    } else if paths.len() <= SHOW {
        format!(" (e.g. {})", examples.join(", "))
    } else {
        format!(
            " (e.g. {} – and {} more)",
            examples.join(", "),
            paths.len() - SHOW
        )
    };
    if lines.len() == 1 {
        Some(format!("1 uncommitted change in the working copy{suffix}"))
    } else {
        Some(format!(
            "{} uncommitted changes in the working copy{suffix}",
            lines.len()
        ))
    }
}
