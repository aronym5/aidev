use super::*;
use crossterm::event::{self, KeyCode};
use std::sync::Arc;

impl App {
    pub(crate) fn open_channel_picker(&mut self) {
        // Alle registrierten Kanäle – alphabetisch sortiert.
        let mut names: Vec<String> = self.channels.names();
        names.sort();

        let mut items = vec!["(kein Kanal)".to_string()];
        let mut keys: Vec<Option<String>> = vec![None];
        for name in &names {
            items.push(name.clone());
            keys.push(Some(name.clone()));
        }
        // Am Ende: „new channel" – Öffnet den Channel Builder.
        items.push("new channel".to_string());
        keys.push(None);

        // Cursor auf den gebundenen Kanal der aktiven Session legen (oder 0).
        let active_name = self.sessions[self.active]
            .channel
            .as_ref()
            .and_then(|ch| self.channels.find_name(ch));
        let cursor = match &active_name {
            Some(name) => items.iter().position(|i| i == name).unwrap_or(0),
            None => 0,
        };
        self.channel_picker = Some(ChannelPicker {
            items,
            keys,
            cursor,
        });
    }

    pub(crate) fn handle_picker_key(&mut self, key: event::KeyEvent) {
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            KeyCode::Esc => self.channel_picker = None,
            KeyCode::Delete | KeyCode::Backspace => {
                // Gewählten Kanal aus der Registry entfernen (Entf). Nur
                // bei bestehenden Kanälen – „(kein Kanal)" (cursor 0) und
                // „new channel" (letzte Zeile) lassen sich nicht schließen.
                if let Some(p) = &self.channel_picker {
                    let is_real_channel = p.cursor > 0
                        && p.items.get(p.cursor).map(|s| s.as_str()) != Some("new channel");
                    if is_real_channel {
                        if let Some(name) = p.keys.get(p.cursor).and_then(|k| k.clone()) {
                            self.begin_close_channel(&name, CloseKind::Picker);
                        }
                    }
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.picker_select(),
            _ if down => {
                if let Some(p) = &mut self.channel_picker {
                    p.cursor = step_cursor(true, p.cursor, p.items.len() - 1);
                }
            }
            _ if up => {
                if let Some(p) = &mut self.channel_picker {
                    p.cursor = step_cursor(false, p.cursor, p.items.len() - 1);
                }
            }
            _ => {}
        }
    }

    fn picker_select(&mut self) {
        let picker = self.channel_picker.take();
        let Some(picker) = picker else {
            return;
        };
        if picker.cursor == 0 {
            let s = self.active_mut();
            s.channel = None;
            apply_channel_permission_default(s);
            return;
        }
        // „new channel" → Channel Builder öffnen.
        if picker.items.get(picker.cursor).map(|s| s.as_str()) == Some("new channel") {
            self.open_channel_builder();
            return;
        }
        let key = match picker.keys.get(picker.cursor).and_then(|k| k.as_deref()) {
            Some(k) => k.to_string(),
            None => return,
        };
        match self.channels.get(&key) {
            Some(ch) => {
                Self::warmup_channel(&ch);
                self.bind_active_channel(key, ch);
            }
            None => {
                let s = self.active_mut();
                s.error = Some(format!("Channel \"{key}\" is not available."));
                s.error_debug = None;
            }
        }
    }

    /// Bindet den angegebenen, bereits registrierten Kanal an die aktive
    /// Session (setzt `channel`, räumt etwaige Fehler aus dem vorherigen
    /// Versuch auf und wendet die kanalabhängige Berechtigungs-Default an,
    /// falls der User sie nicht selbst umgestellt hat).
    pub(crate) fn bind_active_channel(
        &mut self,
        _name: String,
        ch: Arc<dyn crate::channel::Channel>,
    ) {
        let s = self.active_mut();
        s.channel = Some(ch);
        s.error = None;
        s.error_debug = None;
        apply_channel_permission_default(s);
    }
}
