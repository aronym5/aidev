//! Geteilte Text-Kürzung für lange Ausgaben.
//!
//! Zwei Stellen nutzen denselben Algorithmus: die Shell-Ausführung
//! (`channel::run`) begrenzt Konsolen-Ausgaben, das LLM-Modul kürzt
//! Werkzeug-Ergebnisse und Meldungen. Die Invariante ist immer dieselbe:
//! Das **Ende** bleibt vollständig lesbar (dort stehen Fehler/Endergebnis),
//! von VORN werden ganze Zeilen verworfen – nie wird mitten in einer Zeile
//! gekappt, außer eine einzelne Zeile sprengt allein schon das Limit.

/// Kürzt `s` auf höchstens `cap` Zeichen (Zeichen zählen, nicht Bytes).
///
/// Über dem Limit bleiben ganze Endzeilen stehen; davor steht der
/// mehrzeilige `marker` (z. B. `"…\n[vorne gekürzt]\n"`). Passt schon
/// alles, wird der Text unverändert zurückgegeben. Sprengt eine einzelne
/// Zeile allein das Limit, bleibt notgedrungen nur ihr Ende (mit „…“
/// davor) – das Ergebnis umfasst dann exakt `cap` Zeichen.
pub(crate) fn truncate_tail(s: &str, cap: usize, marker: &str) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let budget = cap.saturating_sub(marker.chars().count());
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in s.lines().rev() {
        // Erste (letzte) Zeile ohne Trenn-Umbruch, jede weitere davor +1.
        let add = line.chars().count() + if kept.is_empty() { 0 } else { 1 };
        if used + add > budget {
            break;
        }
        kept.push(line);
        used += add;
    }
    if kept.is_empty() {
        // Pathologisch lange Einzelzeile: auch dann nur ihr Ende zeigen.
        let total = s.chars().count();
        let tail_count = cap.saturating_sub(1).min(total);
        let tail: String = s.chars().skip(total - tail_count).collect();
        return format!("…{tail}");
    }
    kept.reverse();
    format!("{marker}{}", kept.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kurzer_text_bleibt_unveraendert() {
        assert_eq!(truncate_tail("kurz\nende", 100, "M\n"), "kurz\nende");
        assert_eq!(truncate_tail("exakt", 5, "M\n"), "exakt");
    }

    #[test]
    fn ganze_endzeilen_bleiben_marker_stellt_vorn() {
        // Budget = 12 - 3 (Marker „M\n“) … „drei\nvier“ (9) passt, „zwei“ davor
        // würde 14 ergeben → nur die letzten beiden Zeilen bleiben.
        assert_eq!(
            truncate_tail("eins\nzwei\ndrei\nvier", 12, "M\n"),
            "M\ndrei\nvier"
        );
    }

    #[test]
    fn riesenzeile_gibt_nur_ihr_ende_bis_genau_cap() {
        let out = truncate_tail(&"x".repeat(50), 10, "M\n");
        assert_eq!(out.chars().count(), 10);
        assert!(out.starts_with('…'));
        assert!(out.ends_with('x'));
    }

    #[test]
    fn marker_laenger_als_budget_wird_geduldet() {
        // Marker größer als das Limit: budget = 0 → Einzelfall-Ende.
        let out = truncate_tail("a\nb\nc", 4, "SEHR_LANGER_MARKER\n");
        assert!(out.chars().count() <= 4, "{out}");
        assert!(out.ends_with('c'), "{out}");
    }
}
