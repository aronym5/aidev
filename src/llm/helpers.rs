//! Geteilte Hilfsfunktionen: Fehlerketten, Kürzung, Debug-Dump, Zeitstempel.

use std::path::PathBuf;

use serde_json::Value;

/// Formatiert eine Fehlerkette (reqwest → connect → TLS) in einem String.
pub(crate) fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut src: Option<&dyn std::error::Error> = err.source();
    while let Some(next) = src {
        out.push_str(" <- ");
        out.push_str(&next.to_string());
        src = next.source();
    }
    out
}

/// Liest eine menschenlesbare Meldung aus einem API-`error`-Wert (String oder
/// Objekt mit `message`-Feld, wie ihn OpenAI-kompatible Endpunkte senden).
pub(crate) fn error_message(error: &Value) -> String {
    match error {
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
        _ => error.to_string(),
    }
}

/// Länge der einzeiligen Fehler-Kurzfassung (Statuszeile + Chat-Zeile).
pub(crate) const ERROR_SUMMARY_MAX: usize = 160;

/// Vordere `max` Zeichen eines Texts; bei Kürzung endet er mit „…“. Anders als
/// `truncate` (behält das Ende) bleibt hier der ANFANG einer Meldung lesbar.
pub(crate) fn take_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out: String = s.chars().take(budget).collect();
    out.push('…');
    out
}

/// Kurze, EINZEILIGE Fassung eines Server-Fehler-Bodys: bevorzugt `error.message`,
/// sonst das `error`-Objekt/String, sonst der erste nutzbare Text des Roh-Bodys.
/// Zeilenumbrüche und Tabs werden zu Leerzeichen – so landet nie ein ganzer
/// JSON-Baum auf dem Bildschirm.
pub(crate) fn server_error_summary(raw: &str, max: usize) -> String {
    let text = if let Ok(json) = serde_json::from_str::<Value>(raw) {
        match json.get("error") {
            Some(err) => error_message(err),
            None => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    take_head(&joined, max)
}

/// Ergänzt die Kurzfassung um eine verständliche Erklärung, wenn der Endpunkt
/// den Thinking-Mode-Vertrag meldet („`reasoning_content` must be passed back
/// to the API.“). Das ist DeepSeek-Doku-Verhalten bei Requests mit `tools`:
/// assistant-Nachrichten mit Tool-Calls MÜSSEN das Feld tragen – seit dem Fix
/// wird es immer gesetzt (ggf. leer). Trifft der Fehler trotzdem ein, weist
/// die Meldung auf Ursache und mögliche Workarounds hin.
pub(crate) fn reasoning_contract_hint(summary: &str) -> String {
    let lower = summary.to_lowercase();
    if lower.contains("reasoning_content") && lower.contains("passed back") {
        format!(
            "{summary} (Note: DeepSeek \"thinking mode\" requires that every \
             assistant tool call gets `reasoning_content` passed back; aidev now always sets \
             the field – empty string if needed. If the error persists, a new chat or a \
             session compaction may help.)"
        )
    } else {
        summary.to_string()
    }
}

/// Ergänzt eine Kurzfassung um den Pfad des gespeicherten Request/Response-
/// Debug-Materials als eigene Zeile (nur wenn vorhanden). Die UI trennt daran
/// lesbare Meldung und Speicherort voneinander.
pub(crate) fn with_debug(summary: String, debug_path: Option<String>) -> String {
    match debug_path {
        Some(p) => format!("{summary}\n{p}"),
        None => summary,
    }
}

/// Debug-Verzeichnis: `$XDG_DATA_HOME/aidev/debug` bzw. `~/.local/share/aidev/debug`.
pub(crate) fn debug_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("aidev").join("debug"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("aidev")
            .join("debug"),
    )
}

/// Diskutabel robustes Datum: civil_from_days (Howard Hinnant) wandelt
/// Tage-since-Epoche in Jahr/Monat/Tag um – ohne externe Zeit-Bibliothek.
pub(crate) fn civil_from_days(z_days: i64) -> (i64, u32, u32) {
    let z = z_days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 {
        yoe as i64 + era * 400 + 1
    } else {
        yoe as i64 + era * 400
    };
    (year, month, day)
}

/// Zeitstempel `JJJJ-MM-TT_HH-MM-SS.mmm` (UTC) für aufsteigend sortierbare
/// Debug-Ordner.
pub(crate) fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}_{h:02}-{mi:02}-{s:02}.{millis:03}")
}

/// Schreibt Request (als Pretty-JSON) und Antwort-Body (roh) eines
/// fehlgeschlagenen Chat-Aufrufs als Debug-Material ab – für spätere
/// Nachvollziehbarkeit. Liefert den Ordnerpfad, sonst `None`. In Tests wird
/// nichts geschrieben.
pub(crate) fn dump_debug(
    label: &str,
    model: &str,
    url: &str,
    request: &Value,
    response: Option<&str>,
    error: Option<&str>,
) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let base = debug_dir()?;
    let sub = base.join(format!("{}_{}", timestamp(), label));
    std::fs::create_dir_all(&sub).ok()?;
    std::fs::write(
        sub.join("request.json"),
        serde_json::to_string_pretty(request).unwrap_or_default(),
    )
    .ok()?;
    if let Some(resp) = response {
        std::fs::write(sub.join("response.txt"), resp).ok()?;
    }
    std::fs::write(
        sub.join("meta.txt"),
        format!(
            "time:    {}\nmodel:   {model}\nurl:     {url}\nerror:   {}\n",
            timestamp(),
            error.unwrap_or("–")
        ),
    )
    .ok()?;
    Some(sub.display().to_string())
}

/// Kürzt zu lange Anzeige-Ausgabe (nur die Darstellung; das Modell bekommt
/// stets den vollen Text). Über dem Limit wird von OBEN abgeschnitten: am Ende
/// bleibt alles bis zum Schluss lesbar, vorne werden ganze Zeilen verworfen und
/// die Kürzung mit „…“ markiert. Nur wenn schon eine einzelne Zeile allein das
/// Limit sprengt, bleibt notgedrungen nur ihr Ende statt ganzer Zeilen übrig.
/// Der Algorithmus liegt geteilt in `crate::text`; hier kommt nur das Trimmen
/// der Eingabe und der feste Marker dazu.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    crate::text::truncate_tail(s.trim(), max, "…\n")
}
