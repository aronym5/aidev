use super::*;
use crate::llm;
use crate::llm::WorkerEvent;
use std::time::Instant;

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
                WorkerEvent::RoundStart(id, at) => {
                    // Eine neue HTTP-Runde der Antwort wird abgesendet: Die
                    // Streaming-Metrik-Felder zurücksetzen, damit die Statusleiste
                    // wieder „thinking…“ hochzählt (bis FirstToken eintrifft).
                    if let Some(s) = self.session_mut(id) {
                        s.reset_stream_metrics(at);
                    }
                }
                WorkerEvent::FirstToken(id, ttft_ms) => {
                    // Erstes Inhalt-Byte (Reasoning/Tool-Call/Content) da: gemessene
                    // TTFT übernehmen und den TPS-Nenner (UI-Uhr) starten.
                    if let Some(s) = self.session_mut(id) {
                        s.ttft_ms = Some(ttft_ms);
                        s.first_token_at = Some(Instant::now());
                    }
                }
                WorkerEvent::StreamProgress(id, tokens, _stream_ms) => {
                    // Live-Tokenstand der laufenden Runde für die TPS-Anzeige.
                    if let Some(s) = self.session_mut(id) {
                        s.stream_tokens = tokens;
                    }
                }
                WorkerEvent::RoundMetrics(id, metrics) => {
                    // Abgeschlossene Runde: Metriken parken, bis die Runde über
                    // `finish_assistant` finalisiert wird (dann ans Event).
                    if let Some(s) = self.session_mut(id) {
                        s.pending_metrics = Some(metrics);
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
                        // wird stattdessen per `RoundEnd` (explizit vom Worker
                        // nach dem letzten Tool) geschlossen; die Lazy-Abschlüsse
                        // bei `Chunk`/`Reasoning`/`Usage`/`Done`/`Cancelled`
                        // bleiben nur als Fallback.
                    }
                }
                WorkerEvent::RoundEnd(id) => {
                    // Explizites Rundenende vom Worker (er kennt `tools.len()`):
                    // alle Tools der Runde sind beendet, die geparkte
                    // `pending_usage` gehört genau zu dieser Runde. Sofort
                    // abschließen, damit `reported_usage` schon während der
                    // Folge-Anfrage auf dem Event steht – statt erst beim
                    // ersten Chunk danach. Ohne offene Tool-Runde ein No-Op
                    // (idempotent, z. B. nach Cancel/Error doppelt).
                    if let Some(s) = self.session_mut(id) {
                        if s.is_open_tool_round_done() {
                            s.finish_assistant(false, None);
                        }
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
                            nav: ListNav::new(2), // „Yes, run" | „No, decline"
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
                    // 1) Session finalisieren (Assistant-Runde, Phase → Idle).
                    // 2) Automatisch nach der finalen Antwort prüfen, ob der
                    //    Kontext kompaktiert werden sollte – die Kompaktierung
                    //    läuft dann in einem eigenen Thread PARALLEL zur
                    //    Eingabe des nächsten Prompts. Das `usage` des soeben
                    //    beendeten Turns ist jetzt verbindlich (finish_assistant
                    //    hat `context_len` beglichen) – die Entscheidung ist
                    //    also genauso genau wie die bisherige Sendzeit-Prüfung.
                    // `s.compacting` schützt vor einer bereits laufenden
                    // Kompaktierung (kein Doppel-Start).
                    let already_compacting = {
                        let Some(s) = self.session_mut(id) else {
                            continue;
                        };
                        s.retrying = None;
                        s.finish_assistant(false, None);
                        s.phase = Phase::Idle;
                        s.compacting
                    };
                    if !already_compacting {
                        self.auto_compact_after_turn(id);
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
                WorkerEvent::Compacted(id, content, tokens, keep, log_path) => {
                    if let Some(s) = self.session_mut(id) {
                        s.apply_compaction(content, tokens, keep, log_path);
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
                            if !b.tunnels.items.iter().any(|t| {
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
                        if b.worktrees.items.is_empty() && !worktrees.is_empty() {
                            b.worktrees = Selection::wrap_at(worktrees, 0);
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
                WorkerEvent::ModelProbe {
                    model,
                    protocol,
                    ok,
                } => {
                    // Probe-Ergebnis eines Modell-Tests verbuchen – treibt den
                    // grün/gelb/rot-Status im Modell-Picker.
                    self.model_registry.record_probe(&model, protocol, ok);
                }
                WorkerEvent::ModelsRefreshed(fetched_ids_vec) => {
                    // Registry aktualisieren (Status setzen/Deduplizierung).
                    let before = self.model_registry.len();
                    self.model_registry.apply_refresh(&fetched_ids_vec);
                    let added = self.model_registry.len().saturating_sub(before);

                    // Picker-Liste aktualisieren (wenn offen).
                    // Komplette Liste aus der Registry neu aufbauen, um
                    // Reihenfolge und Display konsistent zu halten – vor dem
                    // mutablen Borrow auf `model_picker` bauen.
                    let list = self.model_pick_list();
                    if let Some(p) = &mut self.model_picker {
                        // `set_items` erhält den Cursor und klemmt ihn auf die
                        // neue Länge (bzw. auf „(Standard)", falls nötig).
                        p.items.set_items(list);
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

    /// Startet die automatische Kompaktierung direkt nach dem Abschluss einer
    /// finalen Antwort (Auslöser: Auto). Der Kompaktierungs-Thread läuft in
    /// einem eigenen Thread und meldet sich über `Compacting`/`Compacted` –
    /// der User kann parallel zum Tippen des nächsten Prompts weitermachen.
    ///
    /// Wird nur gestartet, wenn `should_compact` greift (Kontext am
    /// `compact_at`-Anteil des Kontextfensters UND genug alte Turns) und nicht
    /// schon eine Kompaktierung läuft.
    fn auto_compact_after_turn(&mut self, id: usize) {
        // `resolve_endpoint` braucht den Session-Index (die Event-ID ist die
        // interne, fortlaufende `Session::id`).
        let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        // Config/Endpoint VOR dem mutablen Zugriff auf die Session.
        let cfg = self.config.clone();
        let ep = match self.resolve_endpoint(idx) {
            Ok(ep) => ep,
            Err(_) => return, // ohne Endpunkt keine automatische Kompaktierung
        };
        let (current, messages, cancel) = {
            let s = &self.sessions[idx];
            if s.compacting || !should_compact(s, &cfg, &ep) {
                return;
            }
            (
                prompt_tokens(s),
                crate::chat::api_messages(&s.chat),
                s.cancel.clone(),
            )
        };
        // Kein echter Schnitt möglich? (z. B. Konversation besteht nur aus dem
        // einen Query, oder unmittelbar vor dem letzten Query wurde bereits
        // kompaktiert → Projektion hat < 3 user-Nachrichten). Dann starten wir
        // gar keinen Hintergrund-Thread – sonst gäbe es nur die rote Meldung
        // „keine zu kompaktierenden Turns“, obwohl nichts Schlimmes passiert.
        if !llm::can_compact(&messages) {
            return; // `s.compacting` bleibt false (wurde noch nicht gesetzt)
        }
        // Als laufend markieren – schließt das Rennen zu `send_prompt`, das die
        // Sendzeit-Fallback-Kompaktierung über `s.compacting` unterdrückt.
        self.sessions[idx].compacting = true;
        llm::spawn_compact(
            self.tx.clone(),
            id,
            cfg,
            ep,
            messages,
            cancel,
            llm::CompactTrigger::AutoTurn,
            Some(current),
        );
    }
}
