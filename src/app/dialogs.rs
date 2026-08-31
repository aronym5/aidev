use super::*;
use crossterm::event::{self, KeyCode};

impl App {
    pub(crate) fn handle_pre_send_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.pre_send_confirm.take() else {
            return;
        };
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            KeyCode::Esc => {
                // Abbrechen: keine Wahl merken, nichts abschicken.
            }
            _ if down => {
                d.cursor = step_cursor(true, d.cursor, 2);
                self.pre_send_confirm = Some(d);
            }
            _ if up => {
                d.cursor = step_cursor(false, d.cursor, 2);
                self.pre_send_confirm = Some(d);
            }
            KeyCode::Enter | KeyCode::Char(' ') => match d.cursor {
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
            _ => self.pre_send_confirm = Some(d),
        }
    }

    pub(crate) fn handle_exec_confirm_key(&mut self, key: event::KeyEvent) {
        let Some(mut d) = self.exec_confirm.take() else {
            return;
        };
        let down = key.code == KeyCode::Down || key.code == KeyCode::Char('j');
        let up = key.code == KeyCode::Up || key.code == KeyCode::Char('k');
        match key.code {
            // Esc = ablehnen (der Worker erhält `false` und blockiert nicht).
            KeyCode::Esc => {
                let _ = d.reply.send(false);
            }
            _ if down => {
                d.cursor = step_cursor(true, d.cursor, 1);
                self.exec_confirm = Some(d);
            }
            _ if up => {
                d.cursor = step_cursor(false, d.cursor, 1);
                self.exec_confirm = Some(d);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let _ = d.reply.send(d.cursor == 0);
            }
            _ => self.exec_confirm = Some(d),
        }
    }
}
