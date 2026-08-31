use super::*;
use crate::channel::{Channel, ChannelKind};
use crate::llm;
use crate::perm::Permission;
use crossterm::event::{self, KeyCode};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

impl App {
    fn user_run(&mut self, expr: String) {
        let active = self.active;
        let channel = self.sessions[active].channel.clone();
        let Some(ch) = channel else {
            self.sessions[active].error = Some("/run needs a bound channel (Alt+O).".into());
            self.sessions[active].error_debug = None;
            return;
        };
        if expr.is_empty() {
            self.sessions[active].error =
                Some("Usage: /run <shell-command> (e.g. /run cargo build && git status)".into());
            self.sessions[active].error_debug = None;
            return;
        }
        {
            let s = &mut self.sessions[active];
            // `/run …`-Eingabe auch für den ↑/↓-Durchlauf merken.
            s.push_history(&format!("/run {expr}"));
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
            s.chat_follow = true;
            s.chat_anchor = ChatAnchor::default();
            s.chat_scroll_delta = None;
            s.cancel = Arc::new(AtomicBool::new(false));
            s.phase = Phase::WaitingForTool;
            // Anzeige-Label für die Statuszeile; das parent-lose Tool-Event
            // selbst öffnet der Worker-ToolStart (siehe `drain_events`).
            s.active_tool_label = Some(format!("run {expr}"));
        }
        let id = self.sessions[active].id;
        let cancel = self.sessions[active].cancel.clone();
        llm::spawn_user_run(self.tx.clone(), id, ch, expr, cancel);
    }

    fn user_compact(&mut self) {
        let active = self.active;
        // Effektive Config der Session (inkl. `/model`-Auswahl) VOR dem
        // mutablen Zugriff klonen.
        let cfg = self.config.clone();
        let ep = match self.resolve_endpoint(active) {
            Ok(ep) => ep,
            Err(e) => {
                let s = &mut self.sessions[active];
                s.error = Some(e);
                return;
            }
        };
        let s = &mut self.sessions[active];
        s.editor.clear();
        s.error = None;
        s.error_debug = None;
        s.retrying = None;
        s.compacting = false;
        s.cancel = Arc::new(AtomicBool::new(false));
        let id = s.id;
        let cancel = s.cancel.clone();
        let messages = crate::chat::api_messages(&s.chat);
        llm::spawn_compact(self.tx.clone(), id, cfg, ep, messages, cancel);
    }

    fn user_model(&mut self, arg: &str) {
        let active = self.active;
        {
            let s = &mut self.sessions[active];
            if !arg.is_empty() {
                s.push_history(&format!("/model {arg}"));
            }
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
        }
        if arg.is_empty() {
            self.open_model_picker();
            return;
        }
        match self.config.models.get(arg) {
            Some(entry) => {
                let id = entry.id().to_string();
                // Internen Key bilden: „provider/alias".
                let key = self
                    .model_registry
                    .get_by_alias(arg)
                    .map(|e| e.key())
                    .unwrap_or_else(|| {
                        // Fallback: ID aufteilen.
                        match id.split_once('/') {
                            Some((p, _)) => format!("{}/{}", p, arg),
                            None => arg.to_string(),
                        }
                    });
                let display = self
                    .model_registry
                    .get(&key)
                    .map(|e| e.display_full())
                    .unwrap_or_else(|| format!("{arg} ({id})"));
                let s = &mut self.sessions[active];
                s.model_alias = Some(key);
                s.error = Some(format!("Model for this session: {display}"));
            }
            None => {
                // Vielleicht ein interner Key (provider/alias)?
                if let Some(entry) = self.model_registry.get(arg) {
                    let s = &mut self.sessions[active];
                    s.model_alias = Some(arg.to_string());
                    s.error = Some(format!("Model for this session: {}", entry.display_full()));
                    return;
                }
                let names = self.config.model_names();
                let verfuegbar = if names.is_empty() {
                    "keine konfiguriert ([models.<alias>] in config.toml)".to_string()
                } else {
                    names.join(", ")
                };
                let s = &mut self.sessions[active];
                s.error = Some(format!("Unknown model \"{arg}\". Available: {verfuegbar}"));
            }
        }
    }

    pub(crate) fn open_model_picker(&mut self) {
        // Sicherstellen, dass alle config-Modelle in der Registry sind
        // (Modelle könnten nachträglich in config.models eingetragen worden sein).
        self.sync_config_to_registry();

        // Items aus der zentralen Registry aufbauen.
        let mut items: Vec<(String, String)> = Vec::new();
        for entry in self.model_registry.all() {
            items.push((entry.key(), entry.display_full()));
        }

        if items.is_empty() {
            let s = self.active_mut();
            s.error = Some(
                "No models configured – add a `[models.<alias>]` block in config.toml.".into(),
            );
            s.error_debug = None;
            return;
        }

        // Prüfen, ob das Default-Modell bei den Keys vorkommt.
        let default_id = self.config.model.clone();
        let default_key = self
            .model_registry
            .find_by_model_id(&default_id)
            .map(|e| e.key())
            .unwrap_or_else(|| default_id.clone());
        let default_matches = items.iter().any(|(k, _)| *k == default_key);
        let show_default = !default_matches;

        // Cursor positionieren: auf das aktuell gewählte Modell.
        let current_key = self.sessions[self.active].model_alias.as_deref();
        let cursor = if let Some(key) = current_key {
            items
                .iter()
                .position(|(k, _)| k == key)
                .map(|i| if show_default { i + 1 } else { i })
                .unwrap_or(0)
        } else if default_matches {
            items
                .iter()
                .position(|(k, _)| *k == default_key)
                .unwrap_or(0)
        } else {
            0
        };

        self.model_picker = Some(ModelPicker {
            items,
            show_default,
            cursor,
            loading: false,
        });
    }

    /// Stellt sicher, dass alle Modelle aus `config.models` in der Registry
    /// eingetragen sind. Aktualisiert fehlende Einträge, behält bestehende
    /// (inkl. Refresh-Status).
    fn sync_config_to_registry(&mut self) {
        self.model_registry.sync_from_config(&self.config.models);
    }

    pub(crate) fn handle_model_picker_key(&mut self, key: event::KeyEvent) {
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            KeyCode::Esc => self.model_picker = None,
            KeyCode::Enter | KeyCode::Char(' ') => self.model_picker_select(),
            KeyCode::Char('r') => self.refresh_models_from_providers(),
            _ if down => {
                if let Some(p) = &mut self.model_picker {
                    let max = if p.show_default {
                        p.items.len()
                    } else {
                        p.items.len() - 1
                    };
                    p.cursor = step_cursor(true, p.cursor, max);
                }
            }
            _ if up => {
                if let Some(p) = &mut self.model_picker {
                    let max = if p.show_default {
                        p.items.len()
                    } else {
                        p.items.len() - 1
                    };
                    p.cursor = step_cursor(false, p.cursor, max);
                }
            }
            _ => {}
        }
    }

    /// Ruft per HTTP `GET {base_url}/models` die verfügbaren Modelle aller
    /// konfigurierten Provider im Hintergrund ab. Das Ergebnis wird als
    /// `WorkerEvent::ModelsRefreshed` zurückgeliefert und im Model-Picker
    /// angezeigt (plus in `config.models` eingetragen, damit sie wählbar sind).
    fn refresh_models_from_providers(&mut self) {
        // Loading-Flag setzen (verhindert doppelte Aufrufe).
        if let Some(p) = &mut self.model_picker {
            if p.loading {
                return;
            }
            p.loading = true;
        }

        // Provider-Infos klonen, damit der Thread keinen Borrow braucht.
        let providers: Vec<(String, String, Option<String>)> = self
            .config
            .provider
            .iter()
            .map(|(name, pc)| {
                (
                    name.clone(),
                    pc.base_url.trim_end_matches('/').to_string(),
                    pc.api_key.clone(),
                )
            })
            .collect();

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let client = crate::llm::shared_client();
            let mut all_models: Vec<(String, Option<u64>)> = Vec::new();

            for (provider_name, base_url, api_key) in &providers {
                let url = format!("{}/models", base_url);
                let mut req = client.get(&url);
                if let Some(key) = api_key {
                    if !key.is_empty() {
                        req = req.bearer_auth(key);
                    }
                }
                let resp = match req.send() {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let body = match resp.text() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                // JSON parsen: { "data": [ {"id": "gpt-4o", ...}, ... ] }
                let parsed: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
                    for entry in data {
                        if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
                            // opencode.ai mit öffentlichem Key: nur kostenlose
                            // Modelle (+ "big-pickle") anzeigen.
                            if base_url.contains("opencode.ai")
                                && api_key.as_deref() == Some("public")
                                && !id.ends_with("-free")
                                && id != "big-pickle"
                            {
                                continue;
                            }
                            let demand = entry.get("demand").and_then(|v| v.as_u64());
                            all_models.push((format!("{}/{}", provider_name, id), demand));
                        }
                    }
                }
            }

            let _ = tx.send(WorkerEvent::ModelsRefreshed(all_models));
        });
    }

    fn model_picker_select(&mut self) {
        let picker = self.model_picker.take();
        let Some(picker) = picker else {
            return;
        };
        let default_id = self.config.model.clone();
        // Cursor 0 ist „(Standard)" nur, wenn show_default=true.
        let selected = if picker.show_default && picker.cursor == 0 {
            None
        } else {
            let idx = if picker.show_default {
                picker.cursor - 1
            } else {
                picker.cursor
            };
            picker.items.get(idx).cloned()
        };
        let s = self.active_mut();
        match selected {
            None => {
                s.model_alias = None;
                s.error = Some(format!("Model for this session: default ({default_id})"));
            }
            Some((key, display)) => {
                s.model_alias = Some(key.clone());
                s.error = Some(format!("Model for this session: {display}"));
            }
        }
    }

    pub(crate) fn user_commit(&mut self, msg: String) {
        let active = self.active;
        let ch = match self.sessions[active].channel.as_ref() {
            Some(ch) => ch.clone(),
            None => {
                self.sessions[active].error = Some("/commit needs a bound channel (Alt+O).".into());
                return;
            }
        };
        let host = match ch
            .host_root()
            .filter(|h| crate::channel::builder::is_repo_root(h))
        {
            Some(h) => h,
            None => {
                self.sessions[active].error =
                    Some("/commit is only possible with a channel bound to a repo.".into());
                return;
            }
        };
        // Editor leeren & in History merken
        {
            let s = &mut self.sessions[active];
            s.push_history(&format!("/commit {msg}"));
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
        }
        // Commit ausführen
        match crate::repo::git_commit_all(&host, &msg) {
            Ok(true) => {
                self.sessions[active].error = Some(format!("Commit created: {msg}"));
            }
            Ok(false) => {
                self.sessions[active].error = Some("Nothing to commit.".into());
            }
            Err(e) => {
                self.sessions[active].error = Some(format!("Commit failed: {e}"));
            }
        }
    }

    pub(crate) fn user_branch(&mut self, branch_name: String) {
        let active = self.active;

        // Alles was wir aus der aktuellen Session brauchen, hier klonen –
        // danach ist der Borrow auf sessions[active] frei.
        let (channel_kind, channel_root, history, podman_spec) = {
            let s = &self.sessions[active];
            let ch = match s.channel.as_ref() {
                Some(ch) => ch,
                None => {
                    self.sessions[active].error =
                        Some("/branch needs a bound channel (Alt+O).".into());
                    return;
                }
            };
            match ch.kind() {
                ChannelKind::Local | ChannelKind::PodmanRun => {}
                ChannelKind::PodmanAttach => {
                    self.sessions[active].error =
                        Some("/branch does not work with shared (attach) channels.".into());
                    return;
                }
            }
            let host = ch.host_root().unwrap_or_default();
            if host.as_os_str().is_empty() {
                self.sessions[active].error = Some("Channel has no host_root.".into());
                return;
            }
            // Git-Befehle brauchen eine echte Repo-Wurzel/Worktree – ein bloßes
            // Unterverzeichnis oder repo-freier Ordner reicht nicht.
            if !crate::channel::builder::is_repo_root(&host) {
                self.sessions[active].error =
                    Some("/branch needs a channel bound to a repo (Git worktree).".into());
                return;
            }
            // Podman-Laufzeit-Parameter aus dem bestehenden Kanal übernehmen –
            // das funktioniert auch für vom ChannelBuilder erzeugte Kanäle,
            // die nicht in der statischen Konfiguration stehen.
            (ch.kind(), host, s.chat.clone(), ch.podman_run_spec())
        };

        // Repo aus dem host_root des Kanals erkennen
        let repo = match crate::repo::RepoManager::discover(&channel_root) {
            Ok(r) => r,
            Err(e) => {
                self.sessions[active].error = Some(format!("No Git repository found: {e}"));
                return;
            }
        };

        let branch_exists = crate::repo::git_branch_exists(repo.repo_root(), &branch_name);
        let existing_wt = if branch_exists {
            crate::repo::git_worktree_for_branch(repo.repo_root(), &branch_name)
        } else {
            None
        };

        match (branch_exists, existing_wt) {
            // Fall 1: Branch gibt es noch nicht → normal weiter
            (false, _) => {
                self.user_branch_proceed(
                    channel_kind,
                    channel_root,
                    history,
                    &branch_name,
                    &repo,
                    false,
                    false,
                    podman_spec,
                );
            }
            // Fall 2: Branch existiert, kein Worktree
            (true, None) => {
                let head = crate::repo::git_head_commit(&channel_root);
                let branch_commit = crate::repo::git_branch_commit(repo.repo_root(), &branch_name);
                if head == branch_commit {
                    // Selber Commit → stillschweigend normal weiter
                    self.user_branch_proceed(
                        channel_kind,
                        channel_root,
                        history,
                        &branch_name,
                        &repo,
                        false,
                        false,
                        podman_spec,
                    );
                } else {
                    // Anderer Commit → Dialog
                    let summary = format!(
                        "Branch '{branch_name}' exists and points to a different commit \
                         than the current HEAD."
                    );
                    self.branch_pending = Some(BranchPending {
                        channel_kind,
                        channel_root,
                        history,
                        branch_name,
                        has_worktree: false,
                        podman_spec,
                    });
                    self.branch_confirm = Some(BranchConfirm {
                        summary,
                        cursor: 0,
                        options: vec![
                            "Use branch, take over working directory",
                            "Use branch without stash & copy",
                            "Move branch to current HEAD, then proceed as normal",
                            "Cancel",
                        ],
                    });
                }
            }
            // Fall 3: Branch existiert, Worktree existiert
            (true, Some(ref wt_path)) => {
                let head = crate::repo::git_head_commit(&channel_root);
                let branch_commit = crate::repo::git_branch_commit(repo.repo_root(), &branch_name);
                let same_commit = head == branch_commit;
                let same_status = crate::repo::git_same_status(&channel_root, wt_path);

                if same_commit && same_status {
                    // Alles identisch → stillschweigend bestehenden Worktree nutzen
                    self.user_branch_reuse(
                        channel_kind,
                        history,
                        &branch_name,
                        wt_path,
                        podman_spec,
                    );
                } else {
                    let mut lines = Vec::new();
                    if !same_commit {
                        lines.push(format!(
                            "Branch '{branch_name}' points to a different commit."
                        ));
                    }
                    if !same_status {
                        lines.push("Worktree has divergent changes.".to_string());
                    }
                    self.branch_pending = Some(BranchPending {
                        channel_kind,
                        channel_root,
                        history,
                        branch_name,
                        has_worktree: true,
                        podman_spec,
                    });
                    self.branch_confirm = Some(BranchConfirm {
                        summary: lines.join(" "),
                        cursor: 0,
                        options: vec![
                            "Use branch & worktree as they are",
                            "Move branch to HEAD, take over working directory",
                            "Commit old worktree, then take over stash",
                            "Cancel",
                        ],
                    });
                }
            }
        }
    }

    fn resolve_podman_spec(
        &self,
        podman_spec: Option<(String, String, String)>,
    ) -> Option<(String, String, String)> {
        if let Some(spec) = podman_spec {
            return Some(spec);
        }
        self.config
            .channels
            .values()
            .find(|c| matches!(c.kind.as_str(), "podman") && c.image.is_some())
            .map(|cc| {
                (
                    cc.image.clone().unwrap_or_default(),
                    cc.workdir.clone(),
                    cc.run_home(),
                )
            })
    }

    /// `skip_stash_and_copy`: wenn `true`, werden Stash und Kopie übersprungen.
    /// `force_branch_to_head`: wenn `true`, wird der Branch vorher auf HEAD verschoben.
    // Die Parameterzahl ist hier sachlich (Kontext + zwei Flags); ein
    // Parameter-Struct würde die Aufrufstellen nur unleserlicher machen.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn user_branch_proceed(
        &mut self,
        channel_kind: ChannelKind,
        channel_root: PathBuf,
        history: crate::chat::Chat,
        branch_name: &str,
        repo: &crate::repo::RepoManager,
        skip_stash_and_copy: bool,
        force_branch_to_head: bool,
        podman_spec: Option<(String, String, String)>,
    ) {
        let active = self.active;

        // Branch auf HEAD verschieben wenn gewünscht
        if force_branch_to_head {
            if let Some(head) = crate::repo::git_head_commit(&channel_root) {
                if let Err(e) = crate::repo::git_force_branch(repo.repo_root(), branch_name, &head)
                {
                    self.sessions[active].error = Some(format!("Could not move branch: {e}"));
                    return;
                }
            }
        }

        // Worktree anlegen
        let wt = if skip_stash_and_copy {
            // Kein Stash – Worktree auf dem (evtl. verschobenen) Branch anlegen
            match repo.create_worktree(branch_name, Some(&channel_root)) {
                Ok(wt) => wt,
                Err(e) => {
                    self.sessions[active].error = Some(format!("Worktree failed: {e}"));
                    return;
                }
            }
        } else {
            // Mit Stash für uncommitted Änderungen
            match repo.create_worktree_with_stash(branch_name, Some(&channel_root)) {
                Ok(wt) => wt,
                Err(e) => {
                    self.sessions[active].error = Some(format!("Worktree failed: {e}"));
                    return;
                }
            }
        };

        // Optional: Build-Verzeichnisse kopieren
        let copy_info = if skip_stash_and_copy {
            None
        } else {
            let copy_plan = crate::repo::CopyPlan {
                entries: vec![crate::repo::CopyEntry {
                    source: "target".to_string(),
                    strategy: crate::repo::CopyStrategy::Hardlink,
                }],
            };
            match repo.execute_copy_plan(&channel_root, &wt.path, &copy_plan) {
                Ok(stats) => {
                    if stats.files_linked + stats.files_copied > 0 {
                        Some(format!(
                            "Build directory copied: {} files, {:.1}s",
                            stats.files_linked + stats.files_copied,
                            stats.duration.as_secs_f64()
                        ))
                    } else {
                        None
                    }
                }
                Err(e) => Some(format!("Build copy failed (not fatal): {e}")),
            }
        };

        // Neuen Channel erzeugen und Session abschließen (gemeinsame Helfer
        // für proceed/reuse: Worktree-Bindung, Registrierung, neue Session
        // und Eintrag in der alten Session).
        let effective_root = &wt.path;
        let base_folder = crate::repo::git_toplevel(effective_root)
            .ok()
            .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "app".to_string());
        match self.make_branch_channel(
            channel_kind,
            effective_root,
            branch_name,
            &base_folder,
            podman_spec,
            wt.clone(),
        ) {
            Ok((name, ch)) => self.branch_finish(ch, name, branch_name, history, copy_info),
            Err(e) => {
                self.sessions[active].error = Some(e);
                let _ = repo.remove_worktree(branch_name, true);
            }
        }
    }

    pub(crate) fn user_branch_reuse(
        &mut self,
        channel_kind: ChannelKind,
        history: crate::chat::Chat,
        branch_name: &str,
        wt_path: &std::path::Path,
        podman_spec: Option<(String, String, String)>,
    ) {
        let active = self.active;

        // Namensschema wie im ChannelBuilder: Kanal = "image workdir",
        // Container = "aidev-reponame-branchname-image".
        let effective_root = wt_path;
        let base_folder = crate::repo::git_toplevel(effective_root)
            .ok()
            .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "app".to_string());
        // Worktree-Info aus dem bestehenden Pfad ableiten, dann gemeinsame
        // Helfer nutzen (kein erneuter Worktree-Aufbau, keine Build-Kopie).
        let wt = crate::repo::WorktreeInfo {
            name: branch_name.to_string(),
            branch: branch_name.to_string(),
            path: wt_path.to_path_buf(),
        };
        match self.make_branch_channel(
            channel_kind,
            wt_path,
            branch_name,
            &base_folder,
            podman_spec,
            wt,
        ) {
            Ok((name, ch)) => self.branch_finish(ch, name, branch_name, history, None),
            Err(e) => {
                self.sessions[active].error = Some(e);
            }
        }
    }

    /// Erzeugt den neuen Kanal für einen `/branch`-Folge-Kanal (Local oder
    /// Podman-Run) inkl. Namensschema und Worktree-Bindung. Liefert
    /// `(Display-Name, Kanal)` oder einen Fehlertext (z. B. fehlendes Podman-Bild).
    fn make_branch_channel(
        &self,
        channel_kind: ChannelKind,
        effective_root: &Path,
        branch_name: &str,
        base_folder: &str,
        podman_spec: Option<(String, String, String)>,
        wt: crate::repo::WorktreeInfo,
    ) -> Result<(String, Arc<dyn Channel>), String> {
        let channel_display_name: String;
        let new_ch: Arc<dyn Channel> = match channel_kind {
            ChannelKind::Local => {
                channel_display_name = effective_root.display().to_string();
                let ch =
                    crate::channel::local::Local::new(wt.path.clone()).with_worktree(wt.clone());
                Arc::new(ch)
            }
            ChannelKind::PodmanRun => {
                let (image, workdir, home) = match self.resolve_podman_spec(podman_spec) {
                    Some(spec) => spec,
                    None => return Err("No Podman image available for the new channel.".into()),
                };
                let timeout = std::time::Duration::from_secs(self.config.timeout_secs.max(1));
                let (uid, gid) = crate::channel::run::host_uid_gid().unwrap_or_default();
                let name = crate::channel::builder::run_channel_name(&image, effective_root);
                let container = crate::channel::builder::run_container_name(
                    base_folder,
                    Some(branch_name),
                    &image,
                );
                channel_display_name = name.clone();
                let ch = crate::channel::podman::PodmanChannel::new_run(
                    &name,
                    &container,
                    &wt.path,
                    &image,
                    &workdir,
                    timeout,
                    uid,
                    gid,
                    home,
                    self.config.podman.usermapping,
                )
                .with_worktree(wt.clone());
                Arc::new(ch)
            }
            _ => unreachable!(),
        };
        Ok((channel_display_name, new_ch))
    }

    /// Registriert den neuen Kanal, legt die Folge-Session an und merkt den
    /// `/branch`-Befehl lediglich in der ↑/↓-Eingabe-Historie der bisherigen
    /// Session (kein UserPrompt/LLM-Anteil). Gemeinsames Ende von
    /// `user_branch_proceed` und `user_branch_reuse`.
    fn branch_finish(
        &mut self,
        new_ch: Arc<dyn Channel>,
        channel_display_name: String,
        branch_name: &str,
        history: crate::chat::Chat,
        copy_info: Option<String>,
    ) {
        let active = self.active;
        let _registered_name = self.channels.register(channel_display_name, new_ch.clone());
        let id = self.sessions.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let mut new_session = Session::new(id);
        new_session.chat = history;
        new_session.channel = Some(new_ch);
        apply_channel_permission_default(&mut new_session);
        if let Some(info) = copy_info {
            new_session.error = Some(info);
        }
        let cmd = format!("/branch {branch_name}");
        {
            // Nur ↑/↓-Eingabe-Historie + Editor leeren – wie bei allen anderen
            // Slash-Befehlen: KEIN UserPrompt-Event, damit der Befehl weder im
            // Chat angezeigt wird noch in die nächste LLM-Anfrage eingeht.
            let old = &mut self.sessions[active];
            old.push_history(&cmd);
            old.editor.clear();
        }
        self.sessions.push(new_session);
        self.active = self.sessions.len() - 1;
    }

    pub(crate) fn handle_branch_confirm_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.branch_confirm.take() else {
            return;
        };
        let max = d.options.len() - 1;
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            KeyCode::Esc => {
                self.branch_pending = None;
            }
            _ if down => {
                d.cursor = step_cursor(true, d.cursor, max);
                self.branch_confirm = Some(d);
            }
            _ if up => {
                d.cursor = step_cursor(false, d.cursor, max);
                self.branch_confirm = Some(d);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let pending = match self.branch_pending.take() {
                    Some(p) => p,
                    None => return,
                };
                let repo = crate::repo::RepoManager::discover(&pending.channel_root).ok();

                if pending.has_worktree {
                    // 4 Optionen: a) so nutzen, b) branch verschieben + übernehmen, c) committen & löschen, d) abbrechen
                    match d.cursor {
                        0 => {
                            // Branch & Worktree so nutzen wie sie sind
                            let wt_path = crate::repo::git_worktree_for_branch(
                                repo.as_ref()
                                    .map(|r| r.repo_root())
                                    .unwrap_or(&pending.channel_root),
                                &pending.branch_name,
                            );
                            if let Some(path) = wt_path {
                                self.user_branch_reuse(
                                    pending.channel_kind,
                                    pending.history,
                                    &pending.branch_name,
                                    &path,
                                    pending.podman_spec.clone(),
                                );
                            } else {
                                // Worktree weg → normal weiter
                                if let Some(ref repo) = repo {
                                    self.user_branch_proceed(
                                        pending.channel_kind,
                                        pending.channel_root,
                                        pending.history,
                                        &pending.branch_name,
                                        repo,
                                        false,
                                        false,
                                        pending.podman_spec.clone(),
                                    );
                                }
                            }
                        }
                        1 => {
                            // Branch auf HEAD verschieben, Arbeitsverzeichnis übernehmen
                            if let Some(ref repo) = repo {
                                self.user_branch_proceed(
                                    pending.channel_kind,
                                    pending.channel_root,
                                    pending.history,
                                    &pending.branch_name,
                                    repo,
                                    false,
                                    true,
                                    pending.podman_spec.clone(),
                                );
                            }
                        }
                        2 => {
                            // Alten Worktree committen, dann Stash übernehmen
                            let wt_path = crate::repo::git_worktree_for_branch(
                                repo.as_ref()
                                    .map(|r| r.repo_root())
                                    .unwrap_or(&pending.channel_root),
                                &pending.branch_name,
                            );
                            if let Some(path) = wt_path {
                                // 1. Ziel-Worktree committen
                                let _ = crate::repo::git_commit_all(
                                    &path,
                                    "Last Aidev Worktree Snapshot",
                                );
                                // 2. Aktuellen Worktree stashen
                                let had_stash =
                                    crate::repo::git(&pending.channel_root, &["stash"]).is_ok();
                                // 3. Stash auf Ziel-Worktree anwenden
                                if had_stash {
                                    let _ = crate::repo::git(&path, &["stash", "apply"]);
                                }
                                // 4. Stash im aktuellen Worktree wiederherstellen
                                if had_stash {
                                    let _ =
                                        crate::repo::git(&pending.channel_root, &["stash", "pop"]);
                                }
                            }
                            if let Some(ref repo) = repo {
                                self.user_branch_proceed(
                                    pending.channel_kind,
                                    pending.channel_root,
                                    pending.history,
                                    &pending.branch_name,
                                    repo,
                                    false,
                                    false,
                                    pending.podman_spec.clone(),
                                );
                            }
                        }
                        _ => {} // Abbrechen
                    }
                } else {
                    // 5 Optionen: a) nutzen + übernehmen, b) nutzen ohne stash, c) verschieben, d) committen & löschen, e) abbrechen
                    match d.cursor {
                        0 => {
                            // Branch nutzen, Arbeitsverzeichnis übernehmen
                            if let Some(ref repo) = repo {
                                self.user_branch_proceed(
                                    pending.channel_kind,
                                    pending.channel_root,
                                    pending.history,
                                    &pending.branch_name,
                                    repo,
                                    false,
                                    false,
                                    pending.podman_spec.clone(),
                                );
                            }
                        }
                        1 => {
                            // Branch nutzen, ohne Stash & Kopie
                            if let Some(ref repo) = repo {
                                self.user_branch_proceed(
                                    pending.channel_kind,
                                    pending.channel_root,
                                    pending.history,
                                    &pending.branch_name,
                                    repo,
                                    true,
                                    false,
                                    pending.podman_spec.clone(),
                                );
                            }
                        }
                        2 => {
                            // Branch auf HEAD verschieben, dann wie normal
                            if let Some(ref repo) = repo {
                                self.user_branch_proceed(
                                    pending.channel_kind,
                                    pending.channel_root,
                                    pending.history,
                                    &pending.branch_name,
                                    repo,
                                    false,
                                    true,
                                    pending.podman_spec.clone(),
                                );
                            }
                        }
                        _ => {} // Abbrechen
                    }
                }
            }
            _ => self.branch_confirm = Some(d),
        }
    }

    pub(crate) fn handle_enter(&mut self) {
        let active = self.active;
        // Senden während Streaming ignorieren (kein Doppel-Send).
        if matches!(
            self.sessions[active].phase,
            Phase::WaitingForLLM | Phase::WaitingForTool
        ) {
            return;
        }
        let content = self.sessions[active].editor.text_string();
        let content = content.trim().to_string();
        if content.is_empty() {
            return;
        }
        // `/compact` → manuelle Kontext-Kompaktierung: alte Turns durch eine
        // Zusammenfassung ersetzen, ohne einen LLM-Turn zu senden.
        if content == "/compact" {
            self.user_compact();
            return;
        }
        // `/end` → Session beenden (mit Worktree-Aufräumen)
        if content == "/end" {
            self.user_end();
            return;
        }
        // `/branch <name>` → neuen Git-Worktree anlegen, neuen Channel + Session erzeugen.
        if let Some(branch_name) = content
            .strip_prefix("/branch ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.user_branch(branch_name.to_string());
            return;
        }
        // `/commit <msg>` → alle Änderungen im aktuellen Worktree committen.
        if let Some(msg) = content
            .strip_prefix("/commit ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            self.user_commit(msg.to_string());
            return;
        }
        // `/model [alias]` → Modell für alle künftigen Anfragen dieser
        // Session wählen; ohne Argument öffnet der Auswahl-Dialog.
        if content == "/model" || content.starts_with("/model ") {
            self.user_model(content.strip_prefix("/model").unwrap_or("").trim());
            return;
        }
        // `/channel` → ChannelPicker öffnen (wie Alt+C).
        if content == "/channel" || content.starts_with("/channel ") {
            let s = self.active_mut();
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
            self.open_channel_picker();
            return;
        }
        // `/options` → Options-Dialog öffnen (wie Ctrl+O).
        if content == "/options" || content.starts_with("/options ") {
            let s = self.active_mut();
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
            self.open_options_dialog();
            return;
        }
        // `/header` → HTTP-Response-Header der letzten Antwort anzeigen (wie Alt+H).
        if content == "/header" || content.starts_with("/header ") {
            let s = self.active_mut();
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
            self.http_headers_dialog = true;
            return;
        }
        // `/run <shell-ausdruck>` → User-induzierter Tool-Call über den Kanal,
        // ohne das LLM zu befragen. Der komplette Ausdruck geht an die Shell.
        if let Some(expr) = parse_run_line(&content) {
            self.user_run(expr);
            return;
        }
        // `execute` auf einem Local-Kanal ist gefährlich: Solange der User noch
        // keine Entscheidung getroffen hat (`Ask`), wird VOR dem Absenden eine
        // Bestätigung eingeholt (eine gemerkte Wahl `Trusted`/`ConfirmEach`
        // überspringt den Dialog).
        let needs_confirm = self.sessions[active].permission == Permission::Execute
            && self.sessions[active]
                .channel
                .as_ref()
                .is_some_and(|ch| ch.kind() == ChannelKind::Local)
            && self.local_exec_mode == LocalExecMode::Ask;
        if needs_confirm {
            self.pre_send_confirm = Some(PreSendConfirm {
                session: active,
                cursor: 2, // Default: Abbrechen
            });
            return;
        }
        self.send_prompt(active);
    }

    pub(crate) fn send_prompt(&mut self, active: usize) {
        // Config und Endpoint der Session (inkl. `/model`-Auswahl) – Basis für
        // Kompaktierungs-Schwelle (`context_window`) und den Worker.
        let cfg = self.config.clone();
        let ep = match self.resolve_endpoint(active) {
            Ok(ep) => ep,
            Err(e) => {
                let s = &mut self.sessions[active];
                s.error = Some(e);
                return;
            }
        };
        // Kontext-Kompaktierung nötig? Entscheidung VOR dem Zurücksetzen der
        // Session, damit das `usage` des letzten Turns noch verfügbar ist.
        let compact = should_compact(&self.sessions[active], &cfg, &ep);
        // Live-Context-Basis des neuen Turns (monoton): Hat der letzte Turn eine
        // gesicherte Kontext-Größe geliefert (Usage), dient deren prompt_tokens
        // als harter Ankerpunkt (hier darf die Anzeige auch hart nach unten
        // korrigieren). Sonst wird ausschließlich geschätzt – dann zählen wir
        // alle Inhalte monoton weiter, sodass die Statusleiste nie wieder unter
        // den zuletzt geschätzten Wert fällt (nur Compaction senkt sie).
        let prev = self.sessions[active].prompt_base;
        let base_prompt = match self.sessions[active].last_usage() {
            Some(u) => u.prompt_tokens,
            None => prev.max((prompt_chars(&self.sessions[active]) as u64 / 4).max(1)),
        };
        // Die beim Enter aktuell gewählte Berechtigung gilt für diesen ganzen
        // Turn und bleibt als Default für die nächste Nachricht erhalten.
        let permission = self.sessions[active].permission;
        let content = self.sessions[active]
            .editor
            .text_string()
            .trim()
            .to_string();
        {
            let s = &mut self.sessions[active];
            // Eingabe für den ↑/↓-Durchlauf merken.
            s.push_history(&content);
            s.push_user_message(content, Some(permission), ep.model.clone());
            s.editor.clear();
            s.error = None;
            s.error_debug = None;
            s.retrying = None;
            s.aborted = false;
            s.prompt_base = base_prompt;
            s.live_usage_total = None; // neuer Turn → MID-STREAM-Wert zurücksetzen
            s.chat_follow = true;
            s.chat_anchor = ChatAnchor::default();
            s.chat_scroll_delta = None;
            s.compacting = false;
            s.active_model = Some(ep.model.clone());
            s.sent_at = Some(Instant::now());
            s.cancel = Arc::new(AtomicBool::new(false));
            s.phase = Phase::WaitingForLLM;
        }

        let id = self.sessions[active].id;
        let messages = crate::chat::api_messages(&self.sessions[active].chat);
        let cancel = self.sessions[active].cancel.clone();
        let channel = self.sessions[active].channel.clone();
        llm::spawn_worker(
            self.tx.clone(),
            id,
            cfg,
            ep,
            messages,
            cancel,
            channel,
            permission,
            compact,
            self.local_exec_mode,
        );
    }

    fn user_end(&mut self) {
        self.close_session();
    }

    pub(crate) fn handle_esc(&mut self) {
        let active = self.active;
        if matches!(
            self.sessions[active].phase,
            Phase::WaitingForLLM | Phase::WaitingForTool
        ) {
            self.sessions[active].cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub(crate) fn parse_run_line(input: &str) -> Option<String> {
    let rest = input.strip_prefix("/run")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim().to_string())
}

pub(crate) fn path_tail(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}
