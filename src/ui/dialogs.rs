//! Zentrierte Overlay-Dialoge: Kanal-Auswahl, Modell-Auswahl, Channel Builder
//! und Bestätigungsdialoge (Beenden, Worktree, Container, lokale Ausführung).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, BuilderEdit, ChannelPick, ListNav, ModelPick, Phase, Session};
use crate::channel::{ChannelRegistry, ChannelStatus};

use super::*;

/// Breiten-/Größen-Vorgabe eines Auswahl-Overlays.
pub(crate) struct OverlayCfg {
    /// Mindestbreite des Dialogs.
    pub min_width: u16,
    /// Maximalbreite (vor dem Rand-Abzug zur Terminalbreite) – nur für den
    /// `fraction`-Modus relevant.
    pub max_width: u16,
    /// `Some(n)` → feste Breite = `n/10` der Terminalbreite (Bestätigungs-/
    /// Builder-Stil); `None` → Breite am Inhalt orientieren (Picker-Stil).
    pub fraction: Option<u16>,
}

/// Gemeinsames, zentriertes Rendern einer scrollbaren Auswahlliste.
///
/// Zeichnet Hintergrund mit Innenrand, Titel, die (ggf. gescrollten) Einträge,
/// optionale Zusatzzeilen (`extras`, z. B. „… fetching models …“) und die
/// Tasten-Hilfe. Reicht die Liste nicht in die verfügbare Höhe, scrollt sie
/// über den `nav`-Offset (`follow` hält den Cursor dabei sichtbar) und die
/// Fußzeile zeigt einen „▴ N–M/K ▾“-Hinweis.
///
/// `row_of(item, spaltenbreite)` liefert die (ggf. umgebrochene) Höhe eines
/// Eintrags, `render(item, markiert)` dessen Zeile, `width_of(item)` dessen
/// Anzeige-Breite für die Content-Fit-Dialogbreite. Reine Zeichenzählung –
/// ohne Terminal testbar.
/// Bewusst viele Parameter: ein Render-Widget, das alle Werte effizient in
/// einem Rendering-Durchlauf verbraucht (statt sie in einem zusätzlichen
/// Konfig-Objekt zu bündeln).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_list_overlay<T>(
    f: &mut Frame,
    area: Rect,
    cfg: &OverlayCfg,
    nav: &mut ListNav,
    items: &[T],
    title: &str,
    title_fg: Color,
    row_of: impl Fn(&T, u16) -> u16,
    render: impl Fn(&T, bool) -> Line<'static>,
    width_of: impl Fn(&T) -> usize,
    footer: &[(&str, &str)],
    extras: &[Line<'static>],
) {
    let pad = PICKER_PAD as u16;
    let max_w = area.width.saturating_sub(4).max(cfg.min_width);

    // --- Breite: fest (Fraction) oder am Inhalt orientiert ---
    // Im Content-Fit-Fall wird der Inhalt aller Einträge vermessen (auch der
    // nicht sichtbaren), damit nichts rechts abgeschnitten wird.
    let width = match cfg.fraction {
        Some(fr) => (area.width * fr / 10).clamp(cfg.min_width, cfg.max_width.min(max_w)),
        None => {
            let content_w = format!(" {title} ")
                .chars()
                .count()
                .max(key_help_full_width(footer))
                .max(extras.iter().map(line_width).max().unwrap_or(0))
                .max(items.iter().map(width_of).max().unwrap_or(0));
            ((content_w + PICKER_PAD * 2) as u16).clamp(cfg.min_width, max_w)
        }
    };

    // --- Höhe: natürlich (alle Einträge) oder geklemmt (dann wird gescrollt) ---
    let natural = items.len() as u16 + extras.len() as u16 + 4;
    let max_height = area.height.saturating_sub(6).max(5);
    let height = natural.min(max_height).max(5);
    let inner_width = width.saturating_sub(pad * 2);
    // Liste: Höhe − 2 (Rahmen) − Titel(1) − extras − Fußzeile(1), mind. 1.
    let viewport = height
        .saturating_sub(2)
        .saturating_sub(1 + extras.len() as u16 + 1)
        .max(1);
    // Cursor im Fenster halten; `row_of` berücksichtigt mehrzeilige Einträge.
    nav.follow(viewport, |i| row_of(&items[i], inner_width));
    let range = nav.visible(viewport);

    // --- Overlay-Rahmen ---
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(theme().status_bg));
    f.render_widget(bg, rect);
    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        inner_width,
        height.saturating_sub(2),
    );

    // --- Inhalt ---
    let title_line = Line::from(Span::styled(
        format!(" {title} "),
        Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
    ));
    let mut lines: Vec<Line> = Vec::with_capacity(2 + range.len() + extras.len());
    lines.push(title_line);
    for pos in range.clone() {
        let selected = pos == nav.cursor();
        lines.push(render(&items[pos], selected));
    }
    lines.extend(extras.iter().cloned());
    let hint = if nav.offset() > 0 || range.end < items.len() {
        Some(scroll_hint(&range, items.len()))
    } else {
        None
    };
    lines.push(footer_line(footer, inner_width as usize, hint));

    let para = Paragraph::new(lines).style(Style::default().bg(theme().status_bg));
    f.render_widget(para, inner);
}

/// Anzeige-Breite einer (bereits gebauten) Zeile in Zeichen.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// „▴ N–M/K ▾“-Vermerk für scrollende Listen: `▴` wenn oberhalb noch Einträge
/// stehen, `▾` wenn unterhalb noch welche folgen.
fn scroll_hint(range: &std::ops::Range<usize>, total: usize) -> String {
    let mut s = String::new();
    if range.start > 0 {
        s.push('▴');
    }
    s.push_str(&format!(" {}–{}/{}", range.start + 1, range.end, total));
    if range.end < total {
        s.push_str(" ▾");
    }
    s
}

/// Tasten-Hilfszeile – mit rechtsbündig angehängtem Scroll-Hinweis, falls
/// übergeben und noch Platz vorhanden.
fn footer_line(bindings: &[(&str, &str)], max_width: usize, hint: Option<String>) -> Line<'static> {
    let kh = key_help(bindings, max_width);
    let Some(h) = hint else {
        return kh;
    };
    let used: usize = kh.spans.iter().map(|s| s.content.chars().count()).sum();
    let avail = max_width.saturating_sub(used);
    if avail >= h.chars().count() {
        let mut spans = kh.spans;
        spans.push(Span::raw(" ".repeat(avail - h.chars().count())));
        spans.push(Span::styled(h, Style::default().fg(theme().muted)));
        Line::from(spans)
    } else {
        kh
    }
}

/// Zentrierter Auswahl-Dialog für den Kanal der aktiven Session.
pub(crate) fn draw_channel_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = app.channel_picker.as_mut() else {
        return;
    };
    let area = f.area();
    let channels = &app.channels;
    let sessions = &app.sessions;
    let cfg = OverlayCfg {
        min_width: 24,
        max_width: 80,
        fraction: None,
    };
    draw_list_overlay(
        f,
        area,
        &cfg,
        &mut picker.items.nav,
        &picker.items.items,
        "Choose channel for this session",
        theme().accent,
        |_, _| 1,
        |item, sel| match item {
            ChannelPick::NoChannel => channel_picker_row("(no channel)", None, sel, ""),
            ChannelPick::NewChannel => channel_picker_new_channel_row(sel),
            ChannelPick::Channel { name } => {
                let status = channels.get(name).map(|ch| ch.status());
                let usage = channel_usage(sessions, channels, name);
                channel_picker_row(name, status, sel, &usage)
            }
        },
        |item| channel_pick_width(sessions, channels, item),
        &CHANNEL_PICKER_KEYS,
        &[],
    );
}

/// Anzeige-Breite einer Kanal-Picker-Zeile (Indikator 1 + „ {name}" + ggf.
/// „  {usage}") – für die Content-Fit-Dialogbreite.
fn channel_pick_width(
    sessions: &[Session],
    channels: &ChannelRegistry,
    item: &ChannelPick,
) -> usize {
    match item {
        ChannelPick::NoChannel => 1 + 1 + "(no channel)".chars().count(),
        ChannelPick::NewChannel => 1 + 1 + "new channel".chars().count(),
        ChannelPick::Channel { name } => {
            let usage = channel_usage(sessions, channels, name);
            let mut w = 1 + 1 + name.chars().count();
            if !usage.is_empty() {
                w += 2 + usage.chars().count();
            }
            w
        }
    }
}

/// Zentrierter Auswahl-Dialog für das Modell der aktiven Session (`/model`).
/// „(Standard)" erscheint als echter Eintrag, wenn das Default-Modell bei den
/// konfigurierten Modellen fehlt.
pub(crate) fn draw_model_picker(f: &mut Frame, app: &mut App) {
    let Some(picker) = app.model_picker.as_mut() else {
        return;
    };
    let area = f.area();
    let registry = &app.model_registry;
    let extras: Vec<Line<'static>> = if picker.loading {
        vec![Line::from(Span::styled(
            " … fetching models …",
            Style::default()
                .fg(theme().muted)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        Vec::new()
    };
    let cfg = OverlayCfg {
        min_width: 28,
        max_width: 80,
        fraction: None,
    };
    draw_list_overlay(
        f,
        area,
        &cfg,
        &mut picker.items.nav,
        &picker.items.items,
        "Model for this session",
        theme().accent,
        |_, _| 1,
        |item, sel| match item {
            ModelPick::Default => model_picker_row("(default)", sel, None),
            ModelPick::Model { key, display } => {
                // Farblicher Status-Indikator aus der Model-Registry.
                let color = registry.status(key).map(|s| match s {
                    crate::app::models::ModelStatus::ConfigAndFetched => theme().ok, // grün
                    crate::app::models::ModelStatus::ConfigStale => theme().err,     // rot
                    crate::app::models::ModelStatus::FetchedOnly => theme().muted,   // grau
                });
                model_picker_row(display, sel, color)
            }
        },
        |item| match item {
            // +3 statt +1: Der ⬢-Indikator ist 2 Zellen breit + Leerzeichen.
            ModelPick::Default => 3 + "(default)".chars().count(),
            ModelPick::Model { display, .. } => display.chars().count() + 3,
        },
        &MODEL_PICKER_KEYS,
        &extras,
    );
}

/// Eine Zeile des Modell-Auswahl-Dialogs: Status-Indikator (⬢) in der Farbe
/// des Refresh-Status + Anzeigetext. Reine Funktion, damit sie ohne Terminal
/// testbar ist.
fn model_picker_row(display: &str, cursor: bool, status_color: Option<Color>) -> Line<'static> {
    let indicator = Span::styled(
        "\u{2b22}",
        Style::default().fg(status_color.unwrap_or(theme().muted)),
    );
    // Aufteilen in "provider/alias (name)" und "· demand".
    let (main, demand) = match display.split_once('·') {
        Some((m, d)) => (m.trim_end(), Some(d)),
        None => (display, None),
    };
    let base_fg = if display == "(default)" {
        theme().muted
    } else {
        theme().highlight
    };
    let gray = Style::default().fg(theme().muted);
    // Nur das "provider/alias"-Kürzel ist der markierte Text; ein dahinter-
    // stehender Modellname in Klammern und die Demand-Angabe sind grau.
    let (short, mut extra) = match main.find(" (") {
        Some(idx) if main.ends_with(')') => {
            let (short, name) = main.split_at(idx);
            (short, vec![Span::styled(name.to_string(), gray)])
        }
        _ => (main, Vec::new()),
    };
    if let Some(d) = demand {
        extra.push(Span::styled(format!(" ·{d}"), gray));
    }
    plain_row(indicator, short, base_fg, cursor, extra)
}

/// Channel Builder: Tunnel | Host/Repo | Worktree. Ohne Repo-Wurzel als
/// Host-Pfad entfällt die rechte Spalte (nur zwei Spalten). Jede Spalte hat
/// ihre eigene `ListNav`-Navigation (Umlauf, `with_wrap`); lange Spalten
/// scrollen über `follow`, damit der Cursor immer sichtbar bleibt.
pub(crate) fn draw_channel_builder(f: &mut Frame, app: &mut App) {
    let Some(builder) = app.channel_builder.as_mut() else {
        return;
    };
    let area = f.area();
    let width = (area.width * 9 / 10).clamp(60, 120);

    // Innenbreite – hängt nur von `width` ab (nicht von der Höhe) und wird
    // schon hier gebraucht, um die Spaltenbreiten und damit die
    // Umbruchszeilen der Inhalte zu bestimmen.
    let pad = PICKER_PAD as u16;
    let inner_width = width.saturating_sub(pad * 2);

    // Spalten-Breiten: Mit Worktree-Spalte drei gleiche Drittel. Ohne
    // (Host-Pfad ist keine Repo-Wurzel) bleiben Tunnel und Host an ihren
    // gewohnten Positionen – der Host nimmt dann aber die volle Restbreite
    // bis an den rechten Dialogrand ein.
    let show_worktrees = builder.current_is_git;
    let (tunnel_w, host_w, worktree_w) = if show_worktrees {
        let col_w = inner_width / 3;
        (col_w, col_w, inner_width.saturating_sub(col_w * 2))
    } else {
        let col_w = inner_width / 3;
        (col_w, inner_width.saturating_sub(col_w), 0)
    };

    // Spalten-Header
    let header_style = Style::default()
        .fg(theme().muted)
        .add_modifier(Modifier::BOLD);
    let tunnel_header = Line::from(Span::styled(
        format!(" {:<width$}", "Tunnel", width = tunnel_w as usize - 1),
        header_style,
    ));
    let host_header = Line::from(Span::styled(
        format!(
            " {:<width$}",
            "Directory / Repo on host",
            width = host_w as usize - 1
        ),
        header_style,
    ));
    let worktree_header = if show_worktrees {
        Some(Line::from(Span::styled(
            format!(
                " {:<width$}",
                "Worktree / Branch",
                width = worktree_w as usize - 1
            ),
            header_style,
        )))
    } else {
        None
    };

    // === Style-Konstanten ===
    // Aktive Spalte: cursor → White + Bold, kein Hintergrund
    let active_style = Style::default()
        .fg(theme().band_fg)
        .add_modifier(Modifier::BOLD);
    // Inaktive Spalte: genau ein markierter Eintrag
    let selected_style = Style::default().fg(theme().band_fg);
    // Restliche Einträge: gedämpft
    let normal_style = Style::default().fg(theme().highlight);
    // Branches ohne Worktree: noch gedämpfter
    let dim_style = Style::default().fg(theme().muted);

    // Tunnel-Spalte
    let tunnel_cursor = builder.tunnels.nav.cursor();
    let mut tunnel_rows: Vec<Line<'static>> = Vec::new();
    for (i, tunnel) in builder.tunnels.items.iter().enumerate() {
        let is_active_col = builder.col == 0;
        let is_selected = i == tunnel_cursor;
        let style = if is_active_col && is_selected {
            active_style
        } else if is_selected {
            selected_style
        } else {
            normal_style
        };
        let marker = if is_active_col && is_selected {
            "▶ "
        } else if is_selected {
            "● "
        } else {
            "  "
        };
        tunnel_rows.push(Line::from(Span::styled(
            format!("{}{}", marker, tunnel.label()),
            style,
        )));
    }
    // Lade-Indikator solange Podman-Images noch nicht geladen sind
    let tunnel_extra = if !builder.images_loaded {
        Some(Line::from(Span::styled(
            "  …",
            Style::default().fg(theme().muted),
        )))
    } else {
        None
    };

    // Host-Spalte
    let host_cursor = builder.host_paths.nav.cursor();
    let mut host_rows: Vec<Line<'static>> = Vec::new();
    for (i, hp) in builder.host_paths.items.iter().enumerate() {
        let is_active_col = builder.col == 1;
        let is_selected = i == host_cursor;
        let style = if is_active_col && is_selected {
            active_style
        } else if is_selected {
            selected_style
        } else {
            normal_style
        };
        let marker = if is_active_col && is_selected {
            "▶ "
        } else if is_selected {
            "● "
        } else {
            "  "
        };
        host_rows.push(Line::from(Span::styled(
            format!("{}{}", marker, hp.label()),
            style,
        )));
    }

    // Worktree-Spalte entfällt komplett, wenn der Host-Pfad keine
    // Repo-Wurzel ist.
    let worktree_cursor = builder.worktrees.nav.cursor();
    let worktree_rows: Vec<Line<'static>> = if show_worktrees {
        let mut rows = Vec::new();
        for (i, wt) in builder.worktrees.items.iter().enumerate() {
            let is_active_col = builder.col == 2;
            let is_selected = i == worktree_cursor;
            let base_style = if wt.has_worktree {
                normal_style
            } else {
                dim_style
            };
            let style = if is_active_col && is_selected {
                active_style
            } else if is_selected {
                selected_style
            } else {
                base_style
            };
            let marker = if is_active_col && is_selected {
                "▶ "
            } else if is_selected {
                "● "
            } else if wt.has_worktree {
                "  "
            } else {
                "○ "
            };
            rows.push(Line::from(Span::styled(
                format!("{}{}", marker, wt.label),
                style,
            )));
        }
        rows
    } else {
        Vec::new()
    };

    // Wie viele Zeilen braucht jede Spalte nach dem Umbruch langer Pfade?
    // Jede Zeile wird mit `wrapped_row_count` auf die Spaltenbreite
    // umgebrochen; dadurch rücken nachfolgende Einträge in der Spalte nach
    // unten (statt rechts abgeschnitten zu werden).
    let col_rows = |lines: &[Line<'_>], col_w: u16| -> u16 {
        lines.iter().map(|l| wrapped_row_count(l, col_w)).sum()
    };
    let mut tunnel_full = vec![tunnel_header.clone()];
    tunnel_full.extend(tunnel_rows.iter().cloned());
    if let Some(e) = &tunnel_extra {
        tunnel_full.push(e.clone());
    }
    let mut host_full = vec![host_header.clone()];
    host_full.extend(host_rows.iter().cloned());
    let mut max_rows = col_rows(&tunnel_full, tunnel_w).max(col_rows(&host_full, host_w));
    // Worktree-Spalte (inkl. Platzhalter „(no worktrees)" falls leer).
    let worktree_placeholder = if show_worktrees {
        Some(Line::from(Span::styled(
            "  (no worktrees)",
            Style::default().fg(theme().muted),
        )))
    } else {
        None
    };
    if let Some(hdr) = &worktree_header {
        let mut wt_full = vec![hdr.clone()];
        if worktree_rows.is_empty() {
            wt_full.push(worktree_placeholder.clone().expect("Placeholder bei Repo"));
        } else {
            wt_full.extend(worktree_rows.iter().cloned());
        }
        max_rows = max_rows.max(col_rows(&wt_full, worktree_w));
    }
    let max_rows = max_rows as usize;

    // Dialog-Höhe: ebenfalls am (umgebrochenen) Inhalt orientiert. Der
    // Spaltenbereich ist `inner.height - 4` (2 Titelzeilen + 1 Container-
    // Status + 1 Footer), also gilt height = Spaltenhöhe + 4; damit die
    // größte Spalte vollständig sichtbar ist: height = max_rows + 6.
    let height = (max_rows + 6).clamp(15, area.height.saturating_sub(2) as usize) as u16;
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    // Hintergrund
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(theme().status_bg));
    f.render_widget(bg, rect);

    // Innenrand
    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );

    // Titel
    let title = Line::from(Span::styled(
        " New channel ",
        Style::default()
            .fg(theme().accent)
            .add_modifier(Modifier::BOLD),
    ));

    // Spalten-Inhalte: Titel über den Spalten (Platz ist reserviert:
    // Spalten beginnen bei inner.y + 2 = Titel + Leerzeile).
    f.render_widget(
        Paragraph::new(vec![title, Line::from("")]).style(Style::default().bg(theme().status_bg)),
        Rect::new(inner.x, inner.y, inner.width, 2),
    );

    // Spalten-Layout: ohne Repo nur zwei Spalten (Worktree-Spalte entfällt).
    let mut constraints = vec![Constraint::Length(tunnel_w), Constraint::Length(host_w)];
    if show_worktrees {
        constraints.push(Constraint::Length(worktree_w));
    }
    let cols = Layout::horizontal(constraints).split(Rect::new(
        inner.x,
        inner.y + 2, // Nach Titel + Leerzeile
        inner.width,
        inner.height.saturating_sub(4), // Titel + Footer
    ));

    // Spalte rendern: Header + sichtbarer (gescrollter) Ausschnitt. `follow`
    // hält den Cursor sichtbar, `visible_rows` schneidet an Umbruchzeilen.
    let render_col = |f: &mut Frame,
                      area: Rect,
                      header: &Line<'static>,
                      nav: &mut ListNav,
                      rows: &[Line<'static>],
                      extra: Option<&Line<'static>>,
                      col_w: u16| {
        let extra_rows = if extra.is_some() { 1 } else { 0 };
        let viewport = area.height.saturating_sub(1 + extra_rows).max(1);
        let row_of = |i: usize| wrapped_row_count(&rows[i], col_w).max(1);
        nav.follow(viewport, row_of);
        let range = nav.visible_rows(viewport, row_of);
        let mut display = vec![header.clone()];
        for i in range {
            display.push(rows[i].clone());
        }
        if let Some(e) = extra {
            display.push(e.clone());
        }
        let para = Paragraph::new(display)
            .style(Style::default().bg(theme().status_bg))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    };
    render_col(
        f,
        cols[0],
        &tunnel_header,
        &mut builder.tunnels.nav,
        &tunnel_rows,
        tunnel_extra.as_ref(),
        tunnel_w,
    );
    render_col(
        f,
        cols[1],
        &host_header,
        &mut builder.host_paths.nav,
        &host_rows,
        None,
        host_w,
    );
    if show_worktrees {
        let wt_extra = if worktree_rows.is_empty() {
            worktree_placeholder.as_ref()
        } else {
            None
        };
        render_col(
            f,
            cols[2],
            worktree_header.as_ref().expect("Header bei Repo"),
            &mut builder.worktrees.nav,
            &worktree_rows,
            wt_extra,
            worktree_w,
        );
    }

    // Container-Status-Zeile (unterhalb der Spalten)
    if let Some(info) = &builder.container_info {
        let status_line = Line::from(Span::styled(
            format!(" Container: {} ({})", info.name, info.status),
            Style::default().fg(theme().muted),
        ));
        let status_rect = Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(2),
            inner.width,
            1,
        );
        f.render_widget(Paragraph::new(status_line), status_rect);
    }

    // Fußzeile: Tasten-Hilfe. Während ein Inline-Feld offen ist, zeigt sie
    // eine Beschreibung + `↵`/`esc`; im normalen Modus hängt die Kürzel-Liste
    // von der aktiven Spalte ab (`a` Pfad in der Host-, `b` Branch in der
    // Worktree-Spalte).
    let footer: Line<'static> = match &builder.edit {
        Some(BuilderEdit::Branch(_)) => {
            let prefix = " New branch ·";
            let mut spans = vec![Span::styled(prefix, Style::default().fg(theme().muted))];
            let budget = inner.width.saturating_sub(prefix.chars().count() as u16) as usize;
            spans.extend(key_help(&[("↵", "confirm"), ("esc", "cancel")], budget).spans);
            Line::from(spans)
        }
        Some(BuilderEdit::HostPath(_)) => {
            let prefix = " Enter a path ·";
            let mut spans = vec![Span::styled(prefix, Style::default().fg(theme().muted))];
            let budget = inner.width.saturating_sub(prefix.chars().count() as u16) as usize;
            spans.extend(key_help(&[("↵", "confirm"), ("esc", "cancel")], budget).spans);
            Line::from(spans)
        }
        None => {
            let mut keys: Vec<(&str, &str)> = vec![("⇅", "select"), ("⇄", "column")];
            match builder.col {
                1 => keys.push(("a", "add path")),
                2 if builder.current_is_git => keys.push(("b", "new branch")),
                _ => {}
            }
            keys.push(("↵", "confirm"));
            keys.push(("esc", "cancel"));
            key_help(&keys, inner.width as usize)
        }
    };
    let footer_rect = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(Paragraph::new(footer), footer_rect);

    // Inline-Eingabefeld: Host-Pfad unter der mittleren, Branch-Name unter
    // der rechten Spalte (an die Fußzeile gebunden, damit Cursor sichtbar).
    match &builder.edit {
        Some(BuilderEdit::HostPath(editor)) => {
            let input_x = cols[1].x;
            let input_w = if show_worktrees {
                (cols[2].x + cols[2].width).saturating_sub(cols[1].x)
            } else {
                cols[1].width
            };
            let footer_y = inner.y + inner.height.saturating_sub(1);
            let edit_row = cols[1].y + 1 + builder.host_paths.items.len() as u16 + 1;
            let entry_y = edit_row.min(footer_y.saturating_sub(2));
            if entry_y + 1 < footer_y {
                draw_builder_input_inline(f, input_x, entry_y, input_w, editor, "Add path", None);
            }
        }
        Some(BuilderEdit::Branch(editor)) => {
            let input_x = cols[2].x;
            let input_w = cols[2].width;
            let footer_y = inner.y + inner.height.saturating_sub(1);
            let edit_row = cols[2].y + 1 + builder.worktrees.items.len() as u16 + 1;
            // Platz für Label + Editor + ggf. rote Fehlerzeile reservieren.
            let entry_y = edit_row.min(footer_y.saturating_sub(3));
            if entry_y + 2 < footer_y {
                draw_builder_input_inline(
                    f,
                    input_x,
                    entry_y,
                    input_w,
                    editor,
                    "New branch",
                    builder.edit_error.as_deref(),
                );
            }
        }
        None => {}
    }
}

/// Zeichnet das Inline-Eingabefeld im Channel Builder (Pfad oder Branch-Name):
/// Label + Editor unter dem letzten Eintrag der jeweiligen Spalte, aligned mit
/// den Spalten. Eine optionale Fehlermeldung erscheint rot direkt darunter.
fn draw_builder_input_inline(
    f: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    editor: &crate::editor::Editor,
    label: &str,
    error: Option<&str>,
) {
    use crate::editor::InputLayout;

    let prompt = "> ";
    let indent = prompt.len();

    // Label
    let label = Line::from(Span::styled(
        format!(" {label}:"),
        Style::default()
            .fg(theme().accent)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(
        Paragraph::new(label).style(Style::default().bg(theme().status_bg)),
        Rect::new(x, y, width, 1),
    );

    // Editor-Zeile
    let content_width = width as usize;
    let layout = InputLayout {
        width: content_width + indent,
        indent,
        content: content_width,
        ranges: std::iter::once(0..editor.text().len()).collect(),
        total: editor.text().len(),
    };

    let sel_range = editor.selected_range();
    let (cur_row, cur_off) = editor.cursor_row_col(&layout);
    let text_str: String = editor.text().iter().collect();

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        prompt,
        Style::default()
            .fg(Color::Rgb(245, 158, 11))
            .add_modifier(Modifier::BOLD),
    ));

    // Cursor-Block mit expliziten Farben statt `REVERSED` ohne Farben: Auf
    // einem Span mit Reset-Farben (kein gesetztes fg/bg) bewirkt REVERSED
    // keine sichtbare Änderung – die Zelle sähe dann aus wie ein normaler
    // Textzeichenträger und der Cursor wäre praktisch unsichtbar. Ein weißer
    // Block mit dunkler Schrift ist dagegen immer klar erkennbar.
    let cursor_style = Style::default().fg(theme().status_bg).bg(theme().band_fg);

    let chars: Vec<char> = text_str.chars().collect();
    let display_end = chars.len().min(content_width);
    for (i, ch) in chars[..display_end].iter().enumerate() {
        let in_selection = sel_range.as_ref().is_some_and(|r| r.contains(&i));
        let is_cursor = cur_row == 0 && cur_off == i;
        let style = if in_selection {
            Style::default().fg(theme().band_fg).bg(theme().band_bg)
        } else if is_cursor {
            cursor_style
        } else {
            Style::default().fg(theme().band_fg)
        };
        spans.push(Span::styled(ch.to_string(), style));
    }

    // Cursor am Ende des Textes (oder im leeren Feld): ein sichtbarer Block
    // hinter dem letzten Zeichen bzw. an Position 0.
    if cur_row == 0 && cur_off >= display_end {
        spans.push(Span::styled(" ", cursor_style));
    }

    let para = Paragraph::new(Line::from(spans)).style(Style::default().bg(theme().status_bg));
    f.render_widget(para, Rect::new(x, y + 1, width, 1));

    // Optionale Fehlermeldung (z. B. „Could not create branch“) rot direkt
    // unter dem Editor – das Eingabefeld bleibt dann offen zur Korrektur.
    if let Some(err) = error {
        let err_line = Line::from(Span::styled(
            format!(" {err}"),
            Style::default().fg(theme().err),
        ));
        f.render_widget(
            Paragraph::new(err_line).style(Style::default().bg(theme().status_bg)),
            Rect::new(x, y + 2, width, 1),
        );
    }

    // Während das Feld aktiv ist, gehört der Terminal-Cursor hierher –
    // sonst würde er (wie bei allen anderen Dialogen ohne Textinput) als
    // weiterhin leerer Platz in der Chat-Eingabezeile stehen bleiben.
    let cell_col = indent + cur_off;
    let cx = x + cell_col.min(width.saturating_sub(1) as usize) as u16;
    let cy = y + 1;
    f.set_cursor_position((cx, cy));
}

/// Eine Zeile der Kanal-Auswahl: Status-Indikator (⬢) in der Farbe des
/// Kanal-Zustands + Name. Die erste Zeile („kein Kanal“, `None`) zeigt einen
/// grauen, leeren Indikator. Reine Funktion (ohne Registry-Zugriff), damit
/// sie ohne Terminal getestet werden kann.
pub(crate) fn channel_picker_row(
    name: &str,
    status: Option<ChannelStatus>,
    cursor: bool,
    usage: &str,
) -> Line<'static> {
    let indicator = Span::styled(
        if status.is_some() { "⬢" } else { " " },
        Style::default().fg(status.map(channel_status_color).unwrap_or(theme().muted)),
    );
    let base_fg = if name == "(kein Kanal)" {
        theme().muted
    } else {
        theme().highlight
    };
    let extra = if usage.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(
            format!("  {usage}"),
            Style::default().fg(theme().muted),
        )]
    };
    plain_row(indicator, name, base_fg, cursor, extra)
}

/// Sonderzeile „new channel" im Channel-Picker: „+"-Indikator in Akzentfarbe,
/// um den Unterschied zu bestehenden Kanälen deutlich zu machen.
pub(crate) fn channel_picker_new_channel_row(cursor: bool) -> Line<'static> {
    plain_row(
        Span::styled("+", Style::default().fg(theme().accent)),
        "new channel",
        theme().highlight,
        cursor,
        Vec::new(),
    )
}

/// Kurzer Hinweis, welche Sessions einen Kanal gerade nutzen – für die
/// Anzeige im Channel-Picker (z. B. „ · Session 1, 3 (aktiv)").
fn channel_usage(sessions: &[Session], channels: &ChannelRegistry, name: &str) -> String {
    let mut nums: Vec<usize> = Vec::new();
    let mut active = false;
    for (i, s) in sessions.iter().enumerate() {
        let matches = s
            .channel
            .as_ref()
            .and_then(|ch| channels.find_name(ch))
            .as_deref()
            == Some(name);
        if matches {
            nums.push(i + 1);
            if s.phase == Phase::WaitingForLLM
                || s.phase == Phase::WaitingForTool
                || !s.open_tool_ids.is_empty()
            {
                active = true;
            }
        }
    }
    if nums.is_empty() {
        return String::new();
    }
    let list = nums
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if active {
        format!("· Session {list} (aktiv)")
    } else {
        format!("· Session {list}")
    }
}

/// Kürzt eine Zeile für die Dialog-Anzeige (Kommando usw.) auf `n` Zeichen.
fn clip(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Einheitliche Auswahl-Zeile für Listen- und Options-Einträge: Indikator
/// voran, dann ` {text}` – mit `cursor` weiß+fett, ansonsten in `base_fg` –
/// und optional nachgestellte Zusatz-Spans (Statushinweis, Demand …). Die
/// konkreten Dekorateure (`option_row`, `channel_picker_row`, …) berechnen
/// nur noch ihren Indikator, die Textfarbe und die Extra-Spans.
fn plain_row(
    indicator: Span<'static>,
    text: &str,
    base_fg: Color,
    cursor: bool,
    extra: Vec<Span<'static>>,
) -> Line<'static> {
    let mut spans = vec![indicator];
    let style = Style::default()
        .fg(if cursor { theme().band_fg } else { base_fg })
        .add_modifier(if cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    spans.push(Span::styled(format!(" {text}"), style));
    spans.extend(extra);
    Line::from(spans)
}

/// Auswahlzeile eines Bestätigungsdialogs: markiert die aktive Option.
fn option_row(text: &str, cursor: bool) -> Line<'static> {
    plain_row(
        Span::styled(if cursor { "▶" } else { " " }, Style::default()),
        text,
        theme().highlight,
        cursor,
        Vec::new(),
    )
}

/// Schätzt, wie viele sichtbare Zeilen eine `Line` bei der gegebenen Breite
/// einnimmt, wenn ratatui sie umbricht (`Wrap { trim: true }`). Die Schätzung
/// folgt der gleichen gierigen Wort-Trennung wie ratatuis `WordWrapper` (ein
/// Wort, das breiter als die Zeile ist, wird in mehrere Zeilen zerlegt) und
/// unterschätzt die tatsächliche Zeilenzahl nie – der Dialog wird also eher
/// etwas höher als zu klein (und damit abschneidend).
fn wrapped_row_count(line: &Line<'_>, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let width = width as usize;
    let mut text = String::new();
    for s in &line.spans {
        text.push_str(&s.content);
    }
    if text.is_empty() {
        return 1;
    }
    let mut rows = 1u16;
    let mut col = 0usize; // bereits belegte Spalten in der aktuellen Zeile
    for raw in text.split(' ') {
        let w = raw.chars().count();
        if w == 0 {
            continue; // Leerstellen-Reste ignorieren (trim verwirft sie ohnehin)
        }
        if col == 0 {
            // Wort beginnt am Zeilenanfang; ist es breiter als die Zeile, wird
            // es in mehrere Zeilen zerlegt.
            let chunks = w.div_ceil(width);
            rows += (chunks - 1) as u16;
            col = w % width;
            if col == 0 {
                col = width; // exakt passend: Zeile ist voll
            }
        } else if col + 1 + w <= width {
            col += 1 + w; // Leerzeichen + Wort passen noch in die Zeile
        } else {
            // Umbruch nötig: Wort (evtl. zerlegt) kommt in die nächste Zeile.
            rows += 1;
            let chunks = w.div_ceil(width);
            rows += (chunks - 1) as u16;
            col = w % width;
            if col == 0 {
                col = width;
            }
        }
    }
    rows
}

/// Gemeinsames Gerüst der zentrierten Bestätigungsdialoge (flächig, ohne
/// schwere Rahmen – wie die Kanal-Auswahl): Titel, Inhalt und Fußzeile.
///
/// Der Dialog ist bewusst großzügig breit und seine Höhe richtet sich nach der
/// tatsächlich benötigten Zeilenzahl (inkl. Umbrüche), damit lange
/// Überschriften/Texte – etwa die Container-Zeilen im Beenden-Dialog – nicht
/// rechts abgeschnitten oder unten weggeclippt werden.
fn confirmation_dialog(
    f: &mut Frame,
    title: &str,
    title_fg: Color,
    lines: Vec<Line<'static>>,
    footer: &[(&str, &str)],
) {
    let area = f.area();
    // Breit genug: möglichst viel der verfügbaren Breite nutzen, aber mit
    // vernünftigem Maximum und nie breiter als das Terminal selbst.
    let width = (area.width * 9 / 10)
        .clamp(56, 120)
        .min(area.width.saturating_sub(4));
    let pad = PICKER_PAD as u16;
    let inner_width = width.saturating_sub(pad * 2);

    // Gesamten Inhalt vorab zusammensetzen, damit wir die benötigte Höhe
    // (inkl. Umbrüchen) ermitteln können.
    let mut all = Vec::with_capacity(lines.len() + 2);
    all.push(Line::from(Span::styled(
        format!(" {title} "),
        Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
    )));
    all.extend(lines);
    all.push(key_help(footer, inner_width as usize));

    // Höhe an die (ggf. umbrochenen) Zeilen anpassen, statt pauschal eine
    // Zeile pro Eintrag anzunehmen – sonst werden lange Zeilen unten
    // abgeschnitten.
    let needed: u16 = all
        .iter()
        .map(|line| wrapped_row_count(line, inner_width))
        .sum();
    // +2: kleiner Puffer falls wrapped_row_count leicht unterschätzt.
    let height = (needed + 2).clamp(5, area.height.saturating_sub(2).max(5));

    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let bg = Paragraph::new("").style(Style::default().bg(theme().status_bg));
    f.render_widget(bg, rect);

    let inner = Rect::new(
        rect.x + pad,
        rect.y + 1,
        rect.width.saturating_sub(pad * 2),
        rect.height.saturating_sub(2),
    );
    let para = Paragraph::new(all)
        .style(Style::default().bg(theme().status_bg))
        // Text umbrechen, statt ihn am rechten Rand abzuschneiden – lange
        // Überschriften/Texte (z. B. Container-Zeilen) laufen in Folgezeilen.
        .wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

/// Bestätigungsdialoge: VOR dem Absenden mit `execute` auf einem Local-Kanal
/// (drei Optionen – verantworten, einzeln bestätigen, abbrechen), die
/// Einzel-Bestätigung eines `run`-Aufrufs im ConfirmEach-Modus sowie das
/// Beenden bei wesentlichen Änderungen in selbst gestarteten Containern.
pub(crate) fn draw_confirmation(f: &mut Frame, app: &App) {
    if let Some(d) = &app.channel_close {
        match &d.phase {
            crate::app::ChannelClosePhase::ActiveConfirm { nav } => {
                let options = ["Close anyway (active session running)", "Cancel"];
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(
                            " Channel \"{}\" is still in use by an active session.",
                            d.name
                        ),
                        Style::default().fg(theme().err),
                    )),
                    Line::from(Span::styled(
                        " Closing it may interrupt that work.",
                        Style::default().fg(theme().err),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, nav.cursor() == i));
                }
                confirmation_dialog(
                    f,
                    "Close channel – active session?",
                    theme().err,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
            crate::app::ChannelClosePhase::Worktree {
                summary,
                nav,
                options,
            } => {
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(
                            " Worktree of channel \"{}\" has uncommitted changes.",
                            d.name
                        ),
                        Style::default().fg(theme().err),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for note in summary.lines() {
                    lines.push(Line::from(Span::styled(
                        format!(" {note}"),
                        Style::default()
                            .fg(theme().band_fg)
                            .add_modifier(Modifier::BOLD),
                    )));
                }
                lines.push(Line::from(Span::raw(" ")));
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, nav.cursor() == i));
                }
                confirmation_dialog(
                    f,
                    "Worktree has uncommitted changes",
                    theme().err,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
            crate::app::ChannelClosePhase::Container { notes, nav } => {
                let options = ["Yes, close channel & stop container", "No, cancel"];
                let mut lines = vec![
                    Line::from(Span::styled(
                        format!(" Container of channel \"{}\" has unsaved changes.", d.name),
                        Style::default().fg(theme().err),
                    )),
                    Line::from(Span::styled(
                        " Closing would stop it and this state would be lost.",
                        Style::default().fg(theme().err),
                    )),
                    Line::from(Span::raw(" ")),
                ];
                for note in notes {
                    lines.push(Line::from(Span::styled(
                        format!("   • {note}"),
                        Style::default().fg(theme().band_fg),
                    )));
                }
                lines.push(Line::from(Span::raw(" ")));
                for (i, text) in options.iter().enumerate() {
                    lines.push(option_row(text, nav.cursor() == i));
                }
                confirmation_dialog(
                    f,
                    "Container has unsaved changes",
                    theme().err,
                    lines,
                    &NAV_CONFIRM_KEYS,
                );
                return;
            }
        }
    }
    if let Some(d) = &app.stop_confirm {
        let options = ["Yes, quit & stop containers", "No, cancel"];
        let mut lines = vec![
            Line::from(Span::styled(
                " Self-started containers contain unsaved changes.",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::styled(
                " Quitting would stop them and this state would be lost.",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::raw(" ")),
        ];
        // Je betroffenem Container eine Überschrift (mit Name) und darunter
        // die einzelnen Befunde als Aufzählung – übersichtlicher als eine
        // einzelne, gekürzte Zeile pro Kanal.
        for entry in &d.entries {
            let header = match &entry.container {
                Some(c) => format!(" Container \"{}\" (channel \"{}\")", c, entry.channel),
                None => format!(" Channel \"{}\"", entry.channel),
            };
            lines.push(Line::from(Span::styled(
                header,
                Style::default()
                    .fg(theme().band_fg)
                    .add_modifier(Modifier::BOLD),
            )));
            for note in &entry.notes {
                lines.push(Line::from(Span::styled(
                    format!("   • {note}"),
                    Style::default().fg(theme().band_fg),
                )));
            }
            lines.push(Line::from(Span::raw(" ")));
        }
        if d.more > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "   … and {more} more {noun} with changes",
                    more = d.more,
                    noun = if d.more == 1 {
                        "container"
                    } else {
                        "containers"
                    }
                ),
                Style::default().fg(theme().muted),
            )));
            lines.push(Line::from(Span::raw(" ")));
        }
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.nav.cursor() == i));
        }
        confirmation_dialog(f, "Really quit?", theme().err, lines, &NAV_CONFIRM_KEYS);
        return;
    }
    if let Some(d) = &app.branch_confirm {
        let mut lines = vec![
            Line::from(Span::styled(
                format!(" {}", d.summary),
                Style::default().fg(theme().err),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in d.options.iter().enumerate() {
            lines.push(option_row(text, d.nav.cursor() == i));
        }
        confirmation_dialog(
            f,
            "/branch – branch already exists",
            theme().err,
            lines,
            &NAV_CONFIRM_KEYS,
        );
        return;
    }
    if let Some(d) = &app.path_confirm {
        let lines = vec![
            Line::from(Span::styled(
                " This path does not exist yet.",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::styled(
                format!("   {}", clip(&d.path.to_string_lossy(), 60)),
                Style::default()
                    .fg(theme().band_fg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                " Create it with 'mkdir -p' (Enter) or go back to edit (Esc)?",
                Style::default().fg(theme().muted),
            )),
        ];
        confirmation_dialog(
            f,
            "Create directory?",
            theme().err,
            lines,
            &[("↵", "create"), ("esc", "cancel")],
        );
        return;
    }
    if let Some(d) = &app.pre_send_confirm {
        let options = [
            "I take responsibility (e.g. sandbox)",
            "Send, but confirm each exec individually",
            "Cancel",
        ];
        let mut lines = vec![
            Line::from(Span::styled(
                " The model may execute commands directly on your local system.",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::styled(
                " This can be dangerous – only send if you accept the consequences.",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in options.iter().enumerate() {
            let line = option_row(text, d.nav.cursor() == i);
            lines.push(line);
        }
        confirmation_dialog(
            f,
            "Local execution – confirmation",
            theme().err,
            lines,
            &NAV_CONFIRM_KEYS,
        );
        return;
    }
    if let Some(d) = &app.exec_confirm {
        let options = ["Yes, run", "No, decline"];
        let mut lines = vec![
            Line::from(Span::styled(
                " Run this command on the local system?",
                Style::default().fg(theme().err),
            )),
            Line::from(Span::styled(
                format!("   {}", clip(&d.command, 60)),
                Style::default()
                    .fg(theme().band_fg)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("   {}", d.label),
                Style::default()
                    .fg(theme().muted)
                    .add_modifier(Modifier::ITALIC),
            )),
            Line::from(Span::raw(" ")),
        ];
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.nav.cursor() == i));
        }
        confirmation_dialog(f, "Run command?", theme().err, lines, &EXEC_KEYS);
    }
    if let Some(d) = &app.options_dialog {
        let version_label = format!("Version: {}", crate::config::VERSION);
        let mouse_label = if app.mouse_enabled {
            "Mouse capture: ON"
        } else {
            "Mouse capture: OFF"
        };
        let status_label = format!("Model: {}", app.display_model(app.active),);
        let options = [&version_label, mouse_label, &status_label];
        let mut lines = vec![Line::from(Span::raw(" "))];
        for (i, text) in options.iter().enumerate() {
            lines.push(option_row(text, d.nav.cursor() == i));
        }
        confirmation_dialog(f, "Options", Color::Cyan, lines, &NAV_CONFIRM_KEYS);
    }
}

/// Zeigt die HTTP-Response-Header der letzten erfolgreichen LLM-Antwort der
/// aktiven Session (`Alt+H`). Reine Anzeige – Esc/Enter schließen wieder.
pub(crate) fn draw_http_headers_dialog(f: &mut Frame, app: &App) {
    if !app.http_headers_dialog {
        return;
    }
    let s = &app.sessions[app.active];
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &s.last_http_headers {
        Some(headers) if !headers.is_empty() => {
            for (name, value) in headers {
                lines.push(Line::from(Span::styled(
                    format!(" {name}: {value}"),
                    Style::default().fg(theme().band_fg),
                )));
            }
        }
        _ => {
            lines.push(Line::from(Span::styled(
                " (no HTTP response captured yet for this session)",
                Style::default()
                    .fg(theme().muted)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }
    confirmation_dialog(
        f,
        "HTTP headers of last response",
        Color::Cyan,
        lines,
        &[("esc", "close")],
    );
}
