use super::*;
use crossterm::event::{self, KeyCode};

impl App {
    pub(crate) fn open_channel_builder(&mut self) {
        use crate::channel::builder::*;

        // === Sofort: Host-Pfade sammeln (schnell, nur Config + cwd) ===
        let argv = self
            .channel_builder
            .as_ref()
            .and_then(|b| b.argv_path.clone());
        let host_paths = collect_host_paths(&self.config.paths, argv.as_deref());
        let tunnels = vec![Tunnel::Local];

        // Cursor nahe am aktuellen Kanal positionieren
        let (start_host_idx, start_tunnel_idx) =
            if let Some(ch) = &self.sessions[self.active].channel {
                let host = ch.host_root();
                let host_str = host
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let host_idx = host_paths
                    .iter()
                    .enumerate()
                    .find(|(_, hp)| {
                        host_str.contains(&hp.path.display().to_string())
                            || hp.path.display().to_string() == host_str
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                (host_idx, 0)
            } else {
                (0, 0)
            };

        // Git-Worktrees für den Start-Pfad laden (git ist schnell). Nur wenn
        // der Pfad DIREKT eine Repo-Wurzel/Worktree ist – Unterverzeichnisse
        // gelten als repo-freie Host-Ordner.
        let current_path = host_paths.get(start_host_idx).map(|hp| hp.path.clone());
        let is_git = current_path.as_ref().is_some_and(|p| is_repo_root(p));
        let worktrees = current_path
            .as_ref()
            .filter(|p| is_repo_root(p))
            .map(|p| git_list_worktrees_and_branches(p))
            .unwrap_or_default();

        // Skeleton-State sofort anzeigen
        let state = ChannelBuilderState {
            tunnels,
            host_paths,
            worktrees,
            col: 1,
            tunnel_idx: start_tunnel_idx,
            host_idx: start_host_idx,
            worktree_idx: 0,
            container_info: None,
            images_loaded: false, // <-- noch nicht geladen
            current_is_git: is_git,
            argv_path: None,
            host_path_edit: None,
        };
        self.channel_builder = Some(state);

        // === Asynchron: Podman-Images im Hintergrund laden ===
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut images = podman_list_images();
            if images.is_empty() && podman_available() && podman_has_alpine() {
                images.push("alpine".into());
            }
            // WorkingDir für jedes Image ermitteln
            let images_with_wd: Vec<(String, Option<String>)> = images
                .into_iter()
                .map(|img| {
                    let wd = podman_inspect_working_dir(&img);
                    (img, wd)
                })
                .collect();
            let _ = tx.send(WorkerEvent::BuilderLoaded(images_with_wd, None, Vec::new()));
        });

        // Container-Status für initialen Tunnel prüfen (nach dem State gesetzt ist)
        self.update_builder_container();
    }

    fn update_builder_for_host(&mut self) {
        use crate::channel::builder::*;

        // Host-Index und Pfad zuerst extrahieren (kein mutable Borrow)
        let host_idx = match self.channel_builder.as_ref() {
            Some(b) => b.host_idx,
            None => return,
        };
        let path = self
            .channel_builder
            .as_ref()
            .and_then(|b| b.host_paths.get(host_idx).map(|hp| hp.path.clone()));

        // Worktrees laden (git ist schnell). Nur wenn der Pfad DIREKT eine
        // Repo-Wurzel/Worktree ist; Unterverzeichnisse gelten als
        // repo-freie Host-Ordner (kein Worktree-Spalte, keine Branch-Logik).
        let is_repo = path.as_ref().is_some_and(|p| is_repo_root(p));
        let worktrees = if is_repo {
            path.as_ref()
                .map(|p| git_list_worktrees_and_branches(p))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let worktree_idx = worktrees.iter().position(|w| w.is_main).unwrap_or(0);

        // Synchrones Update: Worktrees + Default-Image (schnell)
        if let Some(builder) = &mut self.channel_builder {
            builder.current_is_git = is_repo;
            builder.worktrees = worktrees;
            builder.worktree_idx = worktree_idx;
            builder.container_info = None; // wird async geladen
                                           // Stand der Cursor in der (jetzt entfallenen) Worktree-Spalte?
                                           // Zurück auf die Host-Spalte.
            if !is_repo && builder.col > 1 {
                builder.col = 1;
            }
        }
        self.apply_builder_default_image();

        // Container-Status nur prüfen wenn ein Podman-Image gewählt ist
        self.update_builder_container();
    }

    pub(crate) fn apply_builder_default_image(&mut self) -> bool {
        let default_img = self
            .channel_builder
            .as_ref()
            .and_then(|b| b.host_paths.get(b.host_idx))
            .and_then(|hp| crate::channel::builder::default_image_for_path(&self.config, &hp.path));
        let Some(img) = default_img else {
            return false;
        };
        let Some(builder) = self.channel_builder.as_mut() else {
            return false;
        };
        match builder.tunnels.iter().position(|t| {
            t.image_name()
                .is_some_and(|n| crate::channel::builder::image_names_equal(n, &img))
        }) {
            Some(idx) if idx != builder.tunnel_idx => {
                builder.tunnel_idx = idx;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn update_builder_container(&self) {
        use crate::channel::builder::*;

        let (bg_path, current_image) = match self.channel_builder.as_ref() {
            Some(b) => {
                let tunnel = match b.tunnels.get(b.tunnel_idx) {
                    Some(t) => t,
                    None => return,
                };
                // Nur bei Podman-Images prüfen, nicht bei Local
                let image_name = match tunnel.image_name() {
                    Some(name) => name.to_string(),
                    None => return, // Local → kein Container-Check
                };
                // Effektiven Pfad bestimmen (Worktree bevorzugen)
                let host_path = b.host_paths.get(b.host_idx).map(|hp| hp.path.clone());
                let wt_path = if b.current_is_git {
                    b.worktrees
                        .get(b.worktree_idx)
                        .filter(|w| w.has_worktree && !w.path.as_os_str().is_empty())
                        .map(|w| w.path.clone())
                } else {
                    None
                };
                let path = wt_path.or(host_path);
                match path {
                    Some(p) => (p, Some(image_name)),
                    None => return,
                }
            }
            None => return,
        };

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let info = find_container_for(&bg_path, current_image.as_deref());
            let _ = tx.send(WorkerEvent::BuilderLoaded(Vec::new(), info, Vec::new()));
        });
    }

    pub(crate) fn handle_builder_key(&mut self, key: event::KeyEvent) {
        // Pfad-Input-Modus: alle Tasten an den Editor weiterleiten
        if self
            .channel_builder
            .as_ref()
            .is_some_and(|b| b.host_path_edit.is_some())
        {
            match key.code {
                KeyCode::Esc => {
                    if let Some(b) = &mut self.channel_builder {
                        b.host_path_edit = None;
                    }
                }
                KeyCode::Enter => {
                    self.builder_confirm_host_path();
                }
                _ => {
                    if let Some(b) = &mut self.channel_builder {
                        if let Some(editor) = &mut b.host_path_edit {
                            Self::handle_editor_key_static(editor, key);
                        }
                    }
                }
            }
            return;
        }

        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        let left = key.code == KeyCode::Left || key.code == KeyCode::Char('h');
        let right = key.code == KeyCode::Right || key.code == KeyCode::Char('l');

        match key.code {
            KeyCode::Esc => {
                self.channel_builder = None;
            }
            KeyCode::Enter => {
                self.builder_select();
            }
            KeyCode::Char('a') => {
                self.builder_open_host_path_input();
            }
            _ if down => {
                let mut needs_container_update = false;
                if let Some(b) = &mut self.channel_builder {
                    match b.col {
                        0 => {
                            let max = b.tunnels.len();
                            if max > 0 {
                                b.tunnel_idx = (b.tunnel_idx + 1) % max;
                                needs_container_update = true;
                            }
                        }
                        1 => {
                            let max = b.host_paths.len();
                            if max > 0 {
                                let old_idx = b.host_idx;
                                b.host_idx = (b.host_idx + 1) % max;
                                if old_idx != b.host_idx {
                                    let _ = b;
                                    self.update_builder_for_host();
                                    return;
                                }
                            }
                        }
                        2 => {
                            let max = b.worktrees.len();
                            if max > 0 {
                                b.worktree_idx = (b.worktree_idx + 1) % max;
                                needs_container_update = true;
                            }
                        }
                        _ => {}
                    }
                }
                if needs_container_update {
                    self.update_builder_container();
                }
            }
            _ if up => {
                let mut needs_container_update = false;
                if let Some(b) = &mut self.channel_builder {
                    match b.col {
                        0 => {
                            let max = b.tunnels.len();
                            if max > 0 {
                                if b.tunnel_idx == 0 {
                                    b.tunnel_idx = max - 1;
                                } else {
                                    b.tunnel_idx -= 1;
                                }
                                needs_container_update = true;
                            }
                        }
                        1 => {
                            let max = b.host_paths.len();
                            if max > 0 {
                                let old_idx = b.host_idx;
                                if b.host_idx == 0 {
                                    b.host_idx = max - 1;
                                } else {
                                    b.host_idx -= 1;
                                }
                                if old_idx != b.host_idx {
                                    let _ = b;
                                    self.update_builder_for_host();
                                    return;
                                }
                            }
                        }
                        2 => {
                            let max = b.worktrees.len();
                            if max > 0 {
                                if b.worktree_idx == 0 {
                                    b.worktree_idx = max - 1;
                                } else {
                                    b.worktree_idx -= 1;
                                }
                                needs_container_update = true;
                            }
                        }
                        _ => {}
                    }
                }
                if needs_container_update {
                    self.update_builder_container();
                }
            }
            _ if left => {
                if let Some(b) = &mut self.channel_builder {
                    if b.col > 0 {
                        b.col -= 1;
                        if b.col == 2 && !b.current_is_git {
                            b.col = 1;
                        }
                    }
                }
            }
            _ if right => {
                if let Some(b) = &mut self.channel_builder {
                    if b.col < 2 {
                        if b.col == 1 && !b.current_is_git {
                            // nicht weiter rechts bei Nicht-Repos
                        } else {
                            b.col += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn builder_select(&mut self) {
        let builder = match self.channel_builder.take() {
            Some(b) => b,
            None => return,
        };

        let tunnel = builder.tunnels.get(builder.tunnel_idx);
        let host_path = builder.host_paths.get(builder.host_idx);
        let worktree = if builder.current_is_git {
            builder.worktrees.get(builder.worktree_idx)
        } else {
            None
        };

        let host_root = match host_path {
            Some(hp) => hp.path.clone(),
            None => return,
        };

        // Wurde in der rechten Spalte ein Branch *ohne* bestehendes Worktree
        // gewählt (Label „… (Kein Worktree)“), legen wir das Worktree jetzt an
        // (analog zum /branch-Befehl). Andernfalls würde die Auswahl ignoriert
        // und stattdessen einfach das Host-Verzeichnis gemountet.
        let pending_branch: Option<String> = if builder.current_is_git {
            worktree
                .filter(|wt| !wt.has_worktree && !wt.branch.is_empty())
                .map(|wt| wt.branch.clone())
        } else {
            None
        };

        let created_worktree: Option<crate::repo::WorktreeInfo> =
            if let Some(branch) = &pending_branch {
                match crate::repo::RepoManager::discover(&host_root) {
                    Ok(repo) => {
                        // Bewusst OHNE Stash/Übernahme: Hier wird nur ein
                        // Container für den gewählten Branch gestartet –
                        // `git worktree add` braucht kein sauberes Workdir,
                        // und uncommittetes Arbeiten im Quellverzeichnis
                        // soll unangetastet bleiben. Das Mitnehmen von
                        // Änderungen ist das explizite `/branch`-Angebot.
                        match repo.create_worktree(branch, Some(&host_root)) {
                            Ok(info) => Some(info),
                            Err(e) => {
                                let s = self.active_mut();
                                s.error = Some(format!(
                                    "Could not create worktree for branch '{branch}': {e}"
                                ));
                                s.error_debug = None;
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let s = self.active_mut();
                        s.error = Some(format!("Could not determine Git repo: {e}"));
                        s.error_debug = None;
                        return;
                    }
                }
            } else {
                None
            };

        // Ordnernamen der Basis: bei Repos das Repo-Basisverzeichnis
        // (host_root), bei Nicht-Repos entsprechend der Ordner des
        // gemounteten Host-Verzeichnisses (effective_root == host_root).
        let base_folder = host_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".to_string());

        let effective_root = match &created_worktree {
            Some(wt) => wt.path.clone(),
            None => match worktree {
                Some(wt) if wt.has_worktree && !wt.path.as_os_str().is_empty() => wt.path.clone(),
                _ => host_root,
            },
        };

        let (kind, image, mut workdir) = match tunnel {
            Some(crate::channel::builder::Tunnel::Local) => {
                ("local".to_string(), None, "/app".to_string())
            }
            Some(crate::channel::builder::Tunnel::Image { name, working_dir }) => {
                let wd = working_dir.as_deref().unwrap_or("/app").to_string();
                ("podman".to_string(), Some(name.clone()), wd)
            }
            None => return,
        };

        // Existierenden Container berücksichtigen: mount_destination
        // hat Vorrang
        let has_existing_container = builder
            .container_info
            .as_ref()
            .and_then(|i| i.mount_destination.as_ref())
            .is_some_and(|d| !d.is_empty());

        if has_existing_container {
            // Container existiert → dessen mount_destination verwenden
            workdir = builder
                .container_info
                .as_ref()
                .unwrap()
                .mount_destination
                .clone()
                .unwrap();
        } else if kind == "podman" {
            // Kein existierender Container → WorkingDir + Projektname als Subdir
            let project = effective_root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "app".to_string());
            workdir = format!("{}/{}", workdir.trim_end_matches('/'), project);
        }

        // Branch: beim ggf. neu angelegten Worktree bzw. beim ausgewählten
        // Worktree mit Checkout. Bei einem Branch ohne Worktree (vor Anlage)
        // ist `created_worktree` gesetzt; ansonsten nur bei echten Worktrees.
        let branch: Option<&str> = match &created_worktree {
            Some(wt) => Some(wt.branch.as_str()),
            None => worktree
                .filter(|wt| wt.has_worktree && !wt.path.as_os_str().is_empty())
                .map(|wt| wt.branch.as_str()),
        };

        // Läuft bereits ein passender Container (statt einen neuen zu starten),
        // bevorzugen wir einen vorhandenen Kanal aus der Registry, der genau
        // diesen Container nutzt – statt einen Duplikat-Kanal anzulegen. Erst
        // wenn kein solcher Kanal existiert, erzeugen wir einen neuen, der an
        // den bestehenden Container andockt (Attach-Modus).
        let running_container = builder
            .container_info
            .as_ref()
            .filter(|i| {
                i.status == "running"
                    && i.mount_destination
                        .as_deref()
                        .is_some_and(|d| !d.is_empty())
            })
            .map(|i| i.name.clone());

        if let Some(existing) = &running_container {
            if let Some((_reg_name, ch)) = self.channels.find_by_container(existing) {
                // Bereits vorhandenen Kanal (am selben Container) wiederverwenden.
                Self::warmup_channel(&ch);
                let s = self.active_mut();
                s.channel = Some(ch);
                s.error = None;
                s.error_debug = None;
                apply_channel_permission_default(s);
                return;
            }
        }

        // Neuer Kanal: falls ein passender Container läuft, aber kein passender
        // Kanal existiert, andocken (Attach-Modus) statt einen eigenen
        // Container zu starten.
        let attach_existing = running_container;

        let cfg = crate::config::ChannelConfig {
            kind: kind.clone(),
            image: if attach_existing.is_some() {
                None
            } else {
                image.clone()
            },
            container: attach_existing.clone(),
            // Expliziter Container-Name (Run-Modus):
            // aidev-reponame-branchname-imagename / aidev-hostfoldername-imagename
            run_container: if attach_existing.is_some() {
                None
            } else {
                image.as_deref().map(|img| {
                    crate::channel::builder::run_container_name(&base_folder, branch, img)
                })
            },
            workdir,
            host_root: Some(effective_root.display().to_string()),
            home: None,
        };

        let name = match (tunnel, worktree) {
            // Statusleisten-Name: "imagename worktreepfad"
            (Some(crate::channel::builder::Tunnel::Image { name, .. }), _) => {
                crate::channel::builder::run_channel_name(name, &effective_root)
            }
            // Local-Kanal: einfach der Workdir-Pfad
            (Some(crate::channel::builder::Tunnel::Local), _) => {
                effective_root.display().to_string()
            }
            _ => return,
        };

        match crate::channel::channel_from_config(
            &name,
            &cfg,
            self.config.timeout_secs,
            self.channels.managed.clone(),
            created_worktree,
            self.config.podman.usermapping,
        ) {
            Ok(ch) => {
                let registered_name = self.channels.register(name.clone(), ch.clone());
                Self::warmup_channel(&ch);
                self.bind_active_channel(registered_name, ch);
            }
            Err(err) => {
                let s = self.active_mut();
                s.error = Some(format!("Could not create channel: {err}"));
                s.error_debug = None;
            }
        }
    }

    pub(crate) fn duplicate_active_channel(&mut self) {
        let duplicate = {
            let active = self.active;
            match &self.sessions[active].channel {
                Some(ch) => ch.dup(),
                None => return,
            }
        };
        match duplicate {
            Ok(dup) => {
                // Duplikate erscheinen auch in der Kanal-Auswahl (Alt+O).
                let tag = dup.root();
                self.channels.register(dup.label(), dup.clone());
                self.bind_active_channel(tag, dup);
            }
            Err(err) => {
                let s = self.active_mut();
                s.error = Some(format!("Cannot duplicate: {err}"));
                s.error_debug = None;
            }
        }
    }

    // --- Pfad-Input (A-Taste im Host-Spalte) ---

    /// Öffnet das Pfad-Editor-Feld mit dem aktuell gewählten Host-Pfad als
    /// Ausgangswert. Der User kann den Pfad frei anpassen (z.B. in einen
    /// über/unterordner wechseln).
    fn builder_open_host_path_input(&mut self) {
        let Some(b) = &mut self.channel_builder else {
            return;
        };
        // Nur im Host-Spalte (col == 1) sinnvoll
        if b.col != 1 {
            return;
        }
        let current_path = b
            .host_paths
            .get(b.host_idx)
            .map(|hp| hp.path.display().to_string())
            .unwrap_or_default();
        let mut editor = crate::editor::Editor::new(60);
        editor.set_text(&current_path);
        b.host_path_edit = Some(editor);
    }

    /// Bestätigt den eingegebenen Pfad: normalisiert, fügt ihn zur Liste hinzu
    /// (falls nicht schon vorhanden) und wählt ihn aus.
    fn builder_confirm_host_path(&mut self) {
        let Some(b) = &mut self.channel_builder else {
            return;
        };
        let text = match &b.host_path_edit {
            Some(editor) => editor.text_string(),
            None => return,
        };
        b.host_path_edit = None;

        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        let path = std::path::PathBuf::from(&trimmed);

        // Prüfen ob der Pfad bereits in der Liste ist
        let existing_idx = b.host_paths.iter().position(|hp| hp.path == path);

        let new_idx = if let Some(idx) = existing_idx {
            // Bereits vorhanden → direkt auswählen
            idx
        } else {
            // Neu: an die Liste anhängen und auswählen
            let idx = b.host_paths.len();
            b.host_paths
                .push(crate::channel::builder::HostPath { path });
            idx
        };

        b.host_idx = new_idx;

        // Worktrees/Container-Status für den neuen Pfad aktualisieren
        // (über den Borrow-Konflikt hinweg)
        let _ = b;
        self.update_builder_for_host();
    }

    /// Leitet Tastatureingaben an den pfadspezifischen Editor weiter –
    /// identische Logik wie `handle_key` in `App`, aber als statische
    /// Funktion, damit `handle_builder_key` den Builder-State nicht
    /// doppelt mutabel ausleihen muss.
    fn handle_editor_key_static(editor: &mut crate::editor::Editor, key: event::KeyEvent) {
        let shift = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::SHIFT);
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(c) if !ctrl => editor.insert_char(c),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete_at_cursor(),
            KeyCode::Left if ctrl => editor.move_word(-1, shift),
            KeyCode::Right if ctrl => editor.move_word(1, shift),
            KeyCode::Left => editor.move_cursor(-1, shift),
            KeyCode::Right => editor.move_cursor(1, shift),
            KeyCode::Home => editor.move_home(shift),
            KeyCode::End => editor.move_end(shift),
            _ => {}
        }
    }
}
