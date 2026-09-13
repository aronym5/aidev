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
            tunnels: Selection::wrap_at(tunnels, start_tunnel_idx),
            host_paths: Selection::wrap_at(host_paths, start_host_idx),
            worktrees: Selection::wrap_at(worktrees, 0),
            col: 1,
            container_info: None,
            images_loaded: false, // <-- noch nicht geladen
            current_is_git: is_git,
            argv_path: None,
            edit: None,
            edit_error: None,
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

        // Host-Cursor und Pfad zuerst extrahieren (kein mutabler Borrow).
        let host_idx = match self.channel_builder.as_ref() {
            Some(b) => b.host_paths.nav.cursor(),
            None => return,
        };
        let path = self
            .channel_builder
            .as_ref()
            .and_then(|b| b.host_paths.items.get(host_idx))
            .map(|hp| hp.path.clone());

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
            builder.worktrees = Selection::wrap_at(worktrees, worktree_idx);
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
            .and_then(|b| b.host_paths.selected())
            .and_then(|hp| crate::channel::builder::default_image_for_path(&self.config, &hp.path));
        let Some(img) = default_img else {
            return false;
        };
        let Some(builder) = self.channel_builder.as_mut() else {
            return false;
        };
        match builder.tunnels.items.iter().position(|t| {
            t.image_name()
                .is_some_and(|n| crate::channel::builder::image_names_equal(n, &img))
        }) {
            Some(idx) if idx != builder.tunnels.nav.cursor() => {
                builder.tunnels.set_cursor(idx);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn update_builder_container(&self) {
        use crate::channel::builder::*;

        let (bg_path, current_image) = match self.channel_builder.as_ref() {
            Some(b) => {
                let tunnel = match b.tunnels.selected() {
                    Some(t) => t,
                    None => return,
                };
                // Nur bei Podman-Images prüfen, nicht bei Local
                let image_name = match tunnel.image_name() {
                    Some(name) => name.to_string(),
                    None => return, // Local → kein Container-Check
                };
                // Effektiven Pfad bestimmen (Worktree bevorzugen)
                let host_path = b.host_paths.selected().map(|hp| hp.path.clone());
                let wt_path = if b.current_is_git {
                    b.worktrees
                        .selected()
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
            .is_some_and(|b| b.edit.is_some())
        {
            match key.code {
                KeyCode::Esc => {
                    if let Some(b) = &mut self.channel_builder {
                        b.edit = None;
                        b.edit_error = None;
                    }
                }
                KeyCode::Enter => {
                    let is_branch = self
                        .channel_builder
                        .as_ref()
                        .is_some_and(|b| matches!(b.edit, Some(BuilderEdit::Branch(_))));
                    if is_branch {
                        self.builder_confirm_new_branch();
                    } else {
                        self.builder_confirm_host_path();
                    }
                }
                _ => {
                    if let Some(b) = &mut self.channel_builder {
                        if let Some(ed) = &mut b.edit {
                            let editor = match ed {
                                BuilderEdit::HostPath(e) | BuilderEdit::Branch(e) => e,
                            };
                            Self::handle_editor_key_static(editor, key);
                        }
                        // Beim Weiter-Tippen verschwindet die Fehlermeldung.
                        b.edit_error = None;
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
            // Neuer Branch: nur in der Worktree-Spalte (drei Spalten sichtbar).
            KeyCode::Char('b') => {
                self.builder_open_branch_input();
            }
            _ if down || up => {
                // In der aktiven Spalte mit Umlauf bewegen (ListNav `with_wrap`,
                // wie bisher `(idx + 1) % max` bzw. `idx - 1`).
                let col = match self.channel_builder.as_ref() {
                    Some(b) => b.col,
                    None => return,
                };
                let mut moved = false;
                let mut changed = false;
                if let Some(b) = &mut self.channel_builder {
                    match col {
                        // Spalten mit Umlauf bewegen (ListNav `with_wrap`).
                        0 if !b.tunnels.nav.is_empty() => {
                            b.tunnels.handle_move(&key, b.tunnels.nav.len() as u16);
                            moved = true;
                        }
                        1 if !b.host_paths.nav.is_empty() => {
                            let old = b.host_paths.nav.cursor();
                            b.host_paths.handle_move(&key, b.host_paths.nav.len() as u16);
                            moved = true;
                            changed = old != b.host_paths.nav.cursor();
                        }
                        2 if !b.worktrees.nav.is_empty() => {
                            b.worktrees.handle_move(&key, b.worktrees.nav.len() as u16);
                            moved = true;
                        }
                        _ => {}
                    }
                }
                // Nebeneffekte: Host-Wechsel lädt Worktrees/Default-Image und
                // dann den Container-Status; Tunnel-/Worktree-Wechsel
                // aktualisiert nur den Container-Status.
                match col {
                    1 if changed => {
                        self.update_builder_for_host();
                    }
                    _ if moved && col != 1 => {
                        self.update_builder_container();
                    }
                    _ => {}
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

        let tunnel = builder.tunnels.selected();
        let host_path = builder.host_paths.selected();
        let worktree = if builder.current_is_git {
            builder.worktrees.selected()
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

        // Ordnernamen der Basis: ist der gewählte Host-Pfad selbst eine
        // Repo-Wurzel (Haupt-Repo oder Worktree), der Ordnername des
        // Git-Haupt-Repos; nur bei Nicht-Repos/Unterverzeichnissen der
        // Ordnername des gewählten Pfads selbst.
        let base_folder = crate::channel::builder::base_name_for(&host_root);

        let effective_root = match &created_worktree {
            Some(wt) => wt.path.clone(),
            None => match worktree {
                Some(wt) if wt.has_worktree && !wt.path.as_os_str().is_empty() => wt.path.clone(),
                _ => host_root.clone(),
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
            // Kein existierender Container → WorkingDir + Reponame als Subdir:
            // bei Worktree-/Repo-Wurzel der Ordnername des Git-Haupt-Repos,
            // sonst der Ordnername des gewählten Pfads – so bleibt der
            // Gast-Pfad über verschiedene Worktrees hinweg stabil
            // ("/<wd>/<repo>").
            let project = crate::channel::builder::base_name_for(&host_root);
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
                s.set_channel(Some(ch));
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

    // --- Inline-Eingabefelder (A-Taste Host-Pfad, B-Taste Branch-Name) ---

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
            .selected()
            .map(|hp| hp.path.display().to_string())
            .unwrap_or_default();
        let mut editor = crate::editor::Editor::new(60);
        editor.set_text(&current_path);
        b.edit = Some(BuilderEdit::HostPath(editor));
        b.edit_error = None;
    }

    /// Bestätigt den eingegebenen Pfad: normalisiert, fügt ihn zur Liste hinzu
    /// (falls nicht schon vorhanden) und wählt ihn aus. Existiert der Zielpfad
    /// nicht, wird erst per Dialog gefragt, ob er angelegt werden soll
    /// (Enter = anlegen mit `mkdir -p`, Esc = zurück zur Eingabe).
    fn builder_confirm_host_path(&mut self) {
        let path = {
            let Some(b) = &mut self.channel_builder else {
                return;
            };
            let text = match &b.edit {
                Some(BuilderEdit::HostPath(editor)) => editor.text_string(),
                _ => return,
            };
            b.edit = None;
            b.edit_error = None;

            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                return;
            }
            std::path::PathBuf::from(&trimmed)
        };

        // Zielpfad existiert nicht → erst per Dialog bestätigen lassen, bevor
        // er in die Liste aufgenommen wird (Enter = `mkdir -p`, Esc = zurück).
        if !path.exists() {
            self.path_confirm = Some(PathConfirm { path });
            return;
        }

        self.builder_add_host_path(path);
    }

    /// Fügt `path` zur Host-Pfad-Liste hinzu (falls nicht schon vorhanden),
    /// wählt ihn aus und aktualisiert Worktrees/Container-Status. Gemeinsamer
    /// Abschluss für direkt bestätigte, existierende Pfade UND für per Dialog
    /// angelegte Verzeichnisse.
    fn builder_add_host_path(&mut self, path: std::path::PathBuf) {
        {
            let Some(b) = &mut self.channel_builder else {
                return;
            };
            let existing_idx = b
                .host_paths
                .items
                .iter()
                .position(|hp| hp.path == path);
            let new_idx = if let Some(idx) = existing_idx {
                // Bereits vorhanden → direkt auswählen
                idx
            } else {
                // Neu: an die Liste anhängen und auswählen (`push` hält die
                // Navigationslänge synchron).
                let idx = b.host_paths.items.len();
                b.host_paths
                    .push(crate::channel::builder::HostPath { path });
                idx
            };
            b.host_paths.set_cursor(new_idx);
        }
        // Worktrees/Container-Status für den neuen Pfad aktualisieren
        // (Borrow-Konflikt über den äußeren Scope hinweg gelöst).
        self.update_builder_for_host();
    }

    /// Öffnet das Pfad-Eingabefeld erneut (zur Korrektur) – z. B. nach
    /// Abbrechen oder Scheitern der Anlage eines nicht existierenden Pfads.
    fn builder_reopen_host_path_input(&mut self, text: String, error: Option<String>) {
        if let Some(b) = &mut self.channel_builder {
            if b.col != 1 {
                b.col = 1;
            }
            let mut editor = crate::editor::Editor::new(60);
            editor.set_text(&text);
            b.edit = Some(BuilderEdit::HostPath(editor));
            b.edit_error = error;
        }
    }

    /// Tastatur-Input des „Pfad existiert nicht“-Dialogs:
    /// Enter = anlegen (`mkdir -p`) und zurück zum Builder; Esc = abbrechen
    /// und zurück zur Pfad-Eingabe (Korrekturmöglichkeit).
    pub(crate) fn handle_path_confirm_key(&mut self, key: event::KeyEvent) {
        let Some(d) = self.path_confirm.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                // Abbrechen → zurück zur Eingabemaske.
                self.builder_reopen_host_path_input(d.path.to_string_lossy().to_string(), None);
            }
            KeyCode::Enter | KeyCode::Char(' ') => match std::fs::create_dir_all(&d.path) {
                Ok(()) => {
                    // Angelegt und zurück zum Builder (Pfad auswählen/registrieren).
                    self.builder_add_host_path(d.path);
                }
                Err(err) => {
                    // Anlage gescheitert → zurück zur Eingabe mit Fehlermeldung.
                    self.builder_reopen_host_path_input(
                        d.path.to_string_lossy().to_string(),
                        Some(format!("Could not create directory: {err}")),
                    );
                }
            },
            _ => {
                // Fremde Tasten verschlucken (modaler Dialog).
                self.path_confirm = Some(d);
            }
        }
    }

    /// Öffnet das Branch-Namen-Feld (nur in der Worktree-Spalte, drei Spalten
    /// sichtbar) – vorbelegt mit dem Namen des aktuell gewählten Branchs.
    fn builder_open_branch_input(&mut self) {
        let Some(b) = &mut self.channel_builder else {
            return;
        };
        // Nur in der Worktree-Spalte (bei Repo) sinnvoll.
        if b.col != 2 {
            return;
        }
        let suggestion = b
            .worktrees
            .selected()
            .map(|w| w.branch.clone())
            .unwrap_or_default();
        let mut editor = crate::editor::Editor::new(60);
        if !suggestion.is_empty() {
            editor.set_text(&suggestion);
        }
        b.edit = Some(BuilderEdit::Branch(editor));
        b.edit_error = None;
    }

    /// Bestätigt den Branch-Namen: legt `git branch <name> <Quell-Branch>` an
    /// (derselbe Commit wie der aktuell markierte Branch), lädt die Liste neu
    /// und setzt den Cursor auf den neuen Branch. Bei Fehler bleibt das
    /// Eingabefeld offen und die Meldung wird rot angezeigt.
    fn builder_confirm_new_branch(&mut self) {
        let (name, source, repo) = {
            let Some(b) = &mut self.channel_builder else {
                return;
            };
            let name = match &b.edit {
                Some(BuilderEdit::Branch(editor)) => editor.text_string().trim().to_string(),
                _ => return,
            };
            if name.is_empty() {
                b.edit_error = Some("Enter a branch name".into());
                return;
            }
            // Quelle: der aktuell markierte Branch der Worktree-Spalte.
            let source = b
                .worktrees
                .selected()
                .map(|w| w.branch.clone())
                .filter(|s| !s.is_empty());
            let repo = match b.host_paths.selected() {
                Some(hp) => hp.path.clone(),
                None => return,
            };
            (name, source, repo)
        };

        // `git branch <name> [<quelle>]` – ohne Quelle startet vom HEAD.
        let mut args: Vec<&str> = vec!["branch", &name];
        if let Some(ref src) = source {
            args.push(src);
        }
        match crate::repo::git(&repo, &args) {
            Ok(_) => {
                // Liste neu laden, Cursor auf den neuen Branch setzen.
                if let Some(b) = &mut self.channel_builder {
                    b.edit = None;
                    b.edit_error = None;
                    let wt =
                        crate::channel::builder::git_list_worktrees_and_branches(&repo);
                    let idx = wt.iter().position(|w| w.branch == name).unwrap_or(0);
                    b.worktrees = Selection::wrap_at(wt, idx);
                }
                // Container-Status für die neue Auswahl auffrischen.
                self.update_builder_container();
            }
            Err(err) => {
                if let Some(b) = &mut self.channel_builder {
                    b.edit_error = Some(format!("Could not create branch: {err}"));
                }
            }
        }
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
