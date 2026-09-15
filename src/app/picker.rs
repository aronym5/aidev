use super::*;
use crossterm::event::{self, KeyCode};
use std::sync::Arc;

impl App {
    pub(crate) fn open_channel_picker(&mut self) {
        // Alle registrierten Kanäle – alphabetisch sortiert.
        let mut names: Vec<String> = self.channels.names();
        names.sort();

        let mut items = vec![ChannelPick::NoChannel];
        for name in names {
            items.push(ChannelPick::Channel { name });
        }
        // Am Ende: „new channel" – Öffnet den Channel Builder.
        items.push(ChannelPick::NewChannel);

        // Cursor auf den gebundenen Kanal der aktiven Session legen (oder 0).
        let active_name = self.sessions[self.active]
            .channel
            .as_ref()
            .and_then(|ch| self.channels.find_name(ch));
        let cursor = match &active_name {
            Some(name) => items
                .iter()
                .position(|it| matches!(it, ChannelPick::Channel { name: n } if n == name))
                .unwrap_or(0),
            None => 0,
        };
        self.channel_picker = Some(ChannelPicker {
            items: Selection::new_at(items, cursor),
        });
    }

    pub(crate) fn handle_picker_key(&mut self, key: event::KeyEvent) {
        match key.code {
            KeyCode::Esc => self.channel_picker = None,
            KeyCode::Delete | KeyCode::Backspace => {
                // Gewählten Kanal aus der Registry entfernen (Entf). Nur bei
                // bestehenden Kanälen – „(kein Kanal)" und „new channel"
                // lassen sich nicht schließen.
                let name = self
                    .channel_picker
                    .as_mut()
                    .and_then(|p| match p.items.selected() {
                        Some(ChannelPick::Channel { name }) => Some(name.clone()),
                        _ => None,
                    });
                if let Some(name) = name {
                    self.begin_close_channel(&name, CloseKind::Picker);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.picker_select(),
            _ => {
                if let Some(p) = &mut self.channel_picker {
                    let viewport = p.items.nav.len() as u16;
                    p.items.handle_move(&key, viewport);
                }
            }
        }
    }

    fn picker_select(&mut self) {
        let picker = self.channel_picker.take();
        let Some(picker) = picker else {
            return;
        };
        match picker.items.selected() {
            Some(ChannelPick::NoChannel) => {
                let s = self.active_mut();
                s.set_channel(None);
                apply_channel_permission_default(s);
            }
            // „new channel" → Channel Builder öffnen.
            Some(ChannelPick::NewChannel) => self.open_channel_builder(),
            Some(ChannelPick::Channel { name }) => {
                let ch = self.channels.get(name);
                match ch {
                    Some(ch) => {
                        Self::warmup_channel(&ch);
                        self.bind_active_channel(name.clone(), ch);
                    }
                    None => {
                        let s = self.active_mut();
                        s.error = Some(format!("Channel \"{name}\" is not available."));
                        s.error_debug = None;
                    }
                }
            }
            None => {}
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
        s.set_channel(Some(ch));
        s.error = None;
        s.error_debug = None;
        apply_channel_permission_default(s);
    }
}
