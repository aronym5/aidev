use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::channel::ChannelRegistry;
use crate::config::Config;
use crate::llm::WorkerEvent;
use crate::ui;

mod types;
pub use types::*;

mod session;
pub(crate) use session::{
    apply_channel_permission_default, apply_default_channel, prompt_chars, should_compact,
    LiveChannel,
};
pub use session::{ChatAnchor, Session, ViewLevel};

mod commands;
pub(crate) use commands::path_tail;
pub(crate) use types::step_cursor;

mod builder;
mod close;
mod dialogs;
pub(crate) mod models;
mod picker;
mod worker;

/// Geöffneter Channel-Auswahl-Dialog für die aktive Session.
pub struct ChannelPicker {
    /// Auswahlzeilen; Index 0 ist immer „(kein Kanal)“.
    pub items: Vec<String>,
    /// Tatsächliche Channel-Namen (zum Nachschlagen im Registry).
    /// Index 0 ist None (für „(kein Kanal)“).
    pub keys: Vec<Option<String>>,
    pub cursor: usize,
}

/// Geöffneter Modell-Auswahl-Dialog (`/model` ohne Argument).
/// Enthält alle konfigurierten Aliase in config.toml-Reihenfolge.
/// „(Standard)" erscheint nur als eigener Eintrag, wenn das Default-Modell
/// bei den konfigurierten Modellen fehlt.
pub struct ModelPicker {
    /// Einträge `(interner_key, display_full)` – z.B. `("openai/fast", "openai/fast (gpt-4o-mini)")`.
    pub items: Vec<(String, String)>,
    /// `true`, wenn „(Standard)" als erster Eintrag angezeigt wird.
    pub show_default: bool,
    pub cursor: usize,
    /// `true` solange eine asynchrone Modell-Listen-Aktualisierung läuft.
    pub loading: bool,
}

/// Geöffneter Channel Builder – dreispaltiger Dialog zur Kanal-Erzeugung.
pub struct ChannelBuilderState {
    /// Tunnel-Optionen: Local zuerst, dann benannte Podman-Images
    pub tunnels: Vec<crate::channel::builder::Tunnel>,
    /// Host/Repo-Pfade: Config + cwd + argv, dedupliziert
    pub host_paths: Vec<crate::channel::builder::HostPath>,
    /// Worktrees des aktuell gewählten Host-Pfads
    pub worktrees: Vec<crate::channel::builder::WorktreeEntry>,

    // --- Cursor ---
    /// Aktive Spalte (0=Tunnel, 1=Host, 2=Worktree)
    pub col: usize,
    pub tunnel_idx: usize,
    pub host_idx: usize,
    pub worktree_idx: usize,

    // --- Container-Status ---
    pub container_info: Option<crate::channel::builder::ContainerInfo>,

    // --- Flags ---
    /// Podman-Images wurden bereits geladen
    pub images_loaded: bool,
    /// Der aktuelle Host-Pfad ist ein Git-Repo
    pub current_is_git: bool,
    /// argv-Pfad (sofern von außen übergeben)
    pub argv_path: Option<String>,

    // --- Pfad-Input ---
    /// Offenes Eingabefeld für einen benutzerdefinierten Host-Pfad (`A`-Taste).
    /// `Some(editor)` = Pfad-Input ist aktiv, `None` = normaler Builder-Modus.
    pub host_path_edit: Option<crate::editor::Editor>,
}

pub struct App {
    pub sessions: Vec<Session>,
    /// Index der aktiven Konversation.
    pub active: usize,
    /// Zähler für die Lauf-Animation (Tabs & Status).
    pub spinner: usize,
    pub quit: bool,
    pub config: Config,
    /// Alle konfigurierten Kanäle.
    pub channels: ChannelRegistry,
    /// Zentrale Modell-Verwaltung (Status: config/refresh).
    pub model_registry: models::ModelRegistry,
    /// Offene Kanal-Auswahl (falls nicht None, gehen alle Tasten dorthin).
    pub channel_picker: Option<ChannelPicker>,
    /// Offener Modell-Auswahl-Dialog (`/model`).
    pub model_picker: Option<ModelPicker>,
    /// Offener Channel Builder (falls nicht None, gehen alle Tasten dorthin).
    pub channel_builder: Option<ChannelBuilderState>,
    /// Gemerkter Umgang mit `execute`-Prompts auf Local-Kanälen (Programmlauf).
    pub local_exec_mode: LocalExecMode,
    /// Offener Bestätigungsdialog VOR dem Absenden mit `execute` auf Local.
    pub pre_send_confirm: Option<PreSendConfirm>,
    /// Offene Bestätigung für einen einzelnen `run`-Aufruf (ConfirmEach-Modus).
    pub exec_confirm: Option<ExecConfirm>,
    /// Offener Bestätigungsdialog vor dem Beenden, wenn eigene Run-Container
    /// wesentliche, ungesicherte Änderungen enthalten.
    pub stop_confirm: Option<StopConfirm>,
    /// Offener Dialog zum Schließen eines Kanals (Entf im Picker ODER
    /// Ctrl+D / /end einer Session) – gekapselte Worktree/Container-Aufräumung.
    pub channel_close: Option<ChannelClose>,
    /// Offener Bestätigungsdialog für `/branch` wenn der Branch schon existiert.
    pub branch_confirm: Option<BranchConfirm>,
    /// Offener Options-Dialog (Ctrl+O).
    pub options_dialog: Option<OptionsDialog>,
    /// Offener HTTP-Header-Dialog der letzten LLM-Antwort (`Alt+H`).
    pub http_headers_dialog: bool,
    /// Wurde im vorherigen Key-Event `Esc` gedrückt? Wird genutzt, um
    /// Alt+Keypad-`+`/`-` zu erkennen: viele Terminals senden Esc gefolgt
    /// von `+`/`-` statt `Alt+Char('+')`.
    pub(crate) pending_esc: bool,
    /// Zwischengespeicherte Daten für `/branch` solange der Dialog offen ist.
    pub branch_pending: Option<BranchPending>,
    tx: mpsc::Sender<WorkerEvent>,
    rx: mpsc::Receiver<WorkerEvent>,
    /// Aktueller Zustand des Maus-Reportings (kann per Options-Dialog
    /// umgeschaltet werden). Steuert das Senden von `\x1b[?1000h`/`\x1b[?1000l`.
    pub mouse_enabled: bool,
    /// Gecachter Git-Status (Label `branch@repo`, `clean`) der aktiven Session,
    /// damit `git status` nicht bei jedem Redraw läuft. `None` = (noch) kein
    /// Git-Repo erkannt. Höchstens 1×/s frisch ermittelt (siehe
    /// `refresh_git_status`). Wird von `metadata_line` (ui.rs) gelesen.
    pub(crate) git_status_cache: Option<(String, bool)>,
    /// Zeitpunkt der letzten Git-Status-Prüfung.
    git_status_checked: Instant,
    /// Gecachte Reiter-Titel (ein Eintrag pro Session, Format siehe
    /// `session_tab_label`). Höchstens 1×/s neu berechnet bzw. sofort bei
    /// Änderung der Session-Anzahl – die Titel enthalten Git-Aufrufe und
    /// dürfen nicht pro Frame laufen. Von `draw_tabs` (ui.rs) gelesen.
    pub(crate) tab_labels: Vec<String>,
    /// Zeitpunkt der letzten Reiter-Titel-Berechnung. Crate-intern, damit
    /// Tests die 1-s-Frist gezielt ablaufen lassen können.
    pub(crate) tab_labels_at: Instant,
}

/// Überträgt den konfigurierten Default-Kanal auf eine neue Session und setzt
/// die kanalabhängige Start-Berechtigung, solange der User sie nicht selbst
/// (per Tab) umgestellt hat.
impl App {
    /// Löst das aktuell gewählte Modell der Session zu einem
    /// `ResolvedEndpoint` auf. Nutzt die Registry, um aus dem internen Key
    /// (`provider/alias`) den serverseitigen Modellnamen und die Provider-
    /// Konfiguration zu ermitteln.
    pub(crate) fn resolve_endpoint(
        &self,
        idx: usize,
    ) -> Result<crate::config::ResolvedEndpoint, String> {
        let alias = self.sessions[idx].model_alias.as_deref();

        // 1. Versuch: Registry-Lookup (internes Key → Provider + server_model).
        if let Some(key) = alias {
            if let Some(entry) = self.model_registry.get(key) {
                return self.resolve_from_entry(entry);
            }
        }

        // 2. Versuch: Default-Modell über Registry auflösen (alias = None
        //    oder Key nicht gefunden → Fallback auf config.model).
        let model_id = alias.unwrap_or(&self.config.model);
        if let Some(entry) = self.model_registry.find_by_model_id(model_id) {
            return self.resolve_from_entry(entry);
        }

        // 3. Versuch: Direkt in config.models nach Alias suchen.
        if let Some(key) = alias {
            if let Some(_mc) = self.config.models.get(key) {
                return self.config.resolve(Some(key));
            }
        }

        // 4. Versuch: config.models (rückwärtskompatibel).
        self.config.resolve(alias)
    }

    /// Baut ein `ResolvedEndpoint` aus einem Registry-Eintrag.
    fn resolve_from_entry(
        &self,
        entry: &crate::app::models::ModelEntry,
    ) -> Result<crate::config::ResolvedEndpoint, String> {
        let provider_cfg = self
            .config
            .provider
            .get(&entry.provider)
            .ok_or_else(|| format!("Unbekannter Provider '{}'", entry.provider))?;
        // Kontextfenster: Registry-Eintrag (aus Config) oder globaler Default.
        let cw = entry
            .context_window
            .filter(|w| *w > 0)
            .unwrap_or(self.config.context_window);
        Ok(crate::config::ResolvedEndpoint {
            model: entry.display_key(),
            api_model: entry.api_model().to_string(),
            base_url: provider_cfg.base_url.trim_end_matches('/').to_string(),
            api_key: provider_cfg.api_key.clone().unwrap_or_default(),
            user_agent: crate::config::effective_user_agent(provider_cfg, &entry.provider),
            context_window: cw,
        })
    }

    /// Anzuzeigende Modellbezeichnung einer Session: `"provider/alias"` aus
    /// der Registry oder die volle Modell-ID aus der Config.
    pub(crate) fn display_model(&self, idx: usize) -> String {
        match self.sessions[idx].model_alias.as_deref() {
            Some(key) => {
                if let Some(entry) = self.model_registry.get(key) {
                    return entry.display_key();
                }
                self.config
                    .models
                    .get(key)
                    .map(|m| m.id().to_string())
                    .unwrap_or_else(|| key.to_string())
            }
            None => {
                // Default-Modell: über Registry auflösen, damit der Alias
                // angezeigt wird (z.B. "zen/mimo-2.5" statt "zen/mimo-v2.5-free").
                let model_id = &self.config.model;
                if let Some(entry) = self.model_registry.find_by_model_id(model_id) {
                    return entry.display_key();
                }
                // Fallback:_alias in config.models suchen.
                for (alias, mc) in &self.config.models {
                    if mc.id() == model_id.as_str() {
                        return format!("{}/{}", model_id.split('/').next().unwrap_or(""), alias);
                    }
                }
                model_id.clone()
            }
        }
    }
}

impl App {
    pub(crate) fn new(
        config: Config,
        channels: ChannelRegistry,
        tx: mpsc::Sender<WorkerEvent>,
        rx: mpsc::Receiver<WorkerEvent>,
    ) -> Self {
        let model_registry = models::ModelRegistry::new(&config.models);
        let mut app = App {
            sessions: vec![Session::new(0)],
            active: 0,
            spinner: 0,
            quit: false,
            model_registry,
            config,
            channels,
            channel_picker: None,
            model_picker: None,
            channel_builder: None,
            local_exec_mode: LocalExecMode::default(),
            pre_send_confirm: None,
            exec_confirm: None,
            stop_confirm: None,
            channel_close: None,
            branch_confirm: None,
            options_dialog: None,
            http_headers_dialog: false,
            pending_esc: false,
            branch_pending: None,
            tx,
            rx,
            mouse_enabled: false,
            git_status_cache: None,
            // Vergangenheit → erste Prüfung erfolgt schon im ersten
            // Schleifendurchlauf (statt erst nach 1 s).
            git_status_checked: Instant::now() - Duration::from_secs(2),
            tab_labels: Vec::new(),
            tab_labels_at: Instant::now() - Duration::from_secs(2),
        };
        // Auch die erste Session übernimmt den konfigurierten Default-Kanal.
        apply_default_channel(&app.channels, &mut app.sessions[0]);
        app.mouse_enabled = app.config.mouse;
        // Reiter-Titel sofort berechnen (statt erst nach Ablauf der 1-s-Frist).
        app.refresh_tab_labels();
        // Alle Podman-Kanäle einmalig proben (Statusanzeige für die Auswahl)
        // und den Default-Kanal sofort warm starten (falls er ein
        // Run-Container ist), damit er ohne Wartezeit bereit ist.
        Self::probe_all_channels(&app.channels);
        if let Some(name) = app.channels.default_channel_name().map(str::to_string) {
            if let Some(ch) = app.channels.get(&name) {
                Self::warmup_channel(&ch);
            }
        }
        app
    }

    /// Ermittelt den Git-Status der aktiven Session (Label + sauber/schmutzig)
    /// und cached ihn – höchstens 1×/s. Liefert `true`, wenn sich der Status
    /// gegenüber dem Cache geändert hat, damit die UI nur bei Bedarf neu
    /// gezeichnet wird (Idle: gar nicht, solange nichts passiert; während des
    /// Streamings: 1×/s statt bei jedem Spinner-Tick).
    fn refresh_git_status(&mut self) -> bool {
        if self.git_status_checked.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.git_status_checked = Instant::now();
        let new_status = self.active_session_git_status();
        let changed = new_status != self.git_status_cache;
        self.git_status_cache = new_status;
        changed
    }

    /// Git-Status (`branch@repo`, `clean`) für die aktive Session – oder
    /// `None`, wenn kein gebundener Kanal mit Git-Repo vorliegt.
    fn active_session_git_status(&self) -> Option<(String, bool)> {
        let s = &self.sessions[self.active];
        let host = s.channel.as_ref().and_then(|ch| ch.host_root())?;
        crate::ui::git_status_info(&host).map(|info| (info.label, info.clean))
    }

    /// Titel eines Session-Reiters:
    /// - Kanal mit Repo-Wurzel/Worktree → `branch@reponame`
    /// - Kanal mit bloßem Host-Verzeichnis → Ordnername
    /// - ohne Kanal → Modellname (Alias oder konfigurierte ID)
    fn session_tab_label(&self, idx: usize) -> String {
        let n = idx + 1;
        match self.sessions[idx]
            .channel
            .as_ref()
            .and_then(|ch| ch.host_root())
        {
            Some(host) if crate::channel::builder::is_repo_root(&host) => {
                // Identisch zur Statusleiste: `git_status_info` liefert schon
                // `branch@repo` (Repo-Name vom Haupt-Repo, Branch vom Worktree
                // – wird nur max. 1×/s pro Session aufgerufen).
                let label = crate::ui::git_status_info(&host)
                    .map(|info| info.label)
                    .unwrap_or_else(|| "?@?".to_string());
                format!("{n}: {label}")
            }
            Some(host) => format!("{n}: {}", path_tail(&host)),
            None => format!("{n}: {}", self.display_model(idx)),
        }
    }

    /// Berechnet die Reiter-Titel aller Sessions neu – höchstens 1×/s,
    /// außer die Anzahl der Sessions hat sich geändert (dann sofort, damit
    /// neue/geschlossene Reiter nicht bis zu einer Sekunde falsch bleiben).
    pub(crate) fn refresh_tab_labels(&mut self) {
        let count_changed = self.tab_labels.len() != self.sessions.len();
        if !count_changed && self.tab_labels_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.tab_labels_at = Instant::now();
        self.tab_labels = (0..self.sessions.len())
            .map(|i| self.session_tab_label(i))
            .collect();
    }

    /// Probt alle konfigurierten Podman-Kanäle einmalig im Hintergrund, damit
    /// die Statusanzeige der Kanal-Auswahl beim Öffnen stimmt. Local-Kanäle
    /// melden ohnehin grün und werden übersprungen.
    fn probe_all_channels(channels: &ChannelRegistry) {
        for name in channels.names() {
            if let Some(ch) = channels.get(&name) {
                Self::probe_channel(&ch);
            }
        }
    }

    /// Probt den soeben gebundenen Kanal im Hintergrund, damit ein bereits
    /// laufender Container sofort grün erscheint (statt erst nach dem ersten
    /// Befehl). Nur relevant für Podman-Kanäle; `Local` meldet ohnehin grün.
    fn probe_channel(ch: &Arc<dyn crate::channel::Channel>) {
        if matches!(
            ch.kind(),
            crate::channel::ChannelKind::PodmanRun | crate::channel::ChannelKind::PodmanAttach
        ) {
            let ch = ch.clone();
            let _ = std::thread::spawn(move || ch.probe());
        }
    }

    /// Startet einen gewählten Kanal (PodmanRun) im Hintergrund, ohne die UI
    /// zu blockieren – der Container ist dann sofort einsatzbereit. Für
    /// Local/Attach gibt es nichts zu starten (kein Op).
    pub(crate) fn warmup_channel(ch: &Arc<dyn crate::channel::Channel>) {
        if matches!(ch.kind(), crate::channel::ChannelKind::PodmanRun) {
            let ch = ch.clone();
            let _ = std::thread::spawn(move || ch.warmup());
        }
    }

    pub(crate) fn session_mut(&mut self, id: usize) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub(crate) fn handle_key(&mut self, key: event::KeyEvent) {
        // Offener HTTP-Header-Dialog: reine Anzeige – nur Esc/Enter schließen
        // wieder auf, alle anderen Tasten werden verschluckt.
        if self.http_headers_dialog {
            if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                self.http_headers_dialog = false;
            }
            return;
        }
        if self.stop_confirm.is_some() {
            self.handle_stop_confirm_key(key);
            return;
        }
        if self.branch_confirm.is_some() {
            self.handle_branch_confirm_key(key);
            return;
        }
        if self.pre_send_confirm.is_some() {
            self.handle_pre_send_key(key);
            return;
        }
        if self.exec_confirm.is_some() {
            self.handle_exec_confirm_key(key);
            return;
        }
        if self.channel_close.is_some() {
            self.handle_channel_close_key(key);
            return;
        }
        if self.options_dialog.is_some() {
            self.handle_options_dialog_key(key);
            return;
        }
        if self.channel_picker.is_some() {
            self.handle_picker_key(key);
            return;
        }
        if self.channel_builder.is_some() {
            self.handle_builder_key(key);
            return;
        }
        if self.model_picker.is_some() {
            self.handle_model_picker_key(key);
            return;
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Alt+Keypad-Erkennung: Viele Terminals senden Esc gefolgt von
        // `+`/`-`/`=`/`_` statt `Alt+Char(...)`.  Wurde im vorherigen
        // Key-Event Esc gedrückt und der aktuelle Key ist ein Zoom-Key,
        // behandeln wir ihn als Zoom (mit ALT-Äquivalent).
        let alt = if self.pending_esc {
            matches!(
                key.code,
                KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') | KeyCode::Char('_')
            )
        } else {
            alt
        };
        self.pending_esc = false;

        match key.code {
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Esc => {
                self.handle_esc();
                self.pending_esc = true;
            }
            KeyCode::Char('n') if ctrl => self.new_session(),
            KeyCode::Char('o') if ctrl => self.open_options_dialog(),
            KeyCode::Char('w') if ctrl => self.close_session(),
            KeyCode::Char('c') if alt => self.open_channel_picker(),
            KeyCode::Char('m') if alt => self.open_model_picker(),
            KeyCode::Char('d') if alt => self.duplicate_active_channel(),
            KeyCode::Char('h') if alt => self.http_headers_dialog = true,
            KeyCode::Left if alt => self.switch_session(-1),
            KeyCode::Right if alt => self.switch_session(1),
            KeyCode::Char(c) if ctrl && c.is_ascii_digit() => {
                if let Some(d) = c.to_digit(10) {
                    let idx = d as usize;
                    if idx > 0 && idx <= self.sessions.len() {
                        self.active = idx - 1;
                    }
                }
            }
            KeyCode::PageUp => {
                let s = self.active_mut();
                s.chat_follow = false;
                // Seitenbewegung (halbe Viewport-Höhe) auf den Anker; das
                // Layout (Block-Tops) kennt nur `draw_chat`, daher als Delta.
                let page = (s.chat_viewport.max(1) / 2).max(1) as i64;
                *s.chat_scroll_delta.get_or_insert(0) -= page;
            }
            KeyCode::PageDown => {
                let s = self.active_mut();
                s.chat_follow = false;
                let page = (s.chat_viewport.max(1) / 2).max(1) as i64;
                *s.chat_scroll_delta.get_or_insert(0) += page;
            }
            KeyCode::Tab => {
                let s = self.active_mut();
                // Wechselt die Berechtigung (read → write → execute → read);
                // ohne Kanal gibt es keine Berechtigung, Tab bleibt wirkungslos.
                // Die Wahl bleibt als Default für die nächste Nachricht.
                if s.channel.is_some() {
                    s.permission = s.permission.next();
                    s.permission_touched = true;
                }
            }
            // „Rein-/Rauszoomen“ zwischen den Ansichtsebenen (Detail → Dialog →
            // Übersicht und zurück): Alt + Minus/- raus, Alt + Plus/„=“ rein.
            KeyCode::Char('-') | KeyCode::Char('_') if alt => {
                let s = self.active_mut();
                s.view = s.view.zoom_out();
            }
            KeyCode::Char('+') | KeyCode::Char('=') if alt => {
                let s = self.active_mut();
                s.view = s.view.zoom_in();
            }
            KeyCode::Char('c') if ctrl => {
                let active = self.active;
                if self.sessions[active].editor.is_empty() {
                    // Programmende → stoppt selbst gestartete Container. Vorher
                    // prüfen, ob deren Zustand verlorenginge (Bestätigung).
                    if !self.maybe_open_stop_confirm() {
                        self.quit = true;
                    }
                } else {
                    self.sessions[active].editor.clear();
                }
            }
            KeyCode::Char('d') if ctrl => {
                // EOF-Symbolik wie in der Shell: nur bei leerer Eingabe die
                // Session schließen – nie das ganze Programm beenden.
                if self.sessions[self.active].editor.is_empty() {
                    self.close_session();
                }
            }
            KeyCode::Char(c) => {
                let s = self.active_mut();
                s.editor.insert_char(c);
            }
            KeyCode::Backspace => {
                let s = self.active_mut();
                s.editor.backspace();
            }
            KeyCode::Delete => {
                let s = self.active_mut();
                s.editor.delete_at_cursor();
            }
            KeyCode::Left if ctrl => {
                let s = self.active_mut();
                s.editor.move_word(-1, shift);
            }
            KeyCode::Right if ctrl => {
                let s = self.active_mut();
                s.editor.move_word(1, shift);
            }
            KeyCode::Left => {
                let s = self.active_mut();
                s.editor.move_cursor(-1, shift);
            }
            KeyCode::Right => {
                let s = self.active_mut();
                s.editor.move_cursor(1, shift);
            }
            KeyCode::Up => {
                let s = self.active_mut();
                // ↑ wechselt nur in der ersten umbrochenen Zeile zur vorherigen
                // Eingabe; sonst bewegt es den Cursor innerhalb des Felds.
                if !shift && s.editor.cursor_is_first_row() {
                    s.history_prev();
                } else {
                    s.editor.move_line(-1, shift);
                }
            }
            KeyCode::Down => {
                let s = self.active_mut();
                // ↓ wechselt nur in der letzten umbrochenen Zeile zur nächsten
                // Eingabe; sonst bewegt es den Cursor innerhalb des Felds.
                if !shift && s.editor.cursor_is_last_row() {
                    s.history_next();
                } else {
                    s.editor.move_line(1, shift);
                }
            }
            KeyCode::Home => {
                let s = self.active_mut();
                s.editor.move_home(shift);
            }
            KeyCode::End => {
                let s = self.active_mut();
                s.editor.move_end(shift);
            }
            _ => {}
        }
    }

    fn active_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }

    fn new_session(&mut self) {
        let id = self.sessions.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let mut session = Session::new(id);
        // Neue Sessions erben den konfigurierten Default-Kanal; dessen
        // Zustand wurde beim Start bereits geprobt/gewärmt.
        apply_default_channel(&self.channels, &mut session);
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
    }

    pub(crate) fn close_session(&mut self) {
        let active = self.active;
        self.sessions[active].cancel.store(true, Ordering::Relaxed);
        let name = self.sessions[active]
            .channel
            .as_ref()
            .and_then(|ch| self.channels.find_name(ch));
        let Some(name) = name else {
            self.finish_close_session();
            return;
        };
        // Einheitliches Aufräumen (Worktree + Container) über dieselbe, gekapselte
        // Routine wie das Schließen eines Kanals aus dem Picker (Entf). Danach
        // wird die Session beendet; steht noch ein Dialog offen, bleibt sie
        // offen und das Aufräumen läuft in `handle_channel_close_key` weiter.
        self.begin_close_channel(&name, CloseKind::Session);
    }

    /// Tatsächliches Entfernen der Session – wird nach dem einheitlichen
    /// Kanal-Aufräumen (`close_channel_*`) aufgerufen, sobald der Kanal
    /// geschlossen ist (bzw. abgebrochen wurde).
    pub(crate) fn finish_close_session(&mut self) {
        let active = self.active;
        self.sessions.remove(active);
        if self.sessions.is_empty() {
            self.quit = true;
        } else {
            self.active = self.active.min(self.sessions.len() - 1);
        }
    }

    fn switch_session(&mut self, delta: isize) {
        let n = self.sessions.len();
        self.active = (self.active as isize + delta).rem_euclid(n as isize) as usize;
    }

    /// Läuft irgendwo eine Warte-Animation (Streaming, aktives Werkzeug, Retry,
    /// Kompaktierung)? Nur dann muss der Frame regelmäßig neu gezeichnet werden,
    /// damit der Spinner sich dreht. Im Idle ist der Bildschirm statisch.
    ///
    /// Solange ein blockierender Dialog offen ist, wird bewusst NICHT animiert:
    /// Die Spinner blieben sonst „stehen in der Luft“, während der User eine
    /// Entscheidung treffen muss – und der Frame würde unnötig alle 200 ms neu
    /// gezeichnet, obwohl der Worker (z. B. bei der `run`-Bestätigung) ohnehin
    /// blockiert.
    fn spinner_active(&self) -> bool {
        if self.any_dialog_open() {
            return false;
        }
        self.sessions.iter().any(|s| {
            s.phase == Phase::WaitingForLLM
                || s.phase == Phase::WaitingForTool
                || !s.open_tool_ids.is_empty()
                || s.retrying.is_some()
                || s.compacting
        })
    }

    /// Ist gerade irgendein modaler Dialog offen (er beansprucht die Tastatur
    /// für sich)? Dient u. a. dazu, währenddessen keine Warte-Animation laufen
    /// zu lassen (siehe [`App::spinner_active`]).
    pub(crate) fn any_dialog_open(&self) -> bool {
        self.channel_picker.is_some()
            || self.model_picker.is_some()
            || self.channel_builder.is_some()
            || self.pre_send_confirm.is_some()
            || self.exec_confirm.is_some()
            || self.stop_confirm.is_some()
            || self.channel_close.is_some()
            || self.branch_confirm.is_some()
            || self.options_dialog.is_some()
    }

    /// Wartet gerade ein blockierender Bestätigungsdialog auf eine Entscheidung
    /// des Users? Das sind die Dialoge, die eine Aktion pausieren, bis der User
    /// sie freigibt/ablehnt: die Einzel-Bestätigung eines `run`-Befehls
    /// (ConfirmEach), das Absenden mit `execute` auf einem lokalen Kanal sowie
    /// Beenden/Schließen/Branch-Bestätigungen.
    pub(crate) fn awaiting_decision(&self) -> bool {
        self.exec_confirm.is_some()
            || self.pre_send_confirm.is_some()
            || self.stop_confirm.is_some()
            || self.channel_close.is_some()
            || self.branch_confirm.is_some()
    }

    /// Betrifft die ausstehende User-Entscheidung die Session `idx`? Die
    /// Entscheidungs-Dialoge sind modal – während sie offen sind, kann nicht
    /// zwischen Sessions gewechselt werden –, daher ist immer die aktive
    /// Session gemeint.
    pub(crate) fn session_awaits_decision(&self, idx: usize) -> bool {
        self.awaiting_decision() && idx == self.active
    }

    /// Schaltet Maus-Reporting (Scroll-Rad) per Escape-Sequenz ein/aus.
    /// Beim Aktivieren wird `\x1b[?1000h\x1b[?1006h` gesendet, beim
    /// Deaktivieren `\x1b[?1000l\x1b[?1006l`.  Textmarkierung und
    /// Einfügen mit der mittleren Maustaste funktionieren nur im
    /// deaktivierten Zustand.
    pub(crate) fn toggle_mouse(&mut self) {
        self.mouse_enabled = !self.mouse_enabled;
        let seq = if self.mouse_enabled {
            b"\x1b[?1000h\x1b[?1006h"
        } else {
            b"\x1b[?1000l\x1b[?1006l"
        };
        let _ = io::stdout().write_all(seq);
        let _ = io::stdout().flush();
    }

    /// Öffnet den Options-Dialog (Ctrl+O).
    pub(crate) fn open_options_dialog(&mut self) {
        // Schließt andere modale Dialoge, die noch offen sein könnten.
        self.channel_picker = None;
        self.model_picker = None;
        self.channel_builder = None;
        self.options_dialog = Some(OptionsDialog { cursor: 0 });
    }

    /// Tastatur-Input für den Options-Dialog.
    pub(crate) fn handle_options_dialog_key(&mut self, key: event::KeyEvent) {
        let Some(d) = self.options_dialog.take() else {
            return;
        };
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            KeyCode::Esc => {
                // Dialog schließen.
            }
            KeyCode::Char('o') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                // Ctrl+O schließt den Dialog wieder.
            }
            _ if down => {
                self.options_dialog = Some(OptionsDialog {
                    cursor: (d.cursor + 1).min(1),
                });
            }
            _ if up => {
                self.options_dialog = Some(OptionsDialog {
                    cursor: d.cursor.saturating_sub(1),
                });
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if d.cursor == 0 {
                    self.toggle_mouse();
                }
                // Dialog bleibt offen, damit der User den Status sieht.
                self.options_dialog = Some(d);
            }
            _ => {
                self.options_dialog = Some(d);
            }
        }
    }
}

pub fn run(config: Config) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let channels = ChannelRegistry::new(&config);
    let mut app = App::new(config, channels, tx, rx);

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = (|| {
        // Erster Frame sofort zeichnen, damit die UI ohne Verzögerung steht.
        terminal.draw(|frame| ui::draw(frame, &mut app))?;
        let mut last_draw = Instant::now();
        loop {
            // Neu zeichnen nur bei tatsächlicher Änderung: User-Input,
            // Fenstergrößen-Änderung, eingetroffene Worker-Daten oder laufende
            // Spinner-Animation. Im Idle (kein Spinner, keine Daten) wird kein
            // Frame mehr erzeugt – `event::poll` blockiert einfach.
            let mut redraw = false;
            if event::poll(Duration::from_millis(200))? {
                match event::read()? {
                    CrosstermEvent::Key(key) => {
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                            app.handle_key(key);
                            if app.quit {
                                break;
                            }
                            redraw = true;
                        }
                    }
                    // Terminalgröße geändert → Layout neu aufbauen.
                    CrosstermEvent::Resize(..) => redraw = true,
                    // Mausrad: ScrollUp/ScrollDown → Chat-Bereich scrollen.
                    // (Button-Klicks werden vom minimalen Mouse-Reporting ebenfalls
                    // gemeldet, aber hier ignoriert – Textmarkierung funktioniert
                    // weiterhin, da Motion-Events nicht abgefangen werden.)
                    CrosstermEvent::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            let s = app.active_mut();
                            s.chat_follow = false;
                            let step = 3_i64;
                            *s.chat_scroll_delta.get_or_insert(0) -= step;
                            redraw = true;
                        }
                        MouseEventKind::ScrollDown => {
                            let s = app.active_mut();
                            s.chat_follow = false;
                            let step = 3_i64;
                            *s.chat_scroll_delta.get_or_insert(0) += step;
                            redraw = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            redraw |= app.drain_events();
            // Git-Status höchstens 1×/s frisch ermitteln (nicht bei jedem
            // Redraw). Nur wenn er sich geändert hat, wird neu gezeichnet –
            // im Idle also gar nicht, während des Streamings 1×/s statt bei
            // jedem Spinner-Tick.
            if app.refresh_git_status() {
                redraw = true;
            }
            // Spinner animiert nur, wenn wirklich etwas läuft – dann in festem
            // Takt (≈200 ms). Sonst bleibt der statische Bildschirm unberührt.
            if app.spinner_active() && last_draw.elapsed() >= Duration::from_millis(200) {
                redraw = true;
            }
            if redraw {
                terminal.draw(|frame| ui::draw(frame, &mut app))?;
                app.spinner += 1;
                last_draw = Instant::now();
            }
        }
        Ok(())
    })();

    // Vom Kanal gesteuerte Container (Run-Modus) sauber stoppen.
    app.channels.stop_managed();
    result
}

#[cfg(test)]
mod tests;
