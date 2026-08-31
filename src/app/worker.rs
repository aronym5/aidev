use super::*;
use crate::llm::WorkerEvent;

impl App {
    pub(crate) fn drain_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                WorkerEvent::Chunk(id, chunk) => {
                    if let Some(s) = self.session_mut(id) {
                        s.retrying = None;
                        // Beginnt eine NEUE Runde (finale Antwort nach einer
                        // Tool-Runde), wird die abgeschlossene Tool-Runde zuerst
                        // geschlossen – sonst hingen ihre Tokens/Runden falsch.
                        if s.is_open_tool_round_done() {
                            s.finish_assistant(false, None);
                        }
                        // Erste Text-Runde des Turns (oder nach einer Tool-Runde)
                        // braucht ein offenes Assistant-Ziel.
                        s.ensure_open_assistant();
                        s.append_text(&chunk);
                    }
                }
                WorkerEvent::Reasoning(id, reasoning) => {
                    if let Some(s) = self.session_mut(id) {
                        s.retrying = None;
                        if s.is_open_tool_round_done() {
                            s.finish_assistant(false, None);
                        }
                        s.ensure_open_assistant();
                        s.append_reasoning(&reasoning);
                    }
                }
                WorkerEvent::Usage(id, usage, parts) => {
                    if let Some(s) = self.session_mut(id) {
                        // Beginnt nach einer abgeschlossenen Tool-Runde eine
                        // neue (z. B. Tool-Runde ohne Text/Reasoning), schließt
                        // das usage-EVENT die VORIGE Runde – deren
                        // `pending_usage`/`pending_parts` sind an dieser Stelle
                        // noch aktuell und werden erst danach überschrieben.
                        if s.is_open_tool_round_done() {
                            s.finish_assistant(false, None);
                        }
                        s.pending_usage = Some(usage);
                        s.pending_parts = Some(parts);
                        // Der abgeschlossene Usage-Wert steht für die Statusleiste
                        // sofort als serverbestätigt fest (bis zur nächsten Runde).
                        if let Some(u) = &s.pending_usage {
                            s.live_usage_total = Some(u.total_tokens);
                        }
                    }
                }
                WorkerEvent::UsageUpdate(id, total) => {
                    // MID-STREAM: Endpunkt hat gerade ein `usage`-Event geliefert.
                    // Die serverbestätigte `total_tokens` der laufenden Runde direkt
                    // als aktuellen Kontextstand für die Statusleiste speichern –
                    // präziser als die Zeichen-Heuristik aus dem Live-Tail.
                    if let Some(s) = self.session_mut(id) {
                        s.live_usage_total = Some(total);
                    }
                }
                WorkerEvent::HttpHeaders(id, headers) => {
                    // Response-Header der letzten erfolgreichen LLM-Antwort merken
                    // (für den `Alt+H`-Dialog).
                    if let Some(s) = self.session_mut(id) {
                        s.last_http_headers = Some(headers);
                    }
                }
                WorkerEvent::ToolStart {
                    session,
                    tool_call_id,
                    function_name,
                    arguments,
                    label,
                } => {
                    if let Some(s) = self.session_mut(session) {
                        s.retrying = None;
                        // Manuelles `/run` (Sentinel `tool_call_id == "user_run"`)
                        // bleibt im neuen Event-Layout parent-los – es gehört
                        // KEINEM Turn an und braucht keine Assistant-Sub-Runde
                        // (die frühere Phantom-Runde entfällt). LLM-Tools hängen
                        // als Kinder der (ggf. neu geöffneten) Sub-Runde.
                        let parent = if tool_call_id == "user_run" {
                            None
                        } else {
                            s.ensure_open_assistant();
                            s.open_assistant_id
                        };
                        // Platzhalter-Kind aus Name + Argumenten (noch ohne
                        // Ergebnisdaten); `ToolEnd` setzt über
                        // `tool_kind_from_activity` das finale Kind.
                        let kind = crate::app::session::tool_kind_from_activity(
                            &function_name,
                            &arguments,
                            &crate::llm::ToolActivity::default(),
                        );
                        let id = s.open_tool(
                            parent,
                            tool_call_id,
                            function_name,
                            arguments,
                            kind, // wird bei ToolEnd finalisiert
                        );
                        s.live_tool = Some(id);
                        s.active_tool_label = Some(label);
                    }
                }
                WorkerEvent::ToolOutput(id, chunk) => {
                    if let Some(s) = self.session_mut(id) {
                        if let Some(tid) = s.live_tool {
                            s.append_tool_output(tid, &chunk);
                        }
                    }
                }
                WorkerEvent::ToolEnd(id, activity) => {
                    if let Some(s) = self.session_mut(id) {
                        if let Some(tid) = s.live_tool.take() {
                            s.finalize_tool_event(tid, &activity);
                        }
                        // KEIN `finish_assistant` hier: Bei mehreren Tools einer
                        // Runde kommen ToolStart/ToolEnd verschachtelt
                        // (`Start→End→Start→End`). Ein Abschluss nach dem ERSTEN
                        // ToolEnd würde die Runde vorschnell schließen – die
                        // restlichen Tools hingen dann an einer frischen, falschen
                        // Sub-Runde und verlören ihre gemessenen Tokens. Die Runde
                        // wird stattdessen geschlossen, sobald die NÄCHSTE Runde
                        // beginnt (`Chunk`/`Reasoning`/`Usage`) bzw. bei
                        // `Done`/`Cancelled`.
                    }
                }
                WorkerEvent::ExecConfirm(id, label, command, reply) => {
                    // Nur für eine noch existierende Session anzeigen; sonst
                    // sofort ablehnen, damit der Worker nicht blockiert.
                    if self.sessions.iter().any(|s| s.id == id) {
                        // Eine bereits offene Einzel-Bestätigung wird abgelehnt
                        // (der zugehörige Worker bekommt `false` und läuft weiter).
                        if let Some(prev) = self.exec_confirm.take() {
                            let _ = prev.reply.send(false);
                        }
                        self.exec_confirm = Some(ExecConfirm {
                            label,
                            command,
                            cursor: 0,
                            reply: reply.0,
                        });
                    } else {
                        let _ = reply.0.send(false);
                    }
                }
                WorkerEvent::Retrying(id, summary, retry_at) => {
                    if let Some(s) = self.session_mut(id) {
                        s.retrying = Some((summary, retry_at));
                    }
                }
                WorkerEvent::Done(id) => {
                    if let Some(s) = self.session_mut(id) {
                        s.retrying = None;
                        // `pending_usage` ist genau dieser Turn (Usage kommt vor
                        // Done); `None` = kein Server-Usage → 0-Default.
                        s.finish_assistant(false, None);
                        s.phase = Phase::Idle;
                    }
                }
                WorkerEvent::Cancelled(id) => {
                    if let Some(s) = self.session_mut(id) {
                        s.retrying = None;
                        s.finish_assistant(true, None);
                        s.phase = Phase::Idle;
                    }
                }
                WorkerEvent::Error(id, err) => {
                    if let Some(s) = self.session_mut(id) {
                        // Offenen (angefangenen) Turn-Zustand abschließen, falls
                        // schon Inhalte geflossen sind.
                        if s.open_assistant_id.is_some() {
                            s.finish_assistant(false, None);
                        }
                        s.compacting = false;
                        s.retrying = None;
                        // Kurzfassung (einzeilig) und – falls der Worker ein
                        // Debug-Material ablegte – dessen Pfad (eigene Zeile,
                        // beginnt mit '/') voneinander trennen.
                        let (summary, debug_path) = match err.split_once('\n') {
                            Some((head, path)) if path.starts_with('/') => {
                                (head.to_string(), Some(path.to_string()))
                            }
                            _ => (err, None),
                        };
                        s.error = Some(summary);
                        s.error_debug = debug_path;
                        s.phase = Phase::Idle;
                    }
                }
                WorkerEvent::Compacting(id) => {
                    if let Some(s) = self.session_mut(id) {
                        s.compacting = true;
                    }
                }
                WorkerEvent::Compacted(id, content, tokens) => {
                    // `compact_keep_turns` VOR dem mutablen Borrow der Session
                    // lesen – `session_mut` leiht `self` mutabel aus, ein
                    // gleichzeitiger Zugriff auf `self.config` wäre E0503.
                    let keep = self.config.compact_keep_turns;
                    if let Some(s) = self.session_mut(id) {
                        s.apply_compaction(content, tokens, keep);
                    }
                }
                WorkerEvent::BuilderLoaded(images_with_wd, container_info, worktrees) => {
                    // Erstes Laden der Image-Liste erkennen (danach liefern
                    // weitere BuilderLoaded-Events nur noch Container-Status)
                    let mut first_image_load = false;
                    if let Some(b) = &mut self.channel_builder {
                        first_image_load = !b.images_loaded;
                        // Images in Tunnel-Liste einfügen (local bleibt auf Index 0)
                        for (img, wd) in images_with_wd {
                            if !b.tunnels.iter().any(|t| {
                                t.image_name().is_some_and(|n| {
                                    crate::channel::builder::image_names_equal(n, &img)
                                })
                            }) {
                                b.tunnels.push(crate::channel::builder::Tunnel::Image {
                                    name: img,
                                    working_dir: wd,
                                });
                            }
                        }
                        b.container_info = container_info;
                        // Worktrees nur übernehmen, wenn noch kein eigener Inhalt
                        if b.worktrees.is_empty() && !worktrees.is_empty() {
                            b.worktrees = worktrees;
                        }
                        b.images_loaded = true;
                    }
                    // Nach dem asynchronen Update der Image-Liste erneut
                    // prüfen, ob die aktuelle Tunnel-Auswahl angepasst werden
                    // sollte – z. B. ist das laut Config für das gewählte
                    // Repo-Dir vorgesehene Default-Image jetzt erst verfügbar.
                    // Nur beim ersten Laden, damit eine manuelle Auswahl des
                    // Users durch spätere Container-Status-Events unberührt
                    // bleibt.
                    if first_image_load && self.apply_builder_default_image() {
                        if let Some(b) = &mut self.channel_builder {
                            b.container_info = None; // nach Tunnel-Wechsel veraltet
                        }
                        self.update_builder_container();
                    }
                }
                WorkerEvent::ModelsRefreshed(fetched_ids_vec) => {
                    // Registry aktualisieren (Status setzen/Deduplizierung).
                    let before = self.model_registry.len();
                    self.model_registry.apply_refresh(&fetched_ids_vec);
                    let added = self.model_registry.len().saturating_sub(before);

                    // Picker-Liste aktualisieren (wenn offen).
                    if let Some(p) = &mut self.model_picker {
                        // Komplette Liste aus der Registry neu aufbauen,
                        // um Reihenfolge und Display konsistent zu halten.
                        p.items.clear();
                        for entry in self.model_registry.all() {
                            p.items.push((entry.key(), entry.display_full()));
                        }
                        let default_id = self.config.model.clone();
                        let default_key = self
                            .model_registry
                            .find_by_model_id(&default_id)
                            .map(|e| e.key())
                            .unwrap_or(default_id);
                        p.show_default = !p.items.iter().any(|(k, _)| *k == default_key);
                        // Cursor sichern (nicht über Listenende hinaus).
                        let max = if p.show_default {
                            p.items.len()
                        } else {
                            p.items.len().saturating_sub(1)
                        };
                        p.cursor = p.cursor.min(max);
                        p.loading = false;
                    }

                    if added > 0 {
                        let s = self.active_mut();
                        s.error = Some(format!(
                            "Refreshed: {added} new model{} found.",
                            if added == 1 { "" } else { "s" }
                        ));
                        s.error_debug = None;
                    }
                }
            }
        }
        changed
    }
}
