//! Statuszeile (unterer Rand) samt Git-Status-Info.
//!
//! `git_status_info` und `GitStatusInfo` werden auch von `app.rs` genutzt und
//! über `mod.rs` (`pub(crate) use status::*;`) re-exportiert.

use std::time::Instant;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{prompt_tokens, App, Phase, Session};
use crate::channel::ChannelStatus;

use super::{fmt_duration, theme, tool_icon, tool_name, SPINNER};

pub(crate) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let meta = metadata_line(app);
    let rlen = meta.width() as u16;
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Max(rlen)]).split(area);

    let left = Paragraph::new(status_left(app, cols[0].width as usize))
        .style(Style::default().bg(theme().status_bg));
    f.render_widget(left, cols[0]);

    let right = Paragraph::new(meta)
        .alignment(Alignment::Right)
        .style(Style::default().bg(theme().status_bg));
    f.render_widget(right, cols[1]);
}

/// Linke Hälfte der Statuszeile: Zustand des Chats.
pub(crate) fn status_left(app: &App, max_width: usize) -> Line<'static> {
    let s = &app.sessions[app.active];
    let muted_line =
        |text: String, fg: Color| Line::from(Span::styled(text, Style::default().fg(fg)));
    // Blockierende Bestätigung: Die Arbeit einer Session ist pausiert, bis der
    // User entscheidet. Kein Spinner – stattdessen ein deutlicher Hinweis, dass
    // eine Entscheidung aussteht (mit dem run-Befehl, falls bekannt).
    if app.session_awaits_decision(app.active) {
        let hint = match &app.exec_confirm {
            Some(d) => format!(" ⏸ awaiting your decision: {} …", d.label),
            None => " ⏸ awaiting your decision…".to_string(),
        };
        return muted_line(hint, Color::Rgb(245, 158, 11));
    }
    if let Some(tool) = &s.active_tool_label {
        let ch = SPINNER[app.spinner % SPINNER.len()];
        return muted_line(
            format!(" {ch} {} {tool} …", tool_icon(tool_name(tool))),
            Color::Rgb(245, 158, 11),
        );
    }
    if let Some((summary, retry_at)) = &s.retrying {
        let ch = SPINNER[app.spinner % SPINNER.len()];
        // Countdown in Millisekunden, aufgerundet auf die nächste volle Sekunde –
        // so zeigt `fmt_duration` ganze Sekunden und nie „0 s“/Bruchteile.
        let left_ms = retry_at
            .saturating_duration_since(Instant::now())
            .as_millis() as u64;
        let next_sec = (left_ms / 1000 + 1) * 1000;
        return muted_line(
            format!(" {ch} {summary} – Retry in {}", fmt_duration(next_sec)),
            Color::Rgb(245, 158, 11),
        );
    }
    if s.compacting {
        let ch = SPINNER[app.spinner % SPINNER.len()];
        return muted_line(
            format!(" {ch} Compacting context…"),
            Color::Rgb(245, 158, 11),
        );
    }
    if s.phase == Phase::WaitingForLLM {
        let ch = SPINNER[app.spinner % SPINNER.len()];
        streaming_line(s, ch)
    } else if let Some(err) = &s.error {
        muted_line(format!(" {err}"), Color::Rgb(240, 113, 120))
    } else if s.aborted {
        muted_line(" Aborted".to_string(), theme().highlight)
    } else if app.any_dialog_open() {
        // Solange ein Dialog (Picker/Options/Builder/Bestätigung) die Tastatur
        // beansprucht, keine Haupt-Tastenkürzel in der Chat-Statuszeile zeigen –
        // der Dialog trägt seine eigene Tasten-Hilfe. Läuft die Session gerade
        // („thinking…“, Tool, Retry, …), steht das weiter oben schon; nur im
        // schlichten Idle bleibt die Zeile leer.
        Line::default()
    } else {
        key_help(&STATUS_KEYS, max_width)
    }
}

/// Linke Hälfte während `WaitingForLLM`: Solange kein erstes Inhalt-Byte da
/// ist, zählt „thinking… Ns“ die Wartezeit hoch (ab dem Runden-/Request-Start,
/// Fallback `sent_at`). Sobald Daten eintreffen, wird die Zeit auf den
/// gemessenen TTFT-Wert eingefroren und stattdessen die aktuelle TPS-Rate
/// angezeigt (Tokens bis jetzt / Zeit seit dem ersten Token).
fn streaming_line(s: &Session, ch: &str) -> Line<'static> {
    let text = match s.ttft_ms {
        Some(ttft) => {
            let rate = live_tps(s);
            format!(" {ch} thinking… {} · {} tps", fmt_duration(ttft), rate)
        }
        None => {
            let waited = s
                .round_started_at
                .or(s.sent_at)
                .map_or(0, |t| t.elapsed().as_millis() as u64);
            format!(" {ch} thinking… {}", fmt_duration(waited))
        }
    };
    Line::from(Span::styled(text, Style::default().fg(theme().highlight)))
}

/// Live-Tokenrate der laufenden Runde: `stream_tokens` (bestätigte
/// usage-Inkremente + Schätzung) geteilt durch die Zeit seit dem ersten Token.
fn live_tps(s: &Session) -> String {
    let elapsed = s.first_token_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
    if elapsed <= 0.0 {
        return fmt_tps(0.0);
    }
    fmt_tps(s.stream_tokens as f64 / elapsed)
}

/// Token-pro-Sekunde kompakt mit „.“ als Dezimaltrenner, z. B. „34.2“,
/// „120“, „1.1k“ – bewusst ohne „ tps“-Suffix (das hängt der Aufrufer an).
pub(crate) fn fmt_tps(rate: f64) -> String {
    if rate >= 1000.0 {
        let v = rate / 1000.0;
        format!("{v:.1}k")
    } else if rate >= 100.0 {
        format!("{rate:.0}")
    } else {
        format!("{rate:.1}")
    }
}

/// Tastenkürzel des Hauptfensters für die Statusleiste (in Anzeige-Reihenfolge).
const STATUS_KEYS: [(&str, &str); 7] = [
    ("ctrl+n", "new session"),
    ("alt+⇄", "switch session"),
    ("alt+c", "choose channel"),
    ("alt+m", "choose model"),
    ("tab", "permission"),
    ("alt+±", "zoom"),
    ("ctrl+o", "options"),
];

/// Tasten-Hilfe des Kanal-Pickers (wählen / bestätigen / schließen / abbrechen).
pub(crate) const CHANNEL_PICKER_KEYS: [(&str, &str); 4] = [
    ("⇅", "select"),
    ("↵", "confirm"),
    ("del", "close channel"),
    ("esc", "cancel"),
];

/// Gemeinsame Tasten-Hilfe der Auswahl-/Bestätigungsdialoge (Modell-Picker,
/// Bestätigungsdialoge): wählen / übernehmen / abbrechen.
pub(crate) const NAV_CONFIRM_KEYS: [(&str, &str); 3] =
    [("⇅", "select"), ("↵", "confirm"), ("esc", "cancel")];

/// Tasten-Hilfe des Modell-Pickers: wählen / bestätigen / Refresh / abbrechen.
pub(crate) const MODEL_PICKER_KEYS: [(&str, &str); 4] = [
    ("⇅", "select"),
    ("↵", "confirm"),
    ("r", "refresh"),
    ("esc", "cancel"),
];

/// Tasten-Hilfe der Bestätigung eines einzelnen `run`-Befehls:
/// wählen / bestätigen / ablehnen.
pub(crate) const EXEC_KEYS: [(&str, &str); 3] =
    [("⇅", "select"), ("↵", "confirm"), ("esc", "decline")];

/// Baut eine Tastatur-Hilfszeile aus `(Taste, Bedeutung)`-Zuordnungen und dem
/// maximal verfügbaren Platz (`max_width`, in Zeichen).
///
/// Jeder Eintrag wird als `Taste Bedeutung` formatiert – die Taste in
/// Akzentfarbe und fett (heller), die Bedeutung gedämpft – und mit ` · `
/// aneinandergereiht. Zuordnungen, die nicht mehr in den verfügbaren Platz
/// passen, werden in Anzeige-Reihenfolge weggelassen. Wird von der
/// Statusleiste und sämtlichen Dialogen gemeinsam genutzt.
pub(crate) fn key_help(bindings: &[(&str, &str)], max_width: usize) -> Line<'static> {
    let key_style = Style::default()
        .fg(theme().accent)
        .add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme().muted);
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut width = 1usize; // führendes Leerzeichen
    let mut first = true;
    for (key, meaning) in bindings {
        let sep = if first { 0 } else { 3 }; // " · "
        let entry_w = sep + key.chars().count() + 1 + meaning.chars().count();
        if width + entry_w > max_width {
            break; // passt nicht mehr → Taste weglassen
        }
        if !first {
            spans.push(Span::styled(" · ", muted));
            width += 3;
        }
        first = false;
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(format!(" {meaning}"), muted));
        width += key.chars().count() + 1 + meaning.chars().count();
    }
    Line::from(spans)
}

/// Vollständige (nicht gekürzte) Breite einer Tasten-Hilfe in Zeichen –
/// für die Dialog-Breitenberechnung, damit der Dialog auch bei kleinem
/// Terminal (fast) alle Kürzel unterbringen kann. Gekürzt wird erst beim
/// eigentlichen Rendern in [`key_help`].
pub(crate) fn key_help_full_width(bindings: &[(&str, &str)]) -> usize {
    let mut width = 1usize; // führendes Leerzeichen
    let mut first = true;
    for (key, meaning) in bindings {
        if !first {
            width += 3; // " · "
        }
        width += key.chars().count() + 1 + meaning.chars().count();
        first = false;
    }
    width
}

/// Aktuelle Context-Größe der Session für die Statuszeile samt „grün?“-Flag:
/// grün = serverbestätigt (live-Messung oder letzte abgeschlossene Runde mit
/// Usage), grau = Schätzung (z. B. nach Abbruch/Fehler der letzten Runde oder
/// nach einer Kompaktierung) – analog zur ersten Spalte rechts neben der
/// Usage-Bar in der Overview-Sicht.
///
/// Sobald live neue Daten eintreffen (mid-stream `UsageUpdate` bzw. Runden-
/// `Usage`), zeigt die Statusleiste deren `total_tokens` an dieser Stelle –
/// nicht die der letzten abgeschlossenen Runden. Hat die letzte Runde KEIN
/// bestätigtes Usage (z. B. vom User abgebrochen, mit einem Fehler beendet oder
/// vor einer Kompaktierung gemessen), liefert `last_usage_current` nichts.
/// Statt dann „0T“ zu zeigen, fällt die Anzeige auf die Schätzung `prompt_tokens`
/// zurück: die Kontextlänge des NEUESTEN Events der History + Schätzung der
/// seither angefügten Token (partieller Inhalte, neue Nachricht) – grau, weil
/// nicht bestätigt.
///
/// Nach einer Kompaktierung ist das der Normalfall: Live-Wert und Usage der
/// überlebenden Runden messen noch die alte, größere Historie. Die Statusleiste
/// springt deshalb mit auf den neuen Kontext – Summary + überlebende Turns, plus
/// was gerade in Verarbeitung ist – und zeigt ihn bis zur nächsten Runde mit
/// Usage als Schätzung (grau).
fn context_tokens(s: &Session) -> Option<(u64, bool)> {
    // Live-Wert hat Vorrang, sobald er gesetzt ist: neue Daten zeigen sofort
    // deren `total_tokens`, statt bis zum Rundenende zu warten.
    if let Some(t) = s.live_usage_total.filter(|&t| t > 0) {
        return Some((t, true));
    }
    // Letzte abgeschlossene Runde mit bestätigtem Usage. `last_usage_current`
    // liefert bewusst NICHTS, wenn diese Runde abgebrochen/fehlgeschlagen ist
    // oder vor einer Kompaktierung gemessen wurde (dann käme „0T“ bzw. der
    // alte, zu große Wert) – wir springen direkt auf die Schätzung.
    if let Some(u) = s.last_usage_current().filter(|u| u.total_tokens > 0) {
        return Some((u.total_tokens, true));
    }
    // Nach (oder ohne) Compaction / nach abgebrochener letzter Runde:
    // geschätzte Kontext-Tokens aus dem Event-Log (Kontextlänge des neuesten
    // Events – nach einer Kompaktierung also Summary + Überlebende – plus die
    // Beiträge der offenen Events), sonst `prompt_base` als obere Schranke.
    let est = prompt_tokens(s);
    if est > 0 {
        return Some((est, false));
    }
    (s.prompt_base > 0).then_some((s.prompt_base, false))
}

/// Kompakte Darstellung einer Token-Zahl für die Statuszeile: max. 3 signifikante
/// Stellen, z. B. 45_100 → „45.1kT“, 1_200_000 → „1.2MT“, 10_000 → „10kT“.
pub(crate) fn fmt_ctx(n: u64) -> String {
    if n < 1_000 {
        return format!("{n}T");
    }
    let (v, unit) = if n < 1_000_000 {
        (n as f64 / 1_000.0, "k")
    } else {
        (n as f64 / 1_000_000.0, "M")
    };
    // Max. 3 signifikante Stellen; überflüssige Nachkommastellen entfernen
    // („10.0“ → „10“, „1.20“ → „1.2“), ganze Hunderter bleiben („100“).
    let body = if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        let s = format!("{v:.1}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    };
    // Rundungs-Überlauf (999,9 k → „1000“) auf die nächste Einheit heben,
    // damit nie mehr als 3 signifikante Stellen stehen.
    if body == "1000" && unit == "k" {
        return "1MT".to_string();
    }
    format!("{body}{unit}T")
}

/// Rechte Hälfte: Modell, Context-Umfang (live), Kanal (mit Statusfarbe),
/// Git-Status (fallsRepo), Arbeitsverzeichnis.
pub(crate) fn metadata_line(app: &App) -> Line<'static> {
    let s = &app.sessions[app.active];
    let muted = Style::default().fg(theme().muted);
    // Gewähltes Modell der Session: Alias (falls per /model gewählt), sonst
    // die konfigurierte Modell-ID.
    let mut parts: Vec<Span<'static>> = vec![Span::styled(app.display_model(app.active), muted)];
    if let Some((t, green)) = context_tokens(s) {
        // Kontext-Größe analog zur Overview-Spalte: grün = serverbestätigt
        // (live oder letzte Runde mit Usage), grau = Schätzung (Abbruch/Fehler
        // der letzten Runde, Kompaktierung).
        let ctx_color = if green {
            Style::default().fg(theme().ok)
        } else {
            muted
        };
        parts.push(Span::styled(format!(" · {}", fmt_ctx(t)), ctx_color));
    }
    if let Some(ch) = &s.channel {
        parts.push(Span::styled(" · ", muted));
        parts.push(Span::styled(
            format!("⬢ {}", ch.root()),
            Style::default().fg(channel_status_color(ch.status())),
        ));
        // Git-Status (gecached, siehe `App::refresh_git_status`): branch@reponame
        // (grau wenn clean, rot wenn dirty). Der Cache ist nur gesetzt, wenn ein
        // Git-Repo gebunden ist.
        if let Some((label, clean)) = &app.git_status_cache {
            let color = if *clean { theme().muted } else { theme().err };
            parts.push(Span::styled(" · ", muted));
            parts.push(Span::styled(label.clone(), Style::default().fg(color)));
        }
    }
    // Zoom-Level-Anzeige: ▂▄▆█ (von „summary"/Overview bis „detailed",
    // entgegengesetzt zur bisherigen Reihenfolge, ohne Trennblanks).
    {
        use crate::app::ViewLevel;
        let view = s.view;
        let symbols = [
            (ViewLevel::Overview, "▂"),
            (ViewLevel::Compact, "▄"),
            (ViewLevel::Dialog, "▆"),
            (ViewLevel::Detailed, "█"),
        ];
        parts.push(Span::styled(" · - ", muted));
        for (level, sym) in symbols.iter() {
            if *level == view {
                // Aktiver Zoom: hervorgehoben
                parts.push(Span::styled(
                    *sym,
                    Style::default()
                        .fg(theme().accent)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                parts.push(Span::styled(*sym, muted));
            }
        }
        parts.push(Span::styled(" +", muted));
    }
    Line::from(parts)
}

/// Git-Status-Information für die Statusleiste.
pub(crate) struct GitStatusInfo {
    pub(crate) label: String,
    pub(crate) clean: bool,
}

/// Ermittelt Git-Status für ein Verzeichnis: `branch@reponame` + sauber/schmutzig.
/// Branch und Dirty-Status werden vom Worktree (`host_root`) gelesen,
/// der Repo-Name kommt vom Top-Level-Repo.
///
/// Wird von `App::refresh_git_status` (höchstens 1×/s) aufgerufen; die
/// Statusleiste liest das Ergebnis aus dem Cache, nicht direkt hier.
pub(crate) fn git_status_info(host_root: &std::path::Path) -> Option<GitStatusInfo> {
    // Nur wenn das Verzeichnis DIREKT eine Repo-Wurzel bzw. ein Worktree ist
    // (`.git` als Verzeichnis/Datei), gibt es einen Git-Status. Unter-
    // verzeichnisse von Repos und repo-freie Host-Ordner zeigen nichts.
    if !crate::channel::builder::is_repo_root(host_root) {
        return None;
    }
    // Repo-Name vom Top-Level
    let toplevel = crate::repo::git_toplevel(host_root).ok()?;
    let repo_name = toplevel
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "?".to_string());
    // Branch und Dirty-Status vom Worktree selbst
    let branch = crate::repo::git_current_branch(host_root)?;
    let clean = crate::repo::git_is_clean(host_root).unwrap_or(true);
    Some(GitStatusInfo {
        label: format!("{branch}@{repo_name}"),
        clean,
    })
}

/// Semantische Farbe des `⬢`-Kanal-Indikators je nach Zustand.
pub(crate) fn channel_status_color(status: ChannelStatus) -> Color {
    match status {
        ChannelStatus::Running => theme().ok,
        ChannelStatus::Starting => theme().warn,
        ChannelStatus::Problem => theme().err,
        ChannelStatus::Unknown => theme().muted,
    }
}

#[cfg(test)]
mod tests {
    use super::context_tokens;
    use crate::app::prompt_tokens;
    use crate::app::Session;
    use crate::llm::Usage;
    use crate::perm::Permission;

    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            total_tokens: p + c,
            cached_tokens: None,
        }
    }

    #[test]
    fn abgeschlossener_turn_zeigt_bestaetigt_gruen() {
        let mut s = Session::new(0);
        s.push_user_message("frage".into(), Some(Permission::Read), "m".into());
        let a = s.open_assistant("gedanke".into(), "antwort".into());
        s.chat
            .finalize_assistant(a, std::time::Instant::now(), usage(90_000, 10_000), 0, 0, false);
        let (t, green) = context_tokens(&s).expect("Kontext vorhanden");
        assert_eq!(t, 100_000, "bestätigter Usage als Anker");
        assert!(green, "bestätigt → grün");
    }

    /// Die letzte Runde wurde abgebrochen (kein Usage): statt „0T“ zeigt die
    /// Statuszeile die Schätzung = letzte bestätigte Kontextlänge + seither
    /// angefügte Tokens (grau, nicht bestätigt).
    #[test]
    fn abgebrochene_letzte_runde_faellt_auf_schaetzung_statt_0() {
        let mut s = Session::new(0);
        // Abgeschlossener Turn mit Usage (Anker 100_000).
        s.push_user_message("frage eins".into(), Some(Permission::Read), "m".into());
        let a1 = s.open_assistant("g1".into(), "antwort eins".into());
        s.chat
            .finalize_assistant(a1, std::time::Instant::now(), usage(90_000, 10_000), 0, 0, false);
        // Nächster Turn wird abgebrochen → geschlossen mit Null-Usage.
        s.push_user_message("frage zwei".into(), Some(Permission::Read), "m".into());
        let a2 = s.open_assistant("halb fertig".into(), String::new());
        s.chat.finalize_assistant(
            a2,
            std::time::Instant::now(),
            usage(0, 0),
            0,
            0,
            true,
        );
        // Neuer Prompt bereits gesendet (WaitingForLLM, live noch leer).
        s.push_user_message("frage drei".into(), Some(Permission::Read), "m".into());
        let (t, green) = context_tokens(&s).expect("Kontext vorhanden");
        assert!(!green, "nach Abbruch → Schätzung (grau)");
        assert!(t > 0, "kein 0T nach Abbruch: {t}");
        // Deutlich unter beiden Altwerten (Live 190_000, letzte Usage 170_000).
        assert!(t < 170_000, "deutlich unter den Altwerten: {t}");
    }

    /// Nach einer Kompaktierung beschreiben die alten Werte den aktuellen
    /// Kontext nicht mehr: der zuletzt *live* gemeldete `total_tokens`-Wert und
    /// die Usage der überlebenden Runden (beide gegen die alte, größere Historie
    /// gemessen). Die Statusleiste muss auf den neuen Kontext springen – den
    /// Wert des NEUESTEN Events der History (nach dem Shift) plus die Schätzung
    /// der noch offenen Events, also `prompt_tokens` – und ihn als Schätzung
    /// (grau) kennzeichnen.
    #[test]
    fn kompaktierung_springt_auf_den_neuen_kontext() {
        let turn = |s: &mut Session, frage: &str, p: u64, c: u64| {
            s.push_user_message(frage.into(), Some(Permission::Read), "m".into());
            let a = s.open_assistant("gedanke".into(), "antwort".into());
            s.chat
                .finalize_assistant(a, std::time::Instant::now(), usage(p, c), 0, 0, false);
        };
        let mut s = Session::new(0);
        turn(&mut s, "frage eins", 90_000, 10_000); // Kontext 100_000
        turn(&mut s, "frage zwei", 150_000, 20_000); // Kontext 170_000
        s.live_usage_total = Some(190_000); // Live-Messung der laufenden Runde
        assert_eq!(context_tokens(&s).expect("live").0, 190_000);

        // Reaktive Kompaktierung: Turn 1 wird durch die Summary (1_000 Token)
        // ersetzt, Turn 2 überlebt. Dessen `context_len` wandert um den
        // abgeschnittenen Teil (100_000 - 1_000) nach unten.
        s.apply_compaction("zusammenfassung".into(), 1_000, 0, None);
        let erwartet = prompt_tokens(&s);
        let (t, green) = context_tokens(&s).expect("Kontext vorhanden");
        assert!(!green, "nach Kompaktierung → Schätzung (grau)");
        assert_eq!(t, erwartet, "Statusleiste folgt der History");
        assert_eq!(
            t, 71_000,
            "Summary + überlebender Turn, ohne den alten Rest"
        );
        // Deutlich unter beiden Altwerten (Live 190_000, letzte Usage 170_000).
        assert!(t < 170_000, "deutlich unter den Altwerten: {t}");

        // Offene (noch verarbeitete) Nachricht: ihr Beitrag kommt obendrauf.
        s.open_assistant("halb".into(), String::new());
        let offen = s.chat.order().last().copied().expect("offenes Event");
        let (t_laufend, _) = context_tokens(&s).expect("Kontext vorhanden");
        assert_eq!(
            t_laufend,
            erwartet + s.chat.estimate_contribution(offen),
            "in Verarbeitung befindliche Nachricht zählt als Schätzung dazu"
        );
        assert!(
            t_laufend > erwartet,
            "offene Nachricht erhöht die Anzeige: {t_laufend}"
        );
    }
}
