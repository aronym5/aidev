use super::*;
use crossterm::event::{self, KeyCode};

impl App {
    pub(crate) fn handle_pre_send_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.pre_send_confirm.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                // Abbrechen: keine Wahl merken, nichts abschicken.
            }
            KeyCode::Enter | KeyCode::Char(' ') => match d.nav.cursor() {
                0 => {
                    // Verantwortung übernehmen – für den Rest des Laufs merken.
                    self.local_exec_mode = LocalExecMode::Trusted;
                    self.send_prompt(d.session);
                }
                1 => {
                    // Abschicken, aber jeden exec einzeln bestätigen.
                    self.local_exec_mode = LocalExecMode::ConfirmEach;
                    self.send_prompt(d.session);
                }
                _ => {
                    // Default: Absenden abbrechen, zurück zum Prompt.
                }
            },
            _ => {
                // Gemeinsame Bewegung über die ListNav-Abstraktion.
                d.nav.handle_move(&key, 3, |_| 1);
                self.pre_send_confirm = Some(d);
            }
        }
    }

    pub(crate) fn handle_exec_confirm_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.exec_confirm.take() else {
            return;
        };
        match key.code {
            // Esc = ablehnen (der Worker erhält `false` und blockiert nicht).
            KeyCode::Esc => {
                let _ = d.reply.send(false);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let _ = d.reply.send(d.nav.cursor() == 0);
            }
            _ => {
                // Gemeinsame Bewegung über die ListNav-Abstraktion.
                d.nav.handle_move(&key, 2, |_| 1);
                self.exec_confirm = Some(d);
            }
        }
    }
}
