//! OpenCode-kompatible ID-Generierung für `x-opencode-request` / `x-opencode-session`.
//!
//! Format: `{prefix}_{12 Hex}{14 Base62}`, wobei
//! - der 12-Hex-Teil ein aufsteigend/absteigend sortierbarer Zeitstempel ist
//!   (`timestamp_ms × 0x1000 + counter`, für absteigend bitweise invertiert),
//! - der 14-Base62-Teil ein Zufallswert ist (Kollisionsschutz).
//!
//! Sessions werden **absteigend** erzeugt (neueste zuerst), Messages/Parts/… 
//! **aufsteigend** (chronologische Sortierung ohne Extraschritt).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Stabile Session-ID je Konversation: Der `x-opencode-session`-Header soll
/// über die gesamte Lebensdauer einer Chat-Konversation konstant bleiben. Die
/// Konversations-ID (usize) wird beim ersten Auftreten auf eine generierte
/// `ses_…`-ID abgebildet und anschließend wiederverwendet.
static SESSION_IDS: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

/// Liefert die stabile `ses_…`-ID für die Konversation `conversation`, erzeugt
/// sie beim ersten Aufruf.
pub fn session_id_for(conversation: usize) -> String {
    let mut guard = SESSION_IDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(conversation)
        .or_insert_with(|| format!("ses_{}", payload(true)))
        .clone()
}

// Monotone Sequenz-Nummer je Millisekunde (0x1000 Auflösung), damit mehrere IDs
// derselben Millisekunde trotzdem lexikografisch aufsteigend bleiben.
static mut LAST_TS: u64 = 0;
static mut COUNTER: u32 = 0;

/// Aktueller Zeitstempel in Millisekunden seit Unix-Epoch.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Kern: `{12 Hex (ts*4096 + ctr)}{14 Base62}`, optional bitweise invertiert.
fn payload(descending: bool) -> String {
    let ts = now_ms();
    let counter = unsafe {
        if ts != LAST_TS {
            LAST_TS = ts;
            COUNTER = 0;
        }
        COUNTER += 1;
        COUNTER
    };

    let mut val = ts.wrapping_mul(0x1000).wrapping_add(counter as u64);
    if descending {
        val = !val;
    }

    // 6 Big-Endian-Bytes → 12 Hex-Zeichen
    let hex: String = (0..6)
        .map(|i| format!("{:02x}", (val >> (40 - 8 * i)) & 0xff))
        .collect();

    // 14 Zufalls-Zeichen (Base62) über splitmix64-PRNG – kein externer `rand`-
    // Abruf nötig; der Seed mischt Zeit + Sequenz für ausreichend Entropie.
    let mut seed: u64 = ts
        ^ (counter as u64).wrapping_mul(0x9E3779B97F4A7C15)
        ^ (now_ms() << 32);
    let rand: String = (0..14)
        .map(|_| {
            seed = seed.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = (seed ^ (seed >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            BASE62[((z ^ (z >> 31)) % 62) as usize] as char
        })
        .collect();

    format!("{hex}{rand}")
}

/// Neue, eindeutige **Message**-ID (aufsteigend) für `x-opencode-request`.
pub fn message_id() -> String {
    format!("msg_{}", payload(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape() {
        let m = message_id();
        assert!(m.starts_with("msg_"));
        assert_eq!(m.len(), 3 + 1 + 12 + 14); // 30
        let s = session_id_for(1);
        assert!(s.starts_with("ses_"));
        assert_eq!(s.len(), 3 + 1 + 12 + 14); // 30
    }

    #[test]
    fn ascending_sortable() {
        let a = message_id();
        let b = message_id();
        assert!(a < b, "{a} < {b}");
    }

    #[test]
    fn descending_sortable() {
        // Descending: `~current` sinkt mit wachsendem Zeitstempel → die NEUERE
        // Session hat den KLEINEREN Wert (liest beim Aufsteigend-Sortieren die
        // neuesten zuerst).
        let a = session_id_for(2);
        let b = session_id_for(3);
        assert!(a > b, "{a} > {b}");
    }

    #[test]
    fn session_id_stable_per_conversation() {
        let a = session_id_for(42);
        let b = session_id_for(42);
        let other = session_id_for(43);
        assert_eq!(a, b, "gleiche Konversation → gleiche Session-ID");
        assert_ne!(a, other, "verschiedene Konversationen → andere Session-ID");
    }
}
