//! Pfad-Auflösung und Symlink-Security.
//!
//! `resolve` baut Pfade relativ zur Kanal-Wurzel auf und verhindert das
//! Verlassen der Wurzel durch `..` oder Symlinks.

use std::path::{Component, Path, PathBuf};

pub(crate) fn resolve(base: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                return Err(format!("Path escapes the channel root: {}", rel.display()))
            }
        }
    }
    ensure_within_root(base, &out)?;
    Ok(out)
}

/// Stellt sicher, dass `path` auch nach Auflösung etwaiger Symlinks innerhalb
/// der Kanal-Wurzel `root` liegt – die eigentliche Sicherheitsgrenze der
/// Datei-Operationen ([`Channel::read`]/[`write`]/[`grep`]/[`run`]).
///
/// Die Wurzel wird kanonisiert (ein Symlink der Wurzel selbst ist die
/// vertrauenswürdige, konfigurierte Basis). Danach wird `path` komponentenweise
/// unter der kanonischen Wurzel aufgebaut: Vorhandene Komponenten werden –
/// inklusive Symlinks (ggf. mehrstufig) – aufgelöst und ihr Ziel gegen die
/// Wurzel geprüft. Zeigt ein Symlink nach außen oder ist er kaputt (Ziel nicht
/// auflösbar), wird der Zugriff abgewiesen. Noch nicht vorhandene Teile
/// (z. B. eine neue Datei beim Schreiben) werden erst *innerhalb* der bereits
/// verifizierten Wurzel angelegt und sind dadurch sicher.
///
/// Existiert die Wurzel selbst noch nicht (die erste Schreiboperation legt sie
/// gerade an), gibt es in ihr noch keine Symlinks – der Zugriff bleibt rein
/// lexikalisch unterhalb der Wurzel und ist durch die `..`-Abweisung in
/// [`resolve`] geschützt. Nur wenn ein Bestandteil der Wurzel ein (ggf.
/// kaputter) Symlink ist, wird abgewiesen: Ein solcher könnte einen
/// Schreibzugriff vor der Erzeugung der Wurzel aus ihr hinausführen.
fn ensure_within_root(root: &Path, path: &Path) -> Result<(), String> {
    let root_canon = match root.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            if path_contains_existing_symlink(root) {
                return Err(format!(
                    "Channel root contains a symlink and cannot be resolved: {}",
                    root.display()
                ));
            }
            return Ok(());
        }
    };
    let rel = path
        .strip_prefix(root)
        .map_err(|_| format!("Path outside the channel root: {}", path.display()))?;
    let mut current = root_canon.clone();
    for comp in rel.components() {
        let Component::Normal(part) = comp else {
            continue;
        };
        let next = current.join(part);
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = next.canonicalize().map_err(|_| {
                    format!(
                        "Path contains a broken symlink (target unresolvable): {}",
                        next.display()
                    )
                })?;
                if !target.starts_with(&root_canon) {
                    return Err(format!(
                        "Path escapes the channel root via a symlink: {}",
                        path.display()
                    ));
                }
                current = target;
            }
            Ok(meta) if meta.is_dir() => {
                current = next;
            }
            Ok(_) => {
                // Vorhandene Datei – unterhalb ist kein Zugriff möglich
                // (ENOTDIR); der eigentliche Zugriff scheitert dann ohnehin.
                return Ok(());
            }
            Err(_) => {
                // Noch nicht vorhandener Teil: wird erst innerhalb der bereits
                // verifizierten Wurzel angelegt → sicher.
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Enthält einer der (noch) vorhandenen Bestandteile von `path` einen
/// Symlink? Wird nur benötigt, wenn `path` selbst nicht kanonisierbar ist –
/// dann wäre die Wurzel ein (ggf. kaputter) Symlink und darf nicht als
/// vertrauenswürdige Basis gelten. Ab dem ersten vorhandenen, normalen
/// Bestandteil (Verzeichnis/Datei) ist der Rest oberhalb unbedenklich: fehlende
/// Teile können keine Symlinks sein, vorhandene wurden bereits geprüft.
fn path_contains_existing_symlink(path: &Path) -> bool {
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) => return meta.file_type().is_symlink(),
            Err(_) => {
                if !current.pop() {
                    return false;
                }
            }
        }
    }
}

/// Container-Pfad aus `workdir` + relativem Pfad (kein `..`, kein Escape).
pub(super) fn join_workdir(workdir: &str, rel: &Path) -> Result<String, String> {
    let mut tail: Vec<String> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => tail.push(part.to_string_lossy().to_string()),
            Component::ParentDir => {
                return Err(format!("Path escapes the channel root: {}", rel.display()))
            }
            _ => {}
        }
    }
    Ok(if tail.is_empty() {
        workdir.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), tail.join("/"))
    })
}
