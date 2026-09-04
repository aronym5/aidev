//! Geteilte Datei-Operationen für Kanäle mit Host-Wurzel.
//!
//! `Local` und der Podman-Kanal mit gesetztem `host_root` führen dieselben
//! Operationen aus – sie unterscheiden sich nur darin, wie aus dem
//! kanalrelativen Pfad ein absoluter Host-Pfad wird (`resolve` bzw.
//! `file_path`). Die eigentlichen FS-Zugriffe liegen hier, damit beide
//! Kanal-Varianten garantiert identisch bleiben (gleiche Fehlermeldungen,
//! gleiche Sortierung, gleiche Verzeichnis-Anlage beim Schreiben).

use std::path::Path;

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
