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

use crate::app::{App, Phase, Session};
use crate::channel::ChannelStatus;

use super::{
    tool_icon, tool_name, ACCENT, ACCENT_FG, ERROR_FG, MUTED, SPINNER, STATUS_BG, SYM_ERR,
    SYM_MUTED, SYM_OK, SYM_WARN,
};

pub(crate) fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let meta = metadata_line(app);
    let rlen = meta.width() as u16;
    let cols = Layout::horizontal([Constraint::Fill(1), Constraint::Max(rlen)]).split(area);

    let left = Paragraph::new(status_left(app, cols[0].width as usize))
        .style(Style::default().bg(STATUS_BG));
    f.render_widget(left, cols[0]);

    let right = Paragraph::new(meta)
        .alignment(Alignment::Right)
        .style(Style::default().bg(STATUS_BG));
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
        let left = retry_at.saturating_duration_since(Instant::now()).as_secs() + 1;
        return muted_line(
            format!(" {ch} {summary} – Retry in {left}s"),
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
        muted_line(format!(" {ch} thinking…"), ACCENT_FG)
    } else if let Some(err) = &s.error {
        muted_line(format!(" {err}"), Color::Rgb(240, 113, 120))
    } else if s.aborted {
        muted_line(" Aborted".to_string(), ACCENT_FG)
    } else {
        key_help(&STATUS_KEYS, max_width)
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
    let key_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);
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

/// Aktuelle Context-Größe der Session für die Statuszeile. Sobald live neue
/// Daten eintreffen (mid-stream `UsageUpdate` bzw. Runden-`Usage`), zeigt die
/// Statusleiste Deren `total_tokens` an dieser Stelle – nicht die der letzten
/// abgeschlossenen Runde. Nur solange noch kein Live-Wert vorliegt (Turn-Start,
/// noch keine neuen Daten), fällt sie auf die letzte abgeschlossene Runde bzw.
/// die `prompt_base`-Basis zurück.
fn context_tokens(s: &Session) -> Option<u64> {
    // Live-Wert hat Vorrang, sobald er gesetzt ist: neue Daten zeigen sofort
    // deren `total_tokens`, statt bis zum Rundenende zu warten.
    if let Some(t) = s.live_usage_total.filter(|&t| t > 0) {
        return Some(t);
    }
    if let Some(u) = s.last_usage() {
        return Some(u.total_tokens);
    }
    (s.prompt_base > 0).then_some(s.prompt_base)
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
    let muted = Style::default().fg(MUTED);
    // Gewähltes Modell der Session: Alias (falls per /model gewählt), sonst
    // die konfigurierte Modell-ID.
    let mut parts: Vec<Span<'static>> = vec![Span::styled(app.display_model(app.active), muted)];
    if let Some(t) = context_tokens(s) {
        parts.push(Span::styled(format!(" · {}", fmt_ctx(t)), muted));
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
            let color = if *clean { MUTED } else { ERROR_FG };
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
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
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
        ChannelStatus::Running => SYM_OK,
        ChannelStatus::Starting => SYM_WARN,
        ChannelStatus::Problem => SYM_ERR,
        ChannelStatus::Unknown => SYM_MUTED,
    }
}
