//! Geteilte Datei-Operationen für Kanäle mit Host-Wurzel.
//!
//! `Local` und der Podman-Kanal mit gesetztem `host_root` führen dieselben
//! Operationen aus – sie unterscheiden sich nur darin, wie aus dem
//! kanalrelativen Pfad ein absoluter Host-Pfad wird (`resolve` bzw.
//! `file_path`). Die eigentlichen FS-Zugriffe liegen hier, damit beide
//! Kanal-Varianten garantiert identisch bleiben (gleiche Fehlermeldungen,
//! gleiche Sortierung, gleiche Verzeichnis-Anlage beim Schreiben).

use std::path::Path;

use super::Entry;

/// Liest eine Datei als UTF-8-String.
pub(crate) fn read_at(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("Lese {}: {e}", p.display()))
}

/// Schreibt eine Datei; fehlende Elternverzeichnisse werden angelegt.
pub(crate) fn write_at(p: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(p, content).map_err(|e| format!("Schreibe {}: {e}", p.display()))
}

/// Listet ein Verzeichnis auf (Einträge sortiert nach Name).
pub(crate) fn list_at(p: &Path) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    for e in std::fs::read_dir(p).map_err(|e| format!("ls {}: {e}", p.display()))? {
        let e = e.map_err(|e| format!("ls-Fehler: {e}"))?;
        entries.push(Entry {
            name: e.file_name().to_string_lossy().to_string(),
            is_dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}
