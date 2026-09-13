//! Event-Log-Datenlayout für den Chatverlauf.
//!
//! Die Historie einer Session ist ein einziges, chronologisches **Event-Archiv**
//! mit offenen (wachsenden) Segmenten – die Quelle der Wahrheit für Historie,
//! API-Projektion und Rendering (ersetzt das alte `Vec<ChatMessage>`-Layout samt
//! Redundanz: content vs. Text-Ereignis, `tool_rounds` vs. `api_messages`-
//! Ableitung, doppelte Tool-Outputs).
//!
//! Grundprinzipien:
//! - `Chat` hält `events: HashMap<EventId, ChatEvent>` (O(1)-Zugriff, stabile
//!   Schlüssel über Kompaktierung hinweg) plus `order: Vec<EventId>` als
//!   primäre Chronologie (alle Events – auch offene/live – stehen darin).
//! - „Offen“ ist nur ein Zustand (`time_end == None`), keine eigene Zone.
//! - Projektionen (`api_messages`, `render_blocks`) werden **abgeleitet**,
//!   nichts wird projiziert gespeichert.
//! - Die Wire-Projektion `api_messages` speist `spawn_worker`/`spawn_compact`.

use std::collections::HashMap;
use std::time::Instant;

use crate::llm::{CompletionParts, RoundMetrics, Usage, WireMessage};
use crate::perm::Permission;

use crate::llm::{estimate_tokens, WireFunction, WireToolCall};

// ── Identität ─────────────────────────────────────────────────────────────

/// Technische, monoton wachsende ID für ALLE Events. u64 (kein String), damit
/// keine Kollision mit LLM-generierten `tool_call_id`s möglich ist und keine
/// Fremd-Werte als History-Key dienen.
pub type EventId = u64;

// ── Der Verlauf ───────────────────────────────────────────────────────────

/// Das chronologische Event-Archiv eines Chats.
#[derive(Debug, Clone, PartialEq)]
pub struct Chat {
    /// O(1)-Zugriff pro Event; stabile Keys trotz Kompaktierung.
    events: HashMap<EventId, ChatEvent>,
    /// Primäre Chronologie: ALLE Events (auch offene/live) in Entstehungs-
    /// reihenfolge. Iteration, Slicing und Kompaktierung laufen über diese Liste.
    order: Vec<EventId>,
    next_id: EventId,
}

impl Default for Chat {
    fn default() -> Self {
        Chat {
            events: HashMap::new(),
            order: Vec::new(),
            next_id: 1, // 0 = ungültige/leere ID, erste echte Event-ID ist 1
        }
    }
}

impl Chat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Öffnet einen User-Prompt (vollständig, sofort finalisiert). Existiert ein
    /// offener (offener/live) Turn, wird dieser zuvor geschlossen.
    pub fn push_user_prompt(
        &mut self,
        text: String,
        permission: Permission,
        model: String,
        num_tokens: u64,
        time_begin: Instant,
    ) -> EventId {
        let id = self.next_id;
        self.next_id += 1;
        // Sofortige Basis: bisherige Kontextlänge + Schätzung dieses Prompts;
        // wird von `settle_context_tail` überschrieben, sobald die Folge-Runde
        // bestätigt ist.
        let base = self
            .order
            .last()
            .and_then(|&pid| self.events.get(&pid))
            .and_then(|ev| ev.context_len)
            .unwrap_or(0);
        let estimate = base + estimate_tokens(&text);
        let ev = ChatEvent {
            previous_id: self.order.last().copied(),
            parent_id: None,
            time_begin: Some(time_begin),
            time_end: Some(time_begin), // unmittelbar abgeschlossen
            kind: EventKind::UserPrompt {
                text,
                permission,
                model,
                num_tokens,
            },
            context_len: Some(estimate),
        };
        self.events.insert(id, ev);
        self.order.push(id);
        id
    }

    /// Öffnet eine neue Assistant-Runde (eine HTTP-Unterrunde) – das Event ist
    /// zunächst OFFEN (`time_end == None`) und wächst per Frame. `parent_id` ist
    /// der zugehörige User-Prompt.
    pub fn open_assistant(
        &mut self,
        parent_id: Option<EventId>,
        reasoning: String,
        text: String,
        time_begin: Instant,
    ) -> EventId {
        let id = self.next_id;
        self.next_id += 1;
        let ev = ChatEvent {
            previous_id: self.order.last().copied(),
            parent_id,
            time_begin: Some(time_begin),
            time_end: None, // offen
            kind: EventKind::Assistant {
                reasoning,
                text,
                tool_event_ids: Vec::new(),
                num_tokens_reasoning: 0,
                num_tokens_text: 0,
                reported_usage: Usage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cached_tokens: None,
                },
                completion_parts: None,
                metrics: None,
            },
            context_len: None,
        };
        self.events.insert(id, ev);
        self.order.push(id);
        id
    }

    /// Öffnet ein Tool-Event als Kind einer Assistant-Runde (OFFEN bis Output
    /// abgeschlossen). `manual` (parent_id = None) = User `/run`.
    pub fn open_tool(
        &mut self,
        parent_id: Option<EventId>,
        tool_call_id: String,
        function_name: String,
        arguments: String,
        output: String,
        kind: ToolKind,
        time_begin: Instant,
    ) -> EventId {
        let id = self.next_id;
        self.next_id += 1;
        if let Some(pid) = parent_id {
            // Kind an die Assistant-Runde registrieren (Reihenfolge-Vertrag).
            if let Some(ev) = self.events.get_mut(&pid) {
                if let EventKind::Assistant { tool_event_ids, .. } = &mut ev.kind {
                    tool_event_ids.push(id);
                }
            }
        }
        let ev = ChatEvent {
            previous_id: self.order.last().copied(),
            parent_id,
            time_begin: Some(time_begin),
            time_end: None, // offen
            kind: EventKind::Tool {
                tool_call_id,
                function_name,
                arguments,
                output,
                kind,
                num_tokens_input: 0,
                num_tokens_output: 0,
            },
            context_len: None,
        };
        self.events.insert(id, ev);
        self.order.push(id);
        id
    }

    /// Liest ein Event (primär für Tests & Projektion).
    pub fn event(&self, id: EventId) -> Option<&ChatEvent> {
        self.events.get(&id)
    }

    /// Mutiert ein Event (z. B. Kompaktierungs-Shift auf `context_len`).
    pub(crate) fn event_mut(&mut self, id: EventId) -> Option<&mut ChatEvent> {
        self.events.get_mut(&id)
    }

    /// Alle Event-IDs in chronologischer Reihenfolge.
    pub fn order(&self) -> &[EventId] {
        &self.order
    }

    /// Primäre Chronologie als Iterator über die Events.
    pub fn iter(&self) -> impl Iterator<Item = &ChatEvent> {
        self.order.iter().filter_map(|id| self.events.get(id))
    }

    // ── Live-Mutation (Streaming) ──────────────────────────────────────────

    /// Hängt einen Reasoning-Chunk (des laufenden LLM) an ein OFFENES
    /// Assistant-Event an – es muss das offene sein.
    pub fn append_reasoning(&mut self, id: EventId, chunk: &str) {
        if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                if let EventKind::Assistant { reasoning, .. } = &mut ev.kind {
                    reasoning.push_str(chunk);
                }
            }
        }
    }

    /// Hängt einen Text-Chunk an ein OFFENES Assistant-Event an.
    pub fn append_text(&mut self, id: EventId, chunk: &str) {
        if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                if let EventKind::Assistant { text, .. } = &mut ev.kind {
                    text.push_str(chunk);
                }
            }
        }
    }

    /// Hängt einen Output-Chunk an ein OFFENES Tool-Event an (Live-Ausgabe).
    pub fn append_tool_output(&mut self, id: EventId, chunk: &str) {
        if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                if let EventKind::Tool { output, .. } = &mut ev.kind {
                    output.push_str(chunk);
                }
            }
        }
    }

    /// Schließt ein OFFENES Tool-Event ab (Zeitstempel + endgültiger Inhalt –
    /// der bereits via `append_tool_output` gesammelte Output wird belassen).
    pub fn finish_tool(&mut self, id: EventId, time_end: Instant) {
        if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                ev.time_end = Some(time_end);
            }
        }
    }

    /// Setzt den endgültigen Output (voller Ergebnistext), das UI-`ToolKind`
    /// und die num_tokens eines OFFENEN Tool-Events und schließt es. Existiert
    /// danach kein voller Output, wird "Execution aborted" als Platzhalter
    /// gesetzt (400er-Garantie: zu jedem tool_call ein Tool-Event).
    pub fn set_tool_final(
        &mut self,
        id: EventId,
        output: String,
        kind: ToolKind,
        num_tokens_input: u64,
        num_tokens_output: u64,
        time_end: Instant,
    ) {
        if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                ev.time_end = Some(time_end);
                if let EventKind::Tool {
                    output: o,
                    kind: k,
                    num_tokens_input: ni,
                    num_tokens_output: no,
                    ..
                } = &mut ev.kind
                {
                    *o = if output.is_empty() {
                        "Execution aborted".to_string()
                    } else {
                        output
                    };
                    *k = kind;
                    *ni = num_tokens_input;
                    *no = num_tokens_output;
                }
            }
        }
    }

    /// Schließt ein OFFENES Assistant-Event ab: setzt `time_end` und die
    /// abgeleiteten Token-Werte bzw. die Server-bestätigte Usage. Bei
    /// `aborted` wird zusätzlich ein `Abort`-Event direkt danach eingefügt.
    pub fn finalize_assistant(
        &mut self,
        id: EventId,
        time_end: Instant,
        reported_usage: Usage,
        num_tokens_reasoning: u64,
        num_tokens_text: u64,
        aborted: bool,
    ) {
        let closed = if let Some(ev) = self.events.get_mut(&id) {
            if ev.time_end.is_none() {
                ev.time_end = Some(time_end);
                if let EventKind::Assistant {
                    num_tokens_reasoning: nr,
                    num_tokens_text: nt,
                    reported_usage: ru,
                    ..
                } = &mut ev.kind
                {
                    *nr = num_tokens_reasoning;
                    *nt = num_tokens_text;
                    *ru = reported_usage;
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        if closed && aborted {
            push_control(self, EventKind::Abort, time_end);
        }
        // context_len des abgeschlossenen Turns begleichen (idempotent; läuft
        // sonst auch über `finish_assistant`): ab dem zugehörigen User-Prompt.
        if closed {
            let parent = self.events.get(&id).and_then(|e| e.parent_id);
            let start = parent
                .and_then(|pid| self.order.iter().position(|&i| i == pid))
                .unwrap_or(0);
            self.settle_context_tail(start);
        }
    }

    /// Schreibt die beim Streaming gemessenen Completion-Token je Bereich auf
    /// eine (abgeschlossene) Assistant-Runde – Quelle für
    /// `derive_last_turn_tokens`, die damit exakt statt proportional verteilt.
    pub fn set_completion_parts(&mut self, id: EventId, parts: Option<CompletionParts>) {
        if let Some(ev) = self.events.get_mut(&id) {
            if let EventKind::Assistant {
                completion_parts: cp,
                ..
            } = &mut ev.kind
            {
                *cp = parts;
            }
        }
    }

    /// Schreibt die beim Streaming gemessenen Metriken (TTFT, Stream-Zeit,
    /// Tokens) auf eine (abgeschlossene) Assistant-Runde – Quelle für die
    /// akkumulierte TTFT und die durchschnittliche TPS der Antwort-Fußzeile.
    pub fn set_round_metrics(&mut self, id: EventId, metrics: Option<RoundMetrics>) {
        if let Some(ev) = self.events.get_mut(&id) {
            if let EventKind::Assistant { metrics: m, .. } = &mut ev.kind {
                *m = metrics;
            }
        }
    }

    /// Kompaktierung: fügt genau an der `boundary`-Position ein `Archive`-Event
    /// in `order` ein und lässt alles davor (die bereits zusammengefassten
    /// Nachrichten) im Speicher/der UI erhalten. Gesendet wird davon nichts
    /// mehr: die Projektion `api_messages` startet bei der letzten Summary und
    /// reduziert so das Kontextfenster, ohne Inhalte zu löschen. Die
    /// `previous_id`-Verkettung wird auf die Summary umgebogen (primär bleibt
    /// aber `Chat.order`). Liefert die Anzahl der von der Summary abgedeckten
    /// Events (alles vor der boundary).
    pub fn compact(&mut self, boundary: usize, summary: String, num_tokens: u64) -> u64 {
        let arch = boundary.min(self.order.len());
        let id = self.next_id;
        self.next_id += 1;
        // Vorgänger der Summary = letzter abgedeckter Nachricht; der bisherige
        // Nachfolger an der boundary hängt fortan an der Summary.
        let prev = if arch > 0 {
            Some(self.order[arch - 1])
        } else {
            // Kein abgedeckter Inhalt – die Summary steht am Anfang.
            None
        };
        if let Some(next_id) = self.order.get(arch).copied() {
            if let Some(ev) = self.events.get_mut(&next_id) {
                ev.previous_id = Some(id);
            }
        }
        let ev = ChatEvent {
            previous_id: prev,
            parent_id: None,
            time_begin: Some(Instant::now()),
            time_end: Some(Instant::now()),
            kind: EventKind::Archive {
                summary,
                num_tokens,
                archived_events: arch,
            },
            // Die Summary ist der neue Projektions-Anker: ihre eigene Länge.
            context_len: Some(num_tokens),
        };
        self.events.insert(id, ev);
        self.order.insert(arch, id);
        arch as u64
    }

    /// Schreibt die abgeleiteten Tokens eines User-Prompts.
    pub fn set_user_prompt_tokens(&mut self, id: EventId, num_tokens: u64) {
        if let Some(ev) = self.events.get_mut(&id) {
            if let EventKind::UserPrompt { num_tokens: n, .. } = &mut ev.kind {
                *n = num_tokens;
            }
        }
    }

    /// Schreibt die abgeleiteten Tokens eines Assistant-Events.
    pub fn set_assistant_tokens(
        &mut self,
        id: EventId,
        num_tokens_reasoning: u64,
        num_tokens_text: u64,
    ) {
        if let Some(ev) = self.events.get_mut(&id) {
            if let EventKind::Assistant {
                num_tokens_reasoning: nr,
                num_tokens_text: nt,
                ..
            } = &mut ev.kind
            {
                *nr = num_tokens_reasoning;
                *nt = num_tokens_text;
            }
        }
    }

    /// Schreibt die abgeleiteten Tokens eines Tool-Events.
    pub fn set_tool_tokens(&mut self, id: EventId, num_tokens_input: u64, num_tokens_output: u64) {
        if let Some(ev) = self.events.get_mut(&id) {
            if let EventKind::Tool {
                num_tokens_input: ni,
                num_tokens_output: no,
                ..
            } = &mut ev.kind
            {
                *ni = num_tokens_input;
                *no = num_tokens_output;
            }
        }
    }

    /// Token-Ableitung nach Plan §4.3 für den aktuellen (letzten) Turn.
    ///
    /// Verwendet die vom Server reporteten `prompt_tokens`/`completion_tokens`
    /// der einzelnen Runden und verteilt sie auf die transportierten
    /// Bestandteile des Turns:
    ///   - `completion_tokens` einer Runde → reasoning, text und deren tool_calls
    ///     (proportional zur Länge des jeweils generierten Teils),
    ///   - der User-Prompt → `prompt_tokens` der ersten Runde des Turns minus
    ///     `total_tokens` der Vorgänger-Runde (nur wenn beide Usage vorliegen;
    ///     sonst bleibt die initiale Schätzung stehen),
    ///   - Tool-Antworten → prompt-Differenz der Folgerunde zum `total_tokens`
    ///     der Runde (die eigene Completion der Runde ist damit rausgerechnet),
    ///     nach Output-Länge auf die Tools der Runde verteilt.
    ///
    /// Ohne Usage (Server liefert nichts, erster Turn, manuelle `/run`-Tools)
    /// bleibt die Schätzung (4 Zeichen ≈ 1 Token) stehen. Idempotent: wird bei
    /// jedem `finish_assistant` erneut auf den letzten Turn angewendet und
    /// heilt sich so zu den finalen Werten, sobald alle Runden Usage tragen.
    pub fn derive_last_turn_tokens(&mut self) {
        // 1) Turn-Grenze: letzter UserPrompt in der Chronologie.
        let Some(upos) = self.order.iter().enumerate().rev().find_map(|(i, id)| {
            matches!(&self.events.get(id).map(|e| &e.kind), Some(EventKind::UserPrompt { .. }))
                .then_some(i)
        }) else {
            return; // keine Turns (z. B. nur manuelle /run) → Schätzung bleibt
        };
        let user_id = self.order[upos];

        // 2) Runden des Turns (Assistant-Kinder des User-Prompts) samt ihren
        //    Tools sowie die total_tokens-Baseline der Vorgänger-Runde.
        let mut rounds: Vec<EventId> = Vec::new();
        let mut tools_of: HashMap<EventId, Vec<EventId>> = HashMap::new();
        let mut prev_total: Option<u64> = None;
        for (idx, &id) in self.order.iter().enumerate() {
            let Some(ev) = self.events.get(&id) else {
                continue;
            };
            if idx < upos {
                if let EventKind::Assistant { reported_usage, .. } = &ev.kind {
                    if reported_usage.total_tokens > 0 {
                        prev_total = Some(reported_usage.total_tokens);
                    }
                }
                continue;
            }
            if idx == upos {
                continue; // der UserPrompt selbst
            }
            match &ev.kind {
                EventKind::Assistant { .. } if ev.parent_id == Some(user_id) => rounds.push(id),
                EventKind::Tool { .. } => {
                    if let Some(pid) = ev.parent_id {
                        tools_of.entry(pid).or_default().push(id);
                    }
                }
                _ => {}
            }
        }

        // 3) User-Prompt: `prompt_tokens` der ersten Runde des Turns minus
        //    `total_tokens` der Vorgänger-Runde. Ohne Vorgänger-Runde mit Usage
        //    keine Ableitung – die initiale Schätzung bleibt dann bestehen.
        let user_tokens = match (
            rounds.first().and_then(|&r| round_prompt(self, r)),
            prev_total,
        ) {
            (Some(p), Some(pt)) if p > pt => Some(p - pt),
            _ => None,
        };

        // 4) Rechnen (nur Lesen) – je Runde Completion verteilen und die
        //    Tool-Antworten aus der prompt-Differenz zur Folgerunde ableiten.
        let mut assistant_updates: Vec<(EventId, u64, u64)> = Vec::with_capacity(rounds.len());
        let mut tool_updates: Vec<(EventId, u64, u64)> = Vec::new();
        for (ri, &round) in rounds.iter().enumerate() {
            let Some(ev) = self.events.get(&round) else {
                continue;
            };
            let EventKind::Assistant {
                reasoning,
                text,
                reported_usage,
                completion_parts,
                ..
            } = &ev.kind
            else {
                continue;
            };
            let tools: Vec<EventId> = tools_of.get(&round).cloned().unwrap_or_default();
            let completion = reported_usage.completion_tokens;

            // completion_tokens der Runde → reasoning, text, tool_calls.
            let mut weights: Vec<u64> = vec![reasoning.len() as u64, text.len() as u64];
            weights.extend(tools.iter().map(|&t| call_len(self, t)));
            let total_w: u64 = weights.iter().sum();
            let shares: Vec<u64> = if completion > 0 && total_w > 0 {
                let mut s: Vec<u64> = weights.iter().map(|w| completion * w / total_w).collect();
                let used: u64 = s.iter().sum();
                if let Some(i) = weights.iter().rposition(|w| *w > 0) {
                    s[i] += completion - used;
                }
                s
            } else {
                // Kein Usage → Schätzung je Bestandteil.
                vec![estimate_tokens(reasoning), estimate_tokens(text)]
                    .into_iter()
                    .chain(tools.iter().map(|&t| call_estimate(self, t)))
                    .collect()
            };

            // Beim Streaming gemessene Teile (reasoning/content/je tool_call)
            // haben Vorrang vor der proportionalen Aufteilung – sie sind exakt
            // aus den usage-Inkrementen gemessen statt geraten.
            let measured = completion_parts.as_ref().filter(|p| {
                p.reasoning + p.content + p.tool_calls.iter().copied().sum::<u64>() > 0
            });
            // Gab es eine MESSUNG der Tool-Call-Anteile (Summe > 0)? Nur dann
            // ist die gemessene Aufschlüsselung für die Tool-Spalte belastbar.
            let measured_tools = measured
                .map(|p| p.tool_calls.iter().copied().sum::<u64>())
                .is_some_and(|s| s > 0);
            let (nr, nt, tool_in): (u64, u64, Vec<u64>) = match (measured, measured_tools) {
                // Gemessene Tool-Calls → exakte Aufschlüsselung.
                (Some(p), true) => {
                    let mut ti: Vec<u64> = p.tool_calls.to_vec();
                    ti.resize(tools.len(), 0);
                    (p.reasoning, p.content, ti)
                }
                // reasoning/content gemessen, aber die Tool-Call-Anteile sind 0
                // (z. B. Responses-Stream, dessen Tool-Argument-Bytes nicht in
                // die usage-Inkremente einfließen). Damit die Runde EXAKT auf
                // die gemeldete Completion summiert (jedes reportete Token
                // genau einmal – kein Doppelzählen durch einen zweiten,
                // proportionalen Tool-Anteil), ist der Tool-Anteil der REST der
                // Completion nach den gemessenen reasoning/content-Tokens.
                // Die Antworten der Tool-Zeile (call+result) bleiben dabei
                // verzerrungsfrei nach Call-Bytes verteilt.
                (Some(p), false) => {
                    let used = p.reasoning + p.content;
                    let rem = completion.saturating_sub(used);
                    let tool_w: Vec<u64> = tools.iter().map(|&t| call_len(self, t)).collect();
                    let sum_w: u64 = tool_w.iter().sum();
                    let ti: Vec<u64> = if rem > 0 && sum_w > 0 {
                        let mut s: Vec<u64> = tool_w.iter().map(|w| rem * w / sum_w).collect();
                        let used_w: u64 = s.iter().sum();
                        if let Some(i) = tool_w.iter().rposition(|w| *w > 0) {
                            s[i] += rem - used_w;
                        }
                        s
                    } else {
                        vec![0; tools.len()]
                    };
                    (p.reasoning, p.content, ti)
                }
                (None, _) => (shares[0], shares[1], shares[2..].to_vec()),
            };
            assistant_updates.push((round, nr, nt));

            // Tool-Antworten: prompt-Differenz der Folgerunde zum TOTAL der
            // Runde (`prompt + completion`). Die eigene Completion der Runde
            // (reasoning/text/tool_calls) landet zwar ebenfalls im
            // Folgerunden-Prompt, gehört aber DIESER Runde (wird separat
            // verbucht) – sie zählt NICHT zu den Tool-Ergebnissen und wird
            // daher abgezogen. Nach Output-Länge verteilt; ohne Folgerunde/
            // Usage der Runde/positive Differenz → Schätzung.
            let next_prompt = rounds.get(ri + 1).and_then(|&n| round_prompt(self, n));
            let delta = match (next_prompt, reported_usage.total_tokens) {
                (Some(np), tot) if tot > 0 && np > tot => np - tot,
                _ => 0,
            };
            let out_len: Vec<u64> = tools.iter().map(|&t| output_len(self, t)).collect();
            let total_out: u64 = out_len.iter().sum();
            let out_share: Vec<u64> = if delta > 0 && total_out > 0 {
                let mut s: Vec<u64> = out_len.iter().map(|w| delta * w / total_out).collect();
                let used: u64 = s.iter().sum();
                if let Some(i) = out_len.iter().rposition(|w| *w > 0) {
                    s[i] += delta - used;
                }
                s
            } else {
                tools.iter().map(|&t| output_estimate(self, t)).collect()
            };
            for (k, &t) in tools.iter().enumerate() {
                tool_updates.push((t, tool_in[k], out_share[k]));
            }
        }

        // 5) Anwenden (einmal mutieren).
        if let Some(t) = user_tokens {
            self.set_user_prompt_tokens(user_id, t);
        }
        for (id, nr, nt) in assistant_updates {
            self.set_assistant_tokens(id, nr, nt);
        }
        for (id, ni, no) in tool_updates {
            self.set_tool_tokens(id, ni, no);
        }
    }

    // ── context_len (kumulative Kontextlänge je Event) ────────────────────

    /// Bestätigte (servergemessene) Kontextlänge eines Assistant-Events –
    /// die zentrale Quelle für bestätigte Anker (Renderer + Kompaktierung).
    /// `None`, wenn keine bestätigte Usage vorliegt.
    pub(crate) fn confirmed_context_len(&self, id: EventId) -> Option<u64> {
        let ev = self.events.get(&id)?;
        let EventKind::Assistant {
            reported_usage,
            tool_event_ids,
            completion_parts,
            ..
        } = &ev.kind
        else {
            return None;
        };
        if reported_usage.total_tokens == 0 {
            return None;
        }
        // Gemessene Aufschlüsselung (exakt aus den usage-Inkrementen).
        if let Some(p) = completion_parts {
            if !p.tool_calls.is_empty() {
                return Some(reported_usage.prompt_tokens + p.reasoning + p.content);
            }
            return Some(reported_usage.total_tokens);
        }
        // Keine parts: Tool-Call-Länge aus den angehängten Tool-Events ableiten.
        let tool_len: u64 = tool_event_ids
            .iter()
            .filter_map(|&t| match self.events.get(&t).map(|e| &e.kind) {
                Some(EventKind::Tool {
                    num_tokens_input, ..
                }) => Some(*num_tokens_input),
                _ => None,
            })
            .sum();
        if tool_len > 0 {
            return Some(reported_usage.total_tokens.saturating_sub(tool_len));
        }
        Some(reported_usage.total_tokens)
    }

    /// Bestätigte Kontextlänge eines Nicht-Assistant-Events: die `prompt_tokens`
    /// der DIRECT NACH diesem Event folgenden, abgeschlossenen Assistant-Runde
    /// (Kontext beim Start dieser Antwort – inkl. dieses Events).
    pub(crate) fn verified_context_len(&self, id: EventId) -> Option<u64> {
        let pos = self.order.iter().position(|&i| i == id)?;
        for &nid in &self.order[pos + 1..] {
            let ev = self.events.get(&nid)?;
            if let EventKind::Assistant { reported_usage, .. } = &ev.kind {
                if ev.time_end.is_some() && reported_usage.prompt_tokens > 0 {
                    return Some(reported_usage.prompt_tokens);
                }
            }
        }
        None
    }

    /// Ist die gespeicherte `context_len` dieses Events noch die aktuell
    /// ableitbare bestätigte Zahl? Grün ⇔ `context_len == bestätigter Wert`.
    /// Nach einer Kompaktierung ist sie verschoben und weicht ab → grau.
    pub(crate) fn context_is_green(&self, id: EventId) -> bool {
        let Some(ev) = self.events.get(&id) else {
            return false;
        };
        let Some(cl) = ev.context_len else {
            return false;
        };
        let confirmed = match &ev.kind {
            EventKind::Assistant { .. } => self.confirmed_context_len(id),
            // Manual-/Run-Tools ohne Assistant davor? Die Folge-Runde zählt
            // ohnehin – `verified_context_len` deckt User, Tool und Archive ab.
            _ => self.verified_context_len(id),
        };
        confirmed == Some(cl)
    }

    /// Geschätzter Token-Beitrag eines finalisierten Events zur laufenden
    /// Kontextlänge: nutzt abgeleitete Werte (`num_tokens`, `num_tokens_*`),
    /// sonst Zeichen-Schätzung. Der Output von Tool-Kindern zählt beim
    /// Parent-Assistant (dort gebündelt), das Tool selbst trägt nichts bei.
    /// Auch für OFFENE Events nutzbar (aktueller Streaming-Stand).
    pub(crate) fn estimate_contribution(&self, id: EventId) -> u64 {
        let Some(ev) = self.events.get(&id) else {
            return 0;
        };
        match &ev.kind {
            EventKind::UserPrompt {
                text, num_tokens, ..
            } => {
                if *num_tokens > 0 {
                    *num_tokens
                } else {
                    estimate_tokens(text)
                }
            }
            EventKind::Assistant {
                reasoning,
                text,
                num_tokens_reasoning,
                num_tokens_text,
                tool_event_ids,
                ..
            } => {
                // Leere Bereiche tragen 0 bei (analog zum Renderer, der sie
                // über `!is_empty()`-Gates überspringt – `estimate_tokens("")`
                // wäre sonst fälschlich 1).
                let r = if reasoning.is_empty() {
                    0
                } else if *num_tokens_reasoning > 0 {
                    *num_tokens_reasoning
                } else {
                    estimate_tokens(reasoning)
                };
                let t = if text.is_empty() {
                    0
                } else if *num_tokens_text > 0 {
                    *num_tokens_text
                } else {
                    estimate_tokens(text)
                };
                let tools_add: u64 = tool_event_ids
                    .iter()
                    .filter_map(|&tid| match self.events.get(&tid).map(|e| &e.kind) {
                        Some(EventKind::Tool {
                            output,
                            num_tokens_output,
                            ..
                        }) => Some(if *num_tokens_output > 0 {
                            *num_tokens_output
                        } else {
                            estimate_tokens(output)
                        }),
                        _ => None,
                    })
                    .sum();
                r + t + tools_add
            }
            EventKind::Tool { .. } => 0,
            EventKind::Archive { num_tokens, .. } => *num_tokens,
            _ => 0,
        }
    }

    /// „Begleicht“ die `context_len` aller finalisierten Events ab `start_index`
    /// (Chronologie): bestätigter Wert, wo vorhanden (Assistant: eigene
    /// gemessene Zahl; User/Tool: `prompt_tokens` der Folge-Runde), sonst
    /// geschätzte kumulative Kontextlänge. Läuft nach Turn-Abschluss; ältere
    /// Events (inkl. durch Kompaktierung verschobener) bleiben unberührt,
    /// offene (streaming) Events behalten `None`.
    pub(crate) fn settle_context_tail(&mut self, start_index: usize) {
        let order = self.order.clone();
        // Laufende Basis: context_len des Events unmittelbar vor `start_index`.
        let mut running: u64 = if start_index > 0 {
            self.events
                .get(&order[start_index - 1])
                .and_then(|ev| ev.context_len)
                .unwrap_or(0)
        } else {
            0
        };
        let mut updates: Vec<(EventId, u64)> = Vec::new();
        for &id in &order[start_index..] {
            let Some(ev) = self.events.get(&id) else {
                continue;
            };
            if ev.time_end.is_none() {
                continue; // offen → None lassen
            }
            let confirmed = match &ev.kind {
                EventKind::Assistant { .. } => self.confirmed_context_len(id),
                _ => self.verified_context_len(id),
            };
            if let Some(c) = confirmed {
                updates.push((id, c));
                running = c;
            } else {
                running += self.estimate_contribution(id);
                updates.push((id, running));
            }
        }
        for (id, v) in updates {
            if let Some(ev) = self.events.get_mut(&id) {
                ev.context_len = Some(v);
            }
        }
    }
}

/// Liest die bestätigten `prompt_tokens` einer abgeschlossenen Assistant-Runde.
fn round_prompt(chat: &Chat, id: EventId) -> Option<u64> {
    match chat.events.get(&id).map(|e| &e.kind) {
        Some(EventKind::Assistant { reported_usage, .. }) if reported_usage.prompt_tokens > 0 => {
            Some(reported_usage.prompt_tokens)
        }
        _ => None,
    }
}

/// Länge des Tool-Calls (Funktionsname + Argumente) als Gewicht.
fn call_len(chat: &Chat, id: EventId) -> u64 {
    match chat.events.get(&id).map(|e| &e.kind) {
        Some(EventKind::Tool {
            function_name,
            arguments,
            ..
        }) => (function_name.len() + arguments.len()) as u64,
        _ => 0,
    }
}

/// Schätzung der Tool-Call-Tokens (4 Zeichen ≈ 1 Token).
fn call_estimate(chat: &Chat, id: EventId) -> u64 {
    match chat.events.get(&id).map(|e| &e.kind) {
        Some(EventKind::Tool {
            function_name,
            arguments,
            ..
        }) => estimate_tokens(function_name) + estimate_tokens(arguments),
        _ => 0,
    }
}

/// Länge des Tool-Ergebnistextes als Gewicht.
fn output_len(chat: &Chat, id: EventId) -> u64 {
    match chat.events.get(&id).map(|e| &e.kind) {
        Some(EventKind::Tool { output, .. }) => output.len() as u64,
        _ => 0,
    }
}

/// Schätzung der Tool-Antwort-Tokens (4 Zeichen ≈ 1 Token).
fn output_estimate(chat: &Chat, id: EventId) -> u64 {
    match chat.events.get(&id).map(|e| &e.kind) {
        Some(EventKind::Tool { output, .. }) => estimate_tokens(output),
        _ => 0,
    }
}

/// Helfer: erzeugt ein abgeschlossenes Kontroll-Event und hängt es an.
fn push_control(chat: &mut Chat, kind: EventKind, when: Instant) {
    let id = chat.next_id;
    chat.next_id += 1;
    chat.events.insert(
        id,
        ChatEvent {
            previous_id: chat.order.last().copied(),
            parent_id: None,
            time_begin: Some(when),
            time_end: Some(when),
            kind,
            context_len: None,
        },
    );
    chat.order.push(id);
}

// ── Ein Event ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct ChatEvent {
    /// Vorgänger in der Chronologie (sekundär; primär ist `Chat.order`).
    pub previous_id: Option<EventId>,
    /// Gruppierung / Baum:
    ///   Assistant → parent_id = auslösendes UserPrompt
    ///   Tool      → parent_id = seine Assistant-Unterrunde
    ///   manual    → Tool OHNE parent_id (kein Turn zugehörig)
    pub parent_id: Option<EventId>,
    pub time_begin: Option<Instant>,
    /// `None` = OFFEN/live (mutierbar pro Frame, wird neu gerendert).
    pub time_end: Option<Instant>,
    pub kind: EventKind,
    /// Kumulative Kontextlänge (Tokens) NACH diesem Event.
    /// `None` solange das Event offen (streaming) ist; wird beim
    /// Finalisieren auf die geschätzte Kontextlänge gesetzt und beim
    /// Eintreffen eines bestätigten Wertes (reported_usage) überschrieben.
    /// Bei Kompaktierung werden alle Werte nach der Summary um den Shift
    /// reduziert (einmaliges Update, kein Reactivate).
    pub context_len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    // ── Nutzer-Ebene ──
    UserPrompt {
        text: String,
        /// Nur Anzeige (Band-Farbe). Die angebotenen Tools je HTTP-Request
        /// kommen aus dem LIVE-`session.permission`, nicht aus der Historie.
        permission: Permission,
        /// Stempel des Modells dieses Turns (Fußzeile).
        model: String,
        /// Abgeleitet: prompt_tokens-Differenz zum Vorgänger-Turn (siehe §4.3
        /// des Plans); ohne Vorgänger/Usage → Schätzung.
        num_tokens: u64,
    },

    // ── Modell-Ebene: EIN Event pro HTTP-Unterrunde (eine Wire-assistant) ──
    Assistant {
        /// Gedanken DIESER Runde (leer = keine).
        reasoning: String,
        /// Gestreamter Text DIESER Runde (echt, nie "leer weil Event").
        text: String,
        /// Referenzen auf die Tool-Events dieser Runde (maßgebliche Reihenfolge
        /// der Wire-`tool_calls`).
        tool_event_ids: Vec<EventId>,
        /// Abgeleitet (siehe §4.3) – 0 falls kein Usage.
        num_tokens_reasoning: u64,
        num_tokens_text: u64,
        /// Vom Server bestätigt (diese Runde); 0-Default wenn der Server nichts
        /// liefert.
        reported_usage: Usage,
        /// Beim Streaming gemessene Completion-Token je Bereich (reasoning /
        /// content / tool_calls). `None` bzw. `Some` mit Null-Summe = nicht
        /// gemessen → `derive_last_turn_tokens` fällt auf die proportionale
        /// Aufteilung bzw. Schätzung zurück.
        completion_parts: Option<CompletionParts>,
        /// Beim Streaming gemessene Metriken dieser Runde (TTFT, Stream-Zeit,
        /// Tokens aus den usage-Inkrementen). `None` = keine Tokens empfangen
        /// bzw. Runde läuft noch nicht zu Ende. Grundlage der akkumulierten
        /// TTFT/der durchschnittlichen TPS in der Antwort-Fußzeile.
        metrics: Option<RoundMetrics>,
    },

    // ── Werkzeug-Ebene ──
    Tool {
        /// Vom LLM geliefert oder synthetisiert ("call_aidev_...").
        tool_call_id: String,
        function_name: String,
        /// Roh-JSON der Argumente (normalisiert).
        arguments: String,
        /// VOLLER Ergebnistext – genau einmal.
        output: String,
        /// Nur UI-Zusatz; KEIN für die LLM-Kommunikation relevanter Text.
        kind: ToolKind,
        /// Abgeleitet (siehe §4.3) – 0 falls kein Usage.
        num_tokens_input: u64,
        num_tokens_output: u64,
    },

    // ── Verwaltung / System ──
    /// Markiert einen abgebrochenen Turn (Anzeige "abgebrochen", Fußzeile).
    Abort,
    /// Ergebnis der Kompaktierung. `num_tokens` ist die Token-Zahl der Summary
    /// (aus dem Kompaktierungs-Aufruf abgeleitet; Fallback: Zeichen-Schätzung).
    Archive {
        summary: String,
        num_tokens: u64,
        archived_events: usize,
    },
}

/// UI-/Render-Zusatzdaten eines Tools – bewusst OHNE Ausgabetext (der steht nur
/// in `Tool.output`). Kein Duplikat mehr.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolKind {
    Run {
        cwd: String,
        command: String,
        exit_code: Option<i32>,
    },
    Edit {
        path: String,
        rows: Vec<crate::diff::DiffRow>,
    },
    Read { path: String, range: String },
    Grep { pattern: String, path: String, include: String, num_results:u32 },
    Glob { pattern: String, num_results:u32 },
    Webfetch { url: String, prompt: String },
    Write {path: String },
}

// ── Projection: `api_messages` ────────────────────────────────────────────

/// Baut aus dem Event-Log die Wire-Nachrichten für die API (OpenAI-Format).
///
/// Regeln (siehe Plan §4):
/// - `chat.order` wird in Reihenfolge durchlaufen; JEDES Event wird abgebildet,
///   es gibt kein `publish_to_api`-Filter – auch abgebrochene Inhalte werden
///   mitgeschickt, soweit der Dialog reicht.
/// - **Ab der letzten Summary:** existiert ein `Archive`-Event, startet die
///   Projektion genau dort (Summary inklusive) – ältere, bereits
///   zusammengefasste Nachrichten bleiben im Speicher/der UI, werden aber nicht
///   mehr gesendet (reduziert das Kontextfenster).
/// - `Assistant` = eine Wire-Runde 1:1. Ohne Tool-Calls eine `assistant`-Nachricht
///   (`reasoning_content` nur wenn reasoning nicht leer); mit Tool-Calls eine
///   `assistant`-Nachricht inkl. `tool_calls` (reasoning_content immer, Vertrag)
///   gefolgt von je einer `tool`-Nachricht pro Tool-Kind.
/// - Manuelle Tools (`parent_id == None`) werden übersprungen.
pub fn api_messages(chat: &Chat) -> Vec<WireMessage> {
    // Nur ab der LETZTEN Summary projizieren: die zuvor zusammengefassten
    // Nachrichten sind im Speicher/der UI erhalten, werden aber nicht mehr
    // mitgesendet. Ohne Archive beginnt die Projektion bei 0 (voller Dialog).
    let start = chat
        .order
        .iter()
        .rposition(|id| matches!(chat.events.get(id).map(|ev| &ev.kind), Some(EventKind::Archive { .. })))
        .unwrap_or(0);
    let mut out = Vec::new();
    for id in &chat.order[start..] {
        let Some(ev) = chat.events.get(id) else {
            continue;
        };
        match &ev.kind {
            EventKind::UserPrompt { text, .. } => out.push(user(text)),
            EventKind::Assistant {
                reasoning,
                text,
                tool_event_ids,
                ..
            } => {
                if tool_event_ids.is_empty() {
                    // reine Antwort ohne Tool-Runde
                    out.push(assistant(
                        text,
                        if reasoning.is_empty() {
                            None
                        } else {
                            Some(reasoning.clone())
                        },
                        None,
                    ));
                } else {
                    // Tool-Runde: assistant mit tool_calls + tool-Nachrichten
                    let calls: Vec<WireToolCall> = tool_event_ids
                        .iter()
                        .filter_map(|t_id| chat.events.get(t_id))
                        .map(|t| match &t.kind {
                            EventKind::Tool {
                                tool_call_id,
                                function_name,
                                arguments,
                                ..
                            } => WireToolCall {
                                id: tool_call_id.clone(),
                                ty: "function".into(),
                                function: WireFunction {
                                    name: function_name.clone(),
                                    arguments: arguments.clone(),
                                },
                            },
                            _ => unreachable!("tool_event_id verweist nicht auf ein Tool"),
                        })
                        .collect();
                    out.push(assistant(text, Some(reasoning.clone()), Some(calls)));
                    for t_id in tool_event_ids {
                        if let Some(t) = chat.events.get(t_id) {
                            if let EventKind::Tool {
                                tool_call_id,
                                output,
                                ..
                            } = &t.kind
                            {
                                out.push(WireMessage {
                                    role: "tool".into(),
                                    content: Some(output.clone()),
                                    reasoning_content: None,
                                    tool_calls: None,
                                    tool_call_id: Some(tool_call_id.clone()),
                                });
                            }
                        }
                    }
                }
            }
            EventKind::Tool { .. } => {
                // manual /run (parent_id == None) und Tool-Kinder ohne eigene
                // assistant → übersprungen (die Tool-Ergebnisse erscheinen über
                // ihre Assistant-Runde).
            }
            EventKind::Archive { summary, .. } => {
                // Der Marker „[Compressed history - N earlier messages]“ steht
                // bereits am Anfang von `summary` (siehe `compact_chat_messages`);
                // die Projektion hängt ihn nicht noch einmal davor.
                out.push(user(summary.trim()));
            }
            EventKind::Abort => {
                // bewusst nicht projiziert (Anzeige/Steuerung)
            }
        }
    }
    out
}

fn user(content: &str) -> WireMessage {
    WireMessage {
        role: "user".into(),
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

fn assistant(
    content: &str,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<WireToolCall>>,
) -> WireMessage {
    WireMessage {
        role: "assistant".into(),
        content: if content.is_empty() {
            None
        } else {
            Some(content.to_string())
        },
        reasoning_content,
        tool_calls,
        tool_call_id: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_messages_projiziert_user_und_assistant() {
        let mut chat = Chat::new();
        chat.push_user_prompt("hallo".into(), Permission::Read, "m".into(), 0, Instant::now());
        let aid = chat.open_assistant(None, String::new(), "hi".into(), Instant::now());
        chat.finalize_assistant(aid, Instant::now(), zero(), 0, 0, false);

        assert_eq!(
            api_messages(&chat),
            vec![
                wire("user", Some("hallo".into()), None, None, None),
                wire("assistant", Some("hi".into()), None, None, None),
            ]
        );
    }

    #[test]
    fn kompaktierung_behaelt_altes_an_der_boundary_und_projiziert_ab_summary() {
        let mut chat = Chat::new();
        let a = chat.push_user_prompt("a".into(), Permission::Read, "m".into(), 0, Instant::now());
        let b = chat.push_user_prompt("b".into(), Permission::Read, "m".into(), 0, Instant::now());
        let c = chat.push_user_prompt("c".into(), Permission::Read, "m".into(), 0, Instant::now());
        let arch = chat.compact(
            2,
            "[Compressed history - 2 earlier messages]\n\nalt".into(),
            42,
        );
        assert_eq!(arch, 2);
        // Altbestand bleibt erhalten, Summary sitzt genau an der boundary:
        // a, b, Archive, c.
        assert_eq!(chat.order().len(), 4);
        assert_eq!(chat.order()[0], a);
        assert_eq!(chat.order()[1], b);
        assert_eq!(chat.order()[3], c);
        let sum_id = chat.order()[2];
        // Summary trägt die übergebene Token-Zahl.
        match &chat.event(sum_id).map(|e| &e.kind) {
            Some(EventKind::Archive {
                num_tokens, ..
            }) => assert_eq!(*num_tokens, 42),
            other => panic!("unerwartet: {other:?}"),
        }
        // previous_id-Verkettung: Summary hängt an b, c hängt an der Summary.
        assert_eq!(chat.event(sum_id).and_then(|e| e.previous_id), Some(b));
        assert_eq!(chat.event(c).and_then(|e| e.previous_id), Some(sum_id));
        // Projektion sendet nur die Nachrichten ab der letzten Summary.
        let wire = api_messages(&chat);
        assert_eq!(wire.len(), 2, "nur Summary + letzter User-Prompt");
        assert_eq!(wire[0].role, "user");
        assert_eq!(
            wire[0].content.as_deref(),
            Some("[Compressed history - 2 earlier messages]\n\nalt")
        );
        assert_eq!(wire[1].content.as_deref(), Some("c"));
    }

    #[test]
    fn derive_benutzt_gemessene_completion_parts_statt_proportional() {
        let mut chat = Chat::new();
        let uid = chat.push_user_prompt(
            "frage".into(),
            Permission::Read,
            "m".into(),
            0,
            Instant::now(),
        );
        let aid = chat.open_assistant(Some(uid), "gedanke".into(), "antwort".into(), Instant::now());
        let tid = chat.open_tool(
            Some(aid),
            "call_1".into(),
            "read".into(),
            r#"{"path":"x"}"#.into(),
            String::new(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            Instant::now(),
        );
        chat.set_tool_final(
            tid,
            "inhalt".into(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            0,
            0,
            Instant::now(),
        );
        chat.finalize_assistant(
            aid,
            Instant::now(),
            Usage {
                prompt_tokens: 100,
                completion_tokens: 60,
                total_tokens: 160,
                cached_tokens: None,
            },
            0,
            0,
            false,
        );
        // Beim Streaming gemessene Teile: reasoning 30, content 10, tool 20.
        chat.set_completion_parts(
            aid,
            Some(crate::llm::CompletionParts {
                reasoning: 30,
                content: 10,
                tool_calls: vec![20],
            }),
        );

        chat.derive_last_turn_tokens();

        // Die gemessenen Werte ersetzen die proportionale Aufteilung exakt.
        let assistant = chat.event(aid).expect("Runde").kind.clone();
        match &assistant {
            EventKind::Assistant {
                num_tokens_reasoning,
                num_tokens_text,
                ..
            } => {
                assert_eq!(*num_tokens_reasoning, 30);
                assert_eq!(*num_tokens_text, 10);
            }
            other => panic!("unerwartet: {other:?}"),
        }
        let tool = chat.event(tid).expect("Tool").kind.clone();
        match &tool {
            EventKind::Tool {
                num_tokens_input, ..
            } => assert_eq!(*num_tokens_input, 20),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn tool_ohne_gemessene_parts_haelt_anteil_statt_null() {
        // Responses-Stream (bzw. beliebiger Stream, dessen Tool-Argument-Bytes
        // nicht in die usage-Inkremente einfließen): `completion_parts` misst
        // reasoning/content, aber die Tool-Call-Anteile stehen auf 0. Der
        // Tool-Call darf dann NICHT mit 0 gebucht werden (sonst kollabiert die
        // Tool-Zeile auf einen einzelnen Token-Wert), sondern fällt auf die
        // proportionale/geschätzte Verteilung zurück.
        let mut chat = Chat::new();
        let uid = chat.push_user_prompt(
            "frage".into(),
            Permission::Read,
            "m".into(),
            0,
            Instant::now(),
        );
        let aid = chat.open_assistant(Some(uid), "gedanke".into(), String::new(), Instant::now());
        let tid = chat.open_tool(
            Some(aid),
            "call_1".into(),
            "read".into(),
            r#"{"path":"x"}"#.into(),
            String::new(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            Instant::now(),
        );
        chat.set_tool_final(
            tid,
            "inhalt".into(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            0,
            0,
            Instant::now(),
        );
        chat.finalize_assistant(
            aid,
            Instant::now(),
            Usage {
                prompt_tokens: 100,
                completion_tokens: 60,
                total_tokens: 160,
                cached_tokens: None,
            },
            0,
            0,
            false,
        );
        // gemessene Parts mit NUR reasoning/content – Tool-Calls ungemessen (0).
        chat.set_completion_parts(
            aid,
            Some(crate::llm::CompletionParts {
                reasoning: 30,
                content: 10,
                tool_calls: vec![0],
            }),
        );

        chat.derive_last_turn_tokens();

        let tool = chat.event(tid).expect("Tool").kind.clone();
        match &tool {
            EventKind::Tool {
                num_tokens_input, ..
            } => {
                // Kein harter 0-Wert mehr: Tool-Anteil = Rest der Completion
                // nach den gemessenen reasoning/content-Tokens (60 − 30 − 10).
                assert_eq!(
                    *num_tokens_input, 20,
                    "Tool-Anteil = Completion-Rest (60 − 30 − 10)"
                );
            }
            other => panic!("unerwartet: {other:?}"),
        }
        // Konsistenz: reasoning + content + tool = gemeldete completion EXAKT
        // (jedes reportete Token genau einmal zugeordnet).
        let assistant = chat.event(aid).expect("Runde").kind.clone();
        let EventKind::Assistant {
            num_tokens_reasoning,
            num_tokens_text,
            ..
        } = &assistant
        else {
            panic!("unerwartet: {assistant:?}");
        };
        assert_eq!(
            *num_tokens_reasoning + *num_tokens_text + 20,
            60,
            "Runde summiert exakt auf die gemeldete completion"
        );
    }

    #[test]
    fn tool_output_ist_prompt_differenz_zur_total_der_vorrunde() {
        let mut chat = Chat::new();
        let uid = chat.push_user_prompt(
            "p".into(),
            Permission::Read,
            "m".into(),
            0,
            Instant::now(),
        );
        // Runde A: reine Tool-Runde, prompt 100 + completion 60 = total 160.
        let a = chat.open_assistant(Some(uid), String::new(), String::new(), Instant::now());
        let t = chat.open_tool(
            Some(a),
            "call_1".into(),
            "read".into(),
            r#"{"path":"x"}"#.into(),
            String::new(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            Instant::now(),
        );
        chat.set_tool_final(
            t,
            "ergebnis".into(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            0,
            0,
            Instant::now(),
        );
        chat.finalize_assistant(
            a,
            Instant::now(),
            Usage {
                prompt_tokens: 100,
                completion_tokens: 60,
                total_tokens: 160,
                cached_tokens: None,
            },
            0,
            0,
            false,
        );
        // Folgerunde B: prompt 400 → Differenz 400 − 160 = 240 (NICHT
        // 400 − 100 = 300: die Completion von A zählt nicht zu den Outputs).
        let b = chat.open_assistant(Some(uid), String::new(), "fertig".into(), Instant::now());
        chat.finalize_assistant(
            b,
            Instant::now(),
            Usage {
                prompt_tokens: 400,
                completion_tokens: 10,
                total_tokens: 410,
                cached_tokens: None,
            },
            0,
            0,
            false,
        );

        chat.derive_last_turn_tokens();

        let tool = chat.event(t).expect("Tool").kind.clone();
        match &tool {
            EventKind::Tool {
                num_tokens_output, ..
            } => assert_eq!(*num_tokens_output, 240),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn tool_output_ohne_usage_der_vorrunde_fallt_auf_schaetzung_zurueck() {
        let mut chat = Chat::new();
        let uid = chat.push_user_prompt(
            "p".into(),
            Permission::Read,
            "m".into(),
            0,
            Instant::now(),
        );
        // Runde A: Tool, aber KEIN Usage (Server liefert nichts) → Basis der
        // Differenz unbekannt, Schätzung bleibt (8 Zeichen ≈ 3 Tokens).
        let a = chat.open_assistant(Some(uid), String::new(), String::new(), Instant::now());
        let t = chat.open_tool(
            Some(a),
            "call_1".into(),
            "read".into(),
            r#"{"path":"x"}"#.into(),
            String::new(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            Instant::now(),
        );
        chat.set_tool_final(
            t,
            "ergebnis".into(),
            ToolKind::Read {
                path: "x".into(),
                range: String::new(),
            },
            0,
            0,
            Instant::now(),
        );
        chat.finalize_assistant(a, Instant::now(), zero(), 0, 0, false);
        let b = chat.open_assistant(Some(uid), String::new(), "fertig".into(), Instant::now());
        chat.finalize_assistant(
            b,
            Instant::now(),
            Usage {
                prompt_tokens: 400,
                completion_tokens: 10,
                total_tokens: 410,
                cached_tokens: None,
            },
            0,
            0,
            false,
        );

        chat.derive_last_turn_tokens();

        let tool = chat.event(t).expect("Tool").kind.clone();
        match &tool {
            EventKind::Tool {
                num_tokens_output, ..
            } => assert_eq!(*num_tokens_output, 3),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    fn wire(
        role: &str,
        content: Option<String>,
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<crate::llm::WireToolCall>>,
        tool_call_id: Option<String>,
    ) -> WireMessage {
        WireMessage {
            role: role.into(),
            content,
            reasoning_content,
            tool_calls,
            tool_call_id,
        }
    }

    fn zero() -> Usage {
        Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: None,
        }
    }
}
