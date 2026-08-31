use super::*;
use crossterm::event::{self, KeyCode};

impl App {
    pub(crate) fn begin_close_channel(&mut self, name: &str, kind: CloseKind) {
        self.channel_picker = None;
        // Die gerade geschlossene Session zählt bei der Aktivitätsprüfung
        // nicht mit (sie soll ja geschlossen werden).
        let skip = if kind == CloseKind::Session {
            Some(self.active)
        } else {
            None
        };
        // Aktivitätsprüfung: Ist einer derSessions, die diesen Kanal nutzen,
        // gerade im Streaming / Tool-Einsatz?
        let active = self.sessions.iter().enumerate().any(|(i, s)| {
            if Some(i) == skip {
                return false;
            }
            let matches = s
                .channel
                .as_ref()
                .and_then(|ch| self.channels.find_name(ch))
                .as_deref()
                == Some(name);
            matches && (s.phase == Phase::WaitingForLLM || s.phase == Phase::WaitingForTool || !s.open_tool_ids.is_empty())
        });
        if active {
            self.channel_close = Some(ChannelClose {
                name: name.to_string(),
                kind,
                phase: ChannelClosePhase::ActiveConfirm { cursor: 1 },
            });
        } else {
            self.close_channel_worktree_phase(name, kind);
        }
    }

    fn close_channel_worktree_phase(&mut self, name: &str, kind: CloseKind) {
        // Laufende Worker auf diesem Kanal abbrechen, bevor wir aufräumen
        // (analog zum ursprünglichen `close_session`).
        // Laufende Worker auf diesem Kanal abbrechen (Arc klonen, damit wir
        // self.channels nicht parallel halten müssen).
        let channel_arc = self.channels.get(name);
        for s in &self.sessions {
            if let (Some(arc_ch), Some(target)) = (&s.channel, &channel_arc) {
                if Arc::ptr_eq(arc_ch, target) {
                    s.cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        let wt = self
            .channels
            .get(name)
            .and_then(|ch| ch.owned_worktree().cloned());
        if let Some(wt) = wt {
            if let Ok(repo) = crate::repo::RepoManager::discover(&wt.path) {
                match repo.is_clean(&wt.path) {
                    Ok(true) => {
                        // Fehlschlag beim Aufräumen ist nicht fatal – der Kanal
                        // schließt trotzdem. Ein Konsolen-Print würde das
                        // TUI-Layout zerschießen.
                        let _ = repo.remove_worktree(&wt.name, true);
                        self.close_channel_container_phase(name, kind);
                        return;
                    }
                    Ok(false) => {
                        let summary = repo
                            .status_summary(&wt.path)
                            .unwrap_or_else(|_| "Worktree has uncommitted changes.".to_string());
                        self.channel_close = Some(ChannelClose {
                            name: name.to_string(),
                            kind,
                            phase: ChannelClosePhase::Worktree {
                                summary,
                                cursor: 1,
                                options: CHANNEL_CLOSE_WORKTREE_OPTIONS.to_vec(),
                            },
                        });
                        return;
                    }
                    Err(_) => {}
                }
            }
        }
        // Kein Worktree bzw. nicht prüfbar → Container-Phase.
        self.close_channel_container_phase(name, kind);
    }

    fn close_channel_container_phase(&mut self, name: &str, kind: CloseKind) {
        if let Some(notes) = self
            .channels
            .get(name)
            .and_then(|ch| ch.essential_changes())
        {
            self.channel_close = Some(ChannelClose {
                name: name.to_string(),
                kind,
                phase: ChannelClosePhase::Container { notes, cursor: 1 },
            });
            return;
        }
        self.finalize_close_channel(name, kind);
    }

    fn finalize_close_channel(&mut self, name: &str, kind: CloseKind) {
        if let Some(container) = self.channels.get(name).and_then(|ch| ch.container_name()) {
            self.channels.stop_one(&container);
        }
        // Alle Sessions, die diesen Kanal nutzen, auf "kein Kanal" setzen.
        if let Some(target) = self.channels.get(name) {
            for s in &mut self.sessions {
                if let Some(ch) = &s.channel {
                    if Arc::ptr_eq(ch, &target) {
                        s.channel = None;
                    }
                }
            }
        }
        // Konfigurierte Kanäle bleiben bei Session-Schluss registriert (nur
        // Kanäle mit eigenem Worktree – also die per /branch bzw. Alt+D
        // erzeugten – werden aufgeräumt); beim expliziten Kanal-Schließen
        // (Picker) wird immer ausgetragen.
        let owned = self
            .channels
            .get(name)
            .map(|ch| ch.owned_worktree().is_some())
            .unwrap_or(false);
        if kind == CloseKind::Picker || owned {
            self.channels.unregister(name);
        }
        self.channel_close = None;
        match kind {
            CloseKind::Picker => self.open_channel_picker(),
            CloseKind::Session => self.finish_close_session(),
        }
    }

    pub(crate) fn handle_channel_close_key(&mut self, key: event::KeyEvent) {
        let Some(d) = self.channel_close.take() else {
            return;
        };
        let kind = d.kind;
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        // Abbruch: Dialog zu; je nach Ursprung Picker neu öffnen oder die
        // Session (und damit der Kanal) einfach offen lassen.
        let abort = |app: &mut App, _name: String, kind: CloseKind| {
            app.channel_close = None;
            match kind {
                CloseKind::Picker => app.open_channel_picker(),
                CloseKind::Session => {}
            }
        };
        match &d.phase {
            ChannelClosePhase::ActiveConfirm { cursor } => {
                let mut cursor = *cursor;
                match key.code {
                    KeyCode::Esc => abort(self, d.name.clone(), kind),
                    _ if down => {
                        cursor = (cursor + 1).min(1);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::ActiveConfirm { cursor },
                        });
                    }
                    _ if up => {
                        cursor = cursor.saturating_sub(1);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::ActiveConfirm { cursor },
                        });
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        if cursor == 0 {
                            self.close_channel_worktree_phase(&d.name, kind);
                        } else {
                            abort(self, d.name.clone(), kind);
                        }
                    }
                    _ => {
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::ActiveConfirm { cursor },
                        });
                    }
                }
            }
            ChannelClosePhase::Worktree {
                summary,
                cursor,
                options,
            } => {
                let mut cursor = *cursor;
                let summary = summary.clone();
                let options = options.clone();
                let max = options.len() - 1;
                match key.code {
                    KeyCode::Esc => abort(self, d.name.clone(), kind),
                    _ if down => {
                        cursor = (cursor + 1).min(max);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Worktree {
                                summary,
                                cursor,
                                options,
                            },
                        });
                    }
                    _ if up => {
                        cursor = cursor.saturating_sub(1);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Worktree {
                                summary,
                                cursor,
                                options,
                            },
                        });
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let wt = self
                            .channels
                            .get(&d.name)
                            .and_then(|ch| ch.owned_worktree().cloned());
                        match cursor {
                            0 => abort(self, d.name.clone(), kind),
                            1 => {
                                // Worktree löschen, dann Container-Phase.
                                if let Some(wt) = wt {
                                    if let Ok(repo) = crate::repo::RepoManager::discover(&wt.path) {
                                        // Nicht fatal – Kanal schließt trotzdem.
                                        let _ = repo.remove_worktree(&wt.name, true);
                                    }
                                }
                                self.close_channel_container_phase(&d.name, kind);
                            }
                            2 => {
                                // Behalten → Container-Phase.
                                self.close_channel_container_phase(&d.name, kind);
                            }
                            3 => {
                                // Committen & löschen, dann Container-Phase.
                                if let Some(wt) = wt {
                                    if let Ok(repo) = crate::repo::RepoManager::discover(&wt.path) {
                                        let _ = crate::repo::git_commit_all(
                                            &wt.path,
                                            "Last Aidev Worktree Snapshot",
                                        );
                                        // Nicht fatal – Kanal schließt trotzdem.
                                        let _ = repo.remove_worktree(&wt.name, true);
                                    }
                                }
                                self.close_channel_container_phase(&d.name, kind);
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Worktree {
                                summary,
                                cursor,
                                options,
                            },
                        });
                    }
                }
            }
            ChannelClosePhase::Container { notes, cursor } => {
                let mut cursor = *cursor;
                let notes = notes.clone();
                match key.code {
                    KeyCode::Esc => abort(self, d.name.clone(), kind),
                    _ if down => {
                        cursor = (cursor + 1).min(1);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Container { notes, cursor },
                        });
                    }
                    _ if up => {
                        cursor = cursor.saturating_sub(1);
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Container { notes, cursor },
                        });
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        if cursor == 0 {
                            self.finalize_close_channel(&d.name, kind);
                        } else {
                            abort(self, d.name.clone(), kind);
                        }
                    }
                    _ => {
                        self.channel_close = Some(ChannelClose {
                            name: d.name.clone(),
                            kind,
                            phase: ChannelClosePhase::Container { notes, cursor },
                        });
                    }
                }
            }
        }
    }

    pub(crate) fn maybe_open_stop_confirm(&mut self) -> bool {
        let (entries, more) = self.stop_confirm_entries();
        if entries.is_empty() {
            return false;
        }
        self.stop_confirm = Some(StopConfirm {
            entries,
            more,
            // Default: Abbrechen (sicher) – Stopp verliert ungesicherten Zustand.
            cursor: 1,
        });
        true
    }

    fn stop_confirm_entries(&self) -> (Vec<StopConfirmEntry>, usize) {
        const MAX: usize = 5;
        let mut all: Vec<StopConfirmEntry> = Vec::new();
        for name in self.channels.names() {
            let Some(ch) = self.channels.get(&name) else {
                continue;
            };
            if let Some(notes) = ch.essential_changes() {
                if notes.is_empty() {
                    continue;
                }
                all.push(StopConfirmEntry {
                    channel: name,
                    container: ch.container_name(),
                    notes,
                });
            }
        }
        let more = all.len().saturating_sub(MAX);
        let entries = all.into_iter().take(MAX).collect();
        (entries, more)
    }

    pub(crate) fn handle_stop_confirm_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.stop_confirm.take() else {
            return;
        };
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            // Esc bzw. „Nein, abbrechen“: nichts beenden, Dialog zu.
            KeyCode::Esc => {}
            _ if down => {
                d.cursor = step_cursor(true, d.cursor, 1);
                self.stop_confirm = Some(d);
            }
            _ if up => {
                d.cursor = step_cursor(false, d.cursor, 1);
                self.stop_confirm = Some(d);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if d.cursor == 0 {
                    // Explizit bestätigt → beenden (Container werden gestoppt).
                    self.quit = true;
                }
            }
            _ => self.stop_confirm = Some(d),
        }
    }
}
