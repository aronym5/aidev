//! Worker-Thread: spawn_worker, spawn_user_run, LiveSink.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;

use super::compact::{compact_chat_messages, compact_wire_messages, looks_like_context_error};
use super::http::{request_once, shared_client};
use super::tools_def::{Step, ToolInvocation};
use super::tools_exec::{run_command_display, run_tool_live, tool_activity, tool_label, ToolOut};
use super::wire::WireMessage;
use super::{ExecConfirmReply, WorkerEvent};
use crate::app::LocalExecMode;
use crate::channel::{Channel, ChannelKind};
use crate::config::{Config, ResolvedEndpoint};
use crate::perm::Permission;
// helpers used via super::http and super::compact

/// Liegt ein Kanal vor, darf das Modell Werkzeuge über `channel` ausführen –
/// aber nur die, die `permission` freigibt (Filter der Tool-Definitionen UND
/// Berechtigungsprüfung vor jeder Ausführung). Der Loop läuft bis zur finalen
/// Antwort. Das Runden-Limit
/// (`config.max_tool_rounds`) schützt davor, dass das Modell unendlich Tools
/// aufruft. Wird es erreicht, gibt es KEINEN harten Abbruch: Das Modell
/// bekommt eine letzte Runde OHNE Werkzeuge mit der verbalen Aufforderung,
/// die Antwort jetzt anhand der bisherigen Ergebnisse abzuschließen.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker(
    tx: Sender<WorkerEvent>,
    session: usize,
    config: Config,
    ep: ResolvedEndpoint,
    messages: Vec<WireMessage>,
    cancel: Arc<AtomicBool>,
    channel: Option<Arc<dyn Channel>>,
    permission: Permission,
    compact: bool,
    local_exec_mode: LocalExecMode,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let client = shared_client();
        let max_rounds = config.max_tool_rounds as usize;

        // Wire-Format ist bereits die Chat-Projektion (`api_messages(&chat)`).
        let mut msgs = messages;
        let mut with_tools = channel.is_some();
        // Reaktive Kompaktierung (bei context_length-Fehler) nur EINMAL pro Turn.
        let mut reactive_compacted = false;

        // Proaktive Kompaktierung: alte Nachrichten durch eine Zusammenfassung
        // ersetzen, BEVOR der eigentliche Turn startet. Schlägt sie fehl (z. B.
        // Historie zu kurz oder Endpunkt ohne max_tokens), wird die
        // Original-Historie gesendet – kein harter Abbruch.
        if compact {
            let _ = tx.send(WorkerEvent::Compacting(session));
            match compact_chat_messages(client, &config, &ep, &msgs, &cancel) {
                Ok((repl, content, tokens)) => {
                    let _ = tx.send(WorkerEvent::Compacted(session, content, tokens));
                    msgs = repl;
                }
                Err(_) => {
                    // Ohne Kompaktierung weitermachen; Done/Cancelled/Error
                    // setzen den UI-Kompaktierungs-Status zurück.
                }
            }
        }

        for _ in 0..max_rounds {
            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerEvent::Cancelled(session));
                return;
            }
            // Request-Runde starten. `with_tools` schaltet um, wenn der Endpunkt
            // keine Werkzeug-Unterstützung meldet.
            let (step, supported) = request_once(
                &tx, session, client, &ep, &msgs, &cancel, with_tools, permission,
            );
            // Endpunkt ohne Werkzeug-Unterstützung? Dann ohne Tools weiter.
            with_tools = with_tools && supported;

            match step {
                Step::Final { usage, parts } => {
                    // Usage dieser Runde ans Event-Log geben (`PendingUsage`).
                    if let Some(u) = usage {
                        let _ = tx.send(WorkerEvent::Usage(session, u, parts.clone()));
                    }
                    let _ = tx.send(WorkerEvent::Done(session));
                    return;
                }
                Step::Cancelled => {
                    let _ = tx.send(WorkerEvent::Cancelled(session));
                    return;
                }
                Step::Err(err) => {
                    // Kontextfenster überlaufen? Einmalig komprimieren und die
                    // Anfrage erneut versuchen (reaktive Kompaktierung).
                    if !reactive_compacted && config.compact_auto && looks_like_context_error(&err)
                    {
                        if let Ok(new_msgs) =
                            compact_wire_messages(client, &config, &ep, &msgs, &cancel)
                        {
                            msgs = new_msgs;
                            reactive_compacted = true;
                            continue;
                        }
                    }
                    let _ = tx.send(WorkerEvent::Error(session, err));
                    return;
                }
                Step::Tools {
                    assistant,
                    tools,
                    usage,
                    parts,
                } => {
                    // Usage dieser Runde ebenfalls als `Usage`-Event weitergeben
                    // (nicht nur bei `Step::Final`): Die abgeschlossene
                    // Assistant-Runde mit tool_calls leitet daraus ihre
                    // `num_tokens_text`/`num_tokens_reasoning` ab – sonst stünden
                    // in der Übersicht bei Zwischenantworten 0 Tokens, obwohl der
                    // Server für die Runde Usage geliefert hat.
                    if let Some(u) = usage {
                        let _ = tx.send(WorkerEvent::Usage(session, u, parts.clone()));
                    }
                    // Die Runde wird zuerst in einem Puffer gesammelt und erst
                    // NACH Abschluss verbindlich übernommen. So persistiert ein
                    // mitten in der Ausführung abgebrochener/fehlgeschlagener
                    // Durchlauf keine HALBE Runde (assistant-mit-tool_calls
                    // ohne zugehörige tool-Antworten) – solche Runden würden
                    // beim nächsten Turn als kaputte Historie an den Endpunkt
                    // gehen und dort rätselhafte 400er auslösen.
                    let mut round = vec![assistant];
                    for t in tools {
                        if cancel.load(Ordering::Relaxed) {
                            let _ = tx.send(WorkerEvent::Cancelled(session));
                            return;
                        }
                        let label = tool_label(&t);
                        let _ = tx.send(WorkerEvent::ToolStart {
                            session,
                            tool_call_id: t.id.clone(),
                            function_name: t.name.clone(),
                            arguments: t.arguments.clone(),
                            label: label.clone(),
                        });
                        // Berechtigungsprüfung VOR der Auswertung: ein über die
                        // gefilterten Definitionen hinaus aufgerufenes Werkzeug
                        // wird abgewiesen, ohne den Kanal zu berühren.
                        let allowed = permission.allows(&t.name);
                        // Local-Kanal + `run` im ConfirmEach-Modus: vor der
                        // Ausführung einzeln beim User nachfragen. Der Worker
                        // blockiert, bis die UI über den Reply-Kanal antwortet
                        // (oder der Sender weggefallen ist → ablehnen).
                        let confirmed = if allowed {
                            match channel.as_deref() {
                                Some(ch)
                                    if t.name == "run"
                                        && ch.kind() == ChannelKind::Local
                                        && local_exec_mode == LocalExecMode::ConfirmEach =>
                                {
                                    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                                    let _ = tx.send(WorkerEvent::ExecConfirm(
                                        session,
                                        label.clone(),
                                        run_command_display(&t),
                                        ExecConfirmReply(reply_tx),
                                    ));
                                    reply_rx.recv().unwrap_or(false)
                                }
                                _ => true,
                            }
                        } else {
                            true
                        };
                        let result = if !allowed {
                            ToolOut {
                                text: format!(
                                    "ERROR: tool \"{}\" requires permission \
                                     \"{}\", only \"{}\" is allowed ({}).",
                                    t.name,
                                    Permission::required_for(&t.name).label(),
                                    permission.label(),
                                    permission.description()
                                ),
                                ..Default::default()
                            }
                        } else if !confirmed {
                            ToolOut {
                                text: format!(
                                    "REJECTED: The command \"{}\" was not approved \
                                     by the user - not executed.",
                                    run_command_display(&t)
                                ),
                                ..Default::default()
                            }
                        } else {
                            match channel.as_deref() {
                                Some(ch) => {
                                    let mut sink = LiveSink::new(tx.clone(), session);
                                    let out = run_tool_live(
                                        &t.name,
                                        &t.arguments,
                                        ch,
                                        &mut |s| sink.push(s),
                                        Some(&cancel),
                                    );
                                    sink.flush();
                                    out
                                }
                                None => ToolOut {
                                    text: "ERROR: no channel bound".to_string(),
                                    ..Default::default()
                                },
                            }
                        };
                        let _ = tx.send(WorkerEvent::ToolEnd(
                            session,
                            tool_activity(&result),
                        ));
                        round.push(WireMessage {
                            role: "tool".into(),
                            content: Some(result.text),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: Some(t.id),
                        });
                    }
                    msgs.extend(round);
                }
            }
        }

        // Runden-Limit erreicht: Statt hart abzubrechen, wird das Modell
        // VERBAL und strukturell zum Abschluss gezwungen. Werkzeuge sind für
        // diese letzte Runde deaktiviert, daher kann es nicht weiter Tools
        // aufrufen, sondern muss die Nutzerfrage anhand der bisherigen
        // Werkzeug-Ergebnisse beantworten.
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(WorkerEvent::Cancelled(session));
            return;
        }
        msgs.push(WireMessage {
            role: "system".into(),
            content: Some(format!(
                "You have reached the limit of {max_rounds} tool rounds. \
                 Answer the user's question now, using the tool results so far. \
                 No further tool calls are possible."
            )),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
        let (step, _) = request_once(&tx, session, client, &ep, &msgs, &cancel, false, permission);
        match step {
            Step::Final { usage, parts } => {
                if let Some(u) = usage {
                    let _ = tx.send(WorkerEvent::Usage(session, u, parts.clone()));
                }
                let _ = tx.send(WorkerEvent::Done(session));
            }
            Step::Cancelled => {
                let _ = tx.send(WorkerEvent::Cancelled(session));
            }
            Step::Err(err) => {
                let _ = tx.send(WorkerEvent::Error(session, err));
            }
            Step::Tools { .. } => {
                let _ = tx.send(WorkerEvent::Error(
                    session,
                    format!(
                        "Das Modell hat das Werkzeug-Runden-Limit ({max_rounds}) erreicht \
                         und danach keine abschließende Antwort geliefert – abgebrochen."
                    ),
                ));
            }
        }
    })
}

/// Startet den Worker für einen **User-induzierten Tool-Call** (`/run …`):
/// der Ausdruck geht NICHT an das LLM, sondern wird von der Shell des Kanals
/// verarbeitet (`sh -c <expr>` – Pipes/Umleitungen/Verkettungen möglich).
/// Ausgabe/Exit-Code werden wie bei einer Modell-Tool-Runde gemeldet
/// (`ToolStart` → `ToolEnd` → `Done`); die UI hält daraus ein parent-loses
/// Tool-Event (eigene Timeline-Zeile, nicht Teil eines LLM-Turns).
pub(crate) fn spawn_user_run(
    tx: Sender<WorkerEvent>,
    session: usize,
    channel: Arc<dyn Channel>,
    expr: String,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if cancel.load(Ordering::Relaxed) {
            let _ = tx.send(WorkerEvent::Cancelled(session));
            return;
        }
        let inv = ToolInvocation {
            id: "user_run".to_string(),
            name: "run".to_string(),
            // Der manuelle `/run`-Weg liefert den kompletten Shell-Ausdruck als
            // `command` – identisch zum Modell-Tool, daher läuft er über dieselbe
            // Shell-Auswertung (`shell -c`).
            arguments: json!({ "command": expr }).to_string(),
        };
        let label = tool_label(&inv);
        let _ = tx.send(WorkerEvent::ToolStart {
            session,
            tool_call_id: inv.id.clone(),
            function_name: inv.name.clone(),
            arguments: inv.arguments.clone(),
            label: label.clone(),
        });
        // Live-Ausgabe des laufenden Befehls gebündelt an die UI streamen.
        let mut sink = LiveSink::new(tx.clone(), session);
        let result = run_tool_live(
            &inv.name,
            &inv.arguments,
            channel.as_ref(),
            &mut |s| sink.push(s),
            Some(&cancel),
        );
        sink.flush();
        let _ = tx.send(WorkerEvent::ToolEnd(
            session,
            tool_activity(&result),
        ));
        let _ = tx.send(WorkerEvent::Done(session));
    })
}

/// Bündelt die Live-Ausgabe eines laufenden `run`-Werkzeugs zu
/// `WorkerEvent::ToolOutput`-Events: angesammelte Abschnitte werden höchstens
/// alle ~100 ms an die UI gesendet, damit die Konsolen-Box flüssig fortschreibt,
/// ohne den Event-Kanal zu fluten. `flush()` schickt den Rest nach Abschluss.
///
/// Ein leichter Hintergrund-Timer leert den Puffer zusätzlich alle ~100 ms,
/// *auch ohne neuen Output* – so erreicht eine noch unvollendete Zeile (ohne
/// abschließendes `\n`) die UI während des Laufs sichtbar, genau wie auf der
/// cmdline, statt erst beim nächsten Zeilenumbruch oder am Ende aufzutauchen.
pub(crate) struct LiveSink {
    tx: Sender<WorkerEvent>,
    session: usize,
    pending: Arc<Mutex<String>>,
    last: Instant,
    /// Timer-Stopp-Signal + Faden-Handle (beim Drop gestoppt und gejoint).
    stop: Arc<AtomicBool>,
    timer: Option<JoinHandle<()>>,
}

impl LiveSink {
    pub(crate) fn new(tx: Sender<WorkerEvent>, session: usize) -> Self {
        let pending = Arc::new(Mutex::new(String::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let timer = {
            let tx = tx.clone();
            let pending = pending.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                // Alle ~100 ms den Puffer leeren – auch bei stiller Ausgabe,
                // damit die gerade unvollendete Zeile zeitnah sichtbar wird.
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    let chunk = pending
                        .lock()
                        .map(|mut g| std::mem::take(&mut *g))
                        .unwrap_or_default();
                    if !chunk.is_empty() {
                        let _ = tx.send(WorkerEvent::ToolOutput(session, chunk));
                    }
                }
            })
        };
        LiveSink {
            tx,
            session,
            pending,
            last: Instant::now(),
            stop,
            timer: Some(timer),
        }
    }

    pub(crate) fn push(&mut self, snippet: &str) {
        if let Ok(mut g) = self.pending.lock() {
            g.push_str(snippet);
        }
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.flush();
        }
    }

    /// Sendet den angesammelten Rest und setzt den Takt zurück.
    pub(crate) fn flush(&mut self) {
        let chunk = self
            .pending
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        self.last = Instant::now();
        if !chunk.is_empty() {
            let _ = self.tx.send(WorkerEvent::ToolOutput(self.session, chunk));
        }
    }
}

/// Beendet den Puffer-Timer sauber, damit nach dem Sink keine weiteren
/// `ToolOutput`-Events mehr eintreffen können (Reihenfolge vor `ToolEnd` bleibt
/// garantiert).
impl Drop for LiveSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.timer.take() {
            let _ = handle.join();
        }
    }
}
