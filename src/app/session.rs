use super::Phase;
use crate::channel::{Channel, ChannelRegistry};
use crate::chat;
use crate::chat::{Chat, EventId, EventKind};
use crate::config::Config;
use crate::editor::Editor;
use crate::llm;
use crate::perm::Permission;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Geteilte, zwischen UI- und Worker-Thread veränderbare Kanalzelle.
///
/// Enthält den zum jeweiligen Zeitpunkt gültigen Kanal. Die Session hält die
/// Zelle als Besitzer; die Stream-Worker erhalten beim Absenden eine `Arc`-
/// Kopie und lesen sie bei jedem Tool-Call frisch aus, damit ein Kanalwechsel
/// (Alt+C) während eines laufenden Streams ab dem **nächsten** Tool-Call den
/// neuen Kanal nutzt. Ein bereits gestarteter Befehl läuft unverändert im
/// Anfangskanal zu Ende.
pub(crate) type LiveChannel = Arc<Mutex<Option<Arc<dyn Channel>>>>;

/// Scroll-Anker der manuellen Chat-Position: ein Block des Inhalts-Stapels
/// (ab dem ersten Inhaltsblock nach dem Logo) plus Zeilen-Offset innerhalb
/// dieses Blocks. Damit bleibt die vertikale Mitte beim Tab-Toggle (Blöcke
/// klappen auf/zu, andere Blöcke wachsen/schrumpfen) stabil – die Umrechnung
/// in Bildschirmzeilen passiert pro Frame über die gecachten Block-Tops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatAnchor {
    pub block: usize,
    pub offset: usize,
}

/// Sicht auf den Chatverlauf – „Zoom-Stufe“ von Detail über Dialog zur
/// Übersicht. Steuert, wie Gedanken, Werkzeug-Ausgaben und Antworten gerendert
/// werden; der Wechsel (Tab / Alt+±) baut nur den Historie-Cache neu, der
/// Scroll-Anker hält dabei die vertikale Mitte (bzw. in Auto-follow das untere
/// Bildende) stabil.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewLevel {
    /// Detail: Gedanken aufgeklappt, Run-Konsolen/Diff-Boxen vollständig.
    Detailed,
    /// Dialog: Gedanken zugeklappt, Run-Konsolen/Diff kompakt.
    Dialog,
    /// Kompakt: Gedanken ausgeblendet, Run-Konsolen kompakt, Diffs als Zusammenfassung.
    #[default]
    Compact,
    /// Übersicht: keine Gedanken, Edits ohne Diff, `run` ohne Ausgabe.
    Overview,
}

impl ViewLevel {
    /// Gedanken-Elemente aufgeklappt darstellen?
    pub fn thoughts_open(self) -> bool {
        matches!(self, ViewLevel::Detailed)
    }

    /// Konsolen-Boxen (Run/Diff) aufgeklappt darstellen?
    pub fn boxes_open(self) -> bool {
        matches!(self, ViewLevel::Detailed | ViewLevel::Dialog)
    }

    /// Übersichtsansicht?
    pub fn is_overview(self) -> bool {
        matches!(self, ViewLevel::Overview)
    }

    /// Eine Zoom-Stufe herauszoomen (in der Übersicht bleibt es).
    pub fn zoom_out(self) -> ViewLevel {
        match self {
            ViewLevel::Detailed => ViewLevel::Dialog,
            ViewLevel::Dialog => ViewLevel::Compact,
            ViewLevel::Compact | ViewLevel::Overview => ViewLevel::Overview,
        }
    }

    /// Eine Zoom-Stufe hineinzoomen (im Detail bleibt es).
    pub fn zoom_in(self) -> ViewLevel {
        match self {
            ViewLevel::Overview => ViewLevel::Compact,
            ViewLevel::Compact => ViewLevel::Dialog,
            ViewLevel::Dialog | ViewLevel::Detailed => ViewLevel::Detailed,
        }
    }
}

/// Eine eigenständige Konversation: eigene Historie, eigener Input, eigener
/// Scroll-/Streaming-Zustand. Die Historie ist ein einziges chronologisches
/// Event-Archiv (`chat`), in dem auch die offenen (wachsenden) Segmente des
/// laufenden Turns stehen (nur ein Zustand: `time_end == None`).
pub struct Session {
    pub id: usize,
    /// Das Event-Log – Quelle der Wahrheit für Historie, Projektion & Rendering.
    pub chat: Chat,
    /// Mehrzeiliges Eingabefeld (Umbruch, Cursor, Selektion).
    pub editor: Editor,
    /// Vergangene, gesendete Eingaben dieser Session (älteste zuerst, neueste
    /// zuletzt) für das Durchlaufen per ↑/↓.
    pub input_history: Vec<String>,
    /// Aktuelle Position im Durchlauf: `None` = bei der frischen Eingabe (Draft),
    /// `Some(i)` = `input_history[i]` wird gerade angezeigt.
    pub hist_cursor: Option<usize>,
    /// Gemerkter Original-Text der Eingabe, solange man in der Historie
    /// durchläuft – mit ↓ ganz zurück wird er wiederhergestellt.
    pub hist_draft: String,
    pub phase: Phase,
    /// Laufendes (noch offenes) Assistant-Event des aktuellen Turns – der
    /// Mutations-Target für Reasoning/Text-Chunks des LLM. `None` im Idle.
    pub open_assistant_id: Option<EventId>,
    /// Parallel laufende (offene) Tool-Events – Mutations-Targets für die
    /// Live-Ausgabe. Sie stehen bereits in `chat` (offen), nicht in einer
    /// eigenen Zone.
    pub open_tool_ids: Vec<EventId>,
    /// Das Tool-Event, das gerade Live-Ausgabe (`ToolOutput`) empfängt – i. d. R.
    /// das zuletzt geöffnete laufende `run`-Werkzeug.
    pub(crate) live_tool: Option<EventId>,
    /// Anzeige-Label des aktuell laufenden Werkzeugs (Statuszeile) – analog zum
    /// alten `active_tool`, nur ohne parallele Puffer-Zone.
    pub(crate) active_tool_label: Option<String>,
    /// Server-bestätigte Usage dieses Turns (vom `Usage`-Event), die bei
    /// `Done`/`Cancelled` an `finish_assistant` durchgereicht wird.
    pub(crate) pending_usage: Option<llm::Usage>,
    /// Beim Streaming gemessene Completion-Token je Bereich (reasoning/content/
    /// tool_calls) dieser Runde – wird zusammen mit `pending_usage` geliefert.
    pub(crate) pending_parts: Option<llm::CompletionParts>,
    /// Letzter Assistant-Turn wurde abgebrochen (Anzeige + Fußzeile).
    pub aborted: bool,
    pub error: Option<String>,
    /// Pfad zum gespeicherten Request/Response-Debug-Material des Fehlers (nur
    /// für LLM/Server-Fehler aus dem Worker; im Chat als Detailzeile gezeigt).
    pub error_debug: Option<String>,
    /// Läuft gerade ein Retry (429/5xx/Netzwerkfehler) mit Backoff? Enthält
    /// die Kurzfassung des Fehlers und den Zeitpunkt des nächsten Versuchs –
    /// die Statuszeile zeigt einen Countdown, `phase` bleibt `WaitingForLLM`.
    pub retrying: Option<(String, Instant)>,
    /// Zuletzt bekannter Prompt-Anteil (Token) – Basis der Live-Context-Anzeige
    /// in der Statuszeile, solange noch kein exakter Usage-Wert vorliegt. Wird
    /// beim Turn-Start aus dem letzten Usage (sonst Zeichen-Heuristik) gesetzt.
    pub prompt_base: u64,
    /// Kontextstand des Live-Tails (Zellen/Auflösung aus dem `ContextEstimate`
    /// der Usage-Bars) – wird bei jedem `draw_chat`-Frame aktualisiert und
    /// speist die Übersichts-Balken (und baut den Live-Tail der Schätzung).
    /// Wird NICHT mehr für die Statuszeile herangezogen (dort zählt nur die
    /// zuletzt gestreamte serverbestätigte `total_tokens`, siehe
    /// `live_usage_total`).
    pub live_context: u64,
    /// MID-STREAM serverbestätigte `total_tokens` der laufenden Runde (aus
    /// `WorkerEvent::UsageUpdate`, sobald der Endpunkt während des Streamings
    /// ein `usage`-Event liefert, bzw. aus dem Runden-`Usage`). Die
    /// Statusleiste zeigt im `WaitingForLLM`-Zustand ausschließlich diesen
    /// Wert (letzte gestreamte Zahl), ohne Schätzung aus anderen Quellen.
    /// `None` = noch kein usage der laufenden Runde gesehen.
    pub live_usage_total: Option<u64>,
    /// HTTP-Response-Header der letzten erfolgreichen LLM-Antwort dieser
    /// Session (Daten für den `Alt+H`-Dialog). Wird bei jeder erfolgreichen
    /// API-Antwort durch den neuesten Satz ersetzt.
    pub last_http_headers: Option<Vec<(String, String)>>,
    /// Abbruch-Flag des aktuell laufenden Workers.
    pub cancel: Arc<AtomicBool>,
    /// Chat-Nachlauf: `true`, wenn die Anzeige automatisch am Ende (unten) bleibt.
    pub chat_follow: bool,
    /// Scroll-Anker: Block-Index + Zeilen-Offset innerhalb des Blocks. Damit
    /// bleibt beim Tab-Toggle (Auf-/Zuklappen ändert Blockhöhen) die vertikale
    /// Mitte stabil; die Umrechnung in Bildschirmzeilen passiert pro Frame über
    /// die Block-Tops (siehe `ui::draw_chat`).
    pub chat_anchor: ChatAnchor,
    /// Anstehende Scroll-Verschiebung in Zeilen (PgUp/PgDn): wird im nächsten
    /// Frame auf den Anker angewendet und dann verbraucht.
    pub chat_scroll_delta: Option<i64>,
    /// Höhe des Chat-Viewports aus dem letzten Frame (für PgUp/PgDn).
    pub chat_viewport: u16,
    /// Versionszähler der abgeschlossenen Historie – wird bei jeder Änderung
    /// inkrementiert; `history_cache` ist gültig, solange dieser Wert (und
    /// Breite/Toggles) unverändert ist.
    pub history_version: u64,
    /// Gecachte, umgebrochene Blöcke der Historie (ohne Logo, ohne Live-Tail).
    pub history_cache: Option<crate::ui::HistoryCache>,
    /// Sicht auf den Chatverlauf (Detail/Dialog/Übersicht) – steuert, wie
    /// Gedanken, Werkzeug-Ausgaben und Antworten dargestellt werden.
    pub view: ViewLevel,
    /// Gebundener Kanal (Schnittstelle zum Dateisystem/der Shell) – optional.
    pub channel: Option<Arc<dyn Channel>>,
    /// Geteilte, **live-veränderbare** Kanalzelle für laufende Streams.
    ///
    /// Beim Absenden wird sie mit dem aktuellen Kanal initialisiert und dem
    /// Worker als `Arc`-Kopie mitgegeben. Ein Kanalwechsel (Alt+C) während
    /// eines laufenden Streams schreibt hier hinein, sodass der Worker für
    /// die **folgenden** Tool-Calls dieses Streams den **neuen** Kanal nutzt —
    /// die zum Absendezeitpunkt gewählte Berechtigung bleibt davon unberührt.
    /// `channel` (oben) bleibt die Anzeige-/Ziel-Referenz; beide werden über
    /// [`Session::set_channel`] gemeinsam gesetzt.
    pub(crate) channel_cell: LiveChannel,
    /// Aktuell gewählte Berechtigung der Eingabe – gilt für den nächsten
    /// Absende-Turn und bleibt als Default für die Folge-Nachricht bestehen.
    pub permission: Permission,
    /// Hat der User die Berechtigung per Tab aktiv umgestellt? Erst dann hat
    /// seine Wahl Vorrang vor dem kanalabhängigen Startwert (sonst wird beim
    /// Kanal-Binden neu auf den passenden Default gesetzt).
    pub permission_touched: bool,
    /// Die Kontext-Kompaktierung läuft (separater Zusammenfassungs-Aufruf).
    pub compacting: bool,
    /// Per `/model <alias>` gewähltes Modell dieser Session – gilt für alle
    /// künftigen Anfragen. `None` = der konfigurierte Default (`config.model`).
    pub model_alias: Option<String>,
    /// Modell-ID des laufenden Turns (beim Absenden gesetzt) – Stempel für die
    /// finale Antwort, damit die Fußzeile auch nach einem späteren Wechsel das
    /// tatsächlich genutzte Modell zeigt.
    pub(crate) active_model: Option<String>,
    /// Zeitpunkt des letzten Absendens (für die Antwort-Fußzeile).
    pub(crate) sent_at: Option<Instant>,
}

impl Session {
    pub(crate) fn new(id: usize) -> Self {
        Session {
            id,
            chat: Chat::new(),
            editor: Editor::new(10),
            input_history: Vec::new(),
            hist_cursor: None,
            hist_draft: String::new(),
            phase: Phase::Idle,
            open_assistant_id: None,
            open_tool_ids: Vec::new(),
            live_tool: None,
            active_tool_label: None,
            pending_usage: None,
            pending_parts: None,
            aborted: false,
            error: None,
            error_debug: None,
            retrying: None,
            prompt_base: 0,
            live_context: 0,
            live_usage_total: None,
            last_http_headers: None,
            cancel: Arc::new(AtomicBool::new(false)),
            chat_follow: true,
            chat_anchor: ChatAnchor::default(),
            chat_scroll_delta: None,
            chat_viewport: 0,
            history_version: 0,
            history_cache: None,
            view: ViewLevel::default(),
            channel: None,
            channel_cell: Arc::new(Mutex::new(None)),
            permission: Permission::default(),
            permission_touched: false,
            compacting: false,
            model_alias: None,
            active_model: None,
            sent_at: None,
        }
    }

    /// Setzt den gebundenen Kanal – hält Anzeige-Referenz (`channel`) und die
    /// **Live-Zelle** (`channel_cell`) synchron. Letztere wird von laufenden
    /// Stream-Workern live gelesen, damit ein Kanalwechsel (Alt+C) noch im
    /// selben Stream wirkt und nachfolgende Tool-Calls den neuen Kanal nutzen.
    pub(crate) fn set_channel(&mut self, ch: Option<Arc<dyn Channel>>) {
        self.channel = ch;
        *self.channel_cell.lock().expect("channel cell lock") = self.channel.clone();
    }

    /// Hängt eine vom User stammende Nachricht an den Verlauf an und erhöht
    /// die Historie-Version. `permission` ist die zum Absendezeitpunkt
    /// gewählte Berechtigung (Anzeige der Band-Farbe), `model` der Stempel des
    /// Turns für die Fußzeile.
    pub(crate) fn push_user_message(
        &mut self,
        content: String,
        permission: Option<Permission>,
        model: String,
    ) {
        // initiale Schätzung (4 Zeichen ≈ 1 Token); wird von der
        // Token-Ableitung überschrieben, sobald bessere Daten vorliegen.
        let est_tokens = llm::estimate_tokens(&content);
        self.chat.push_user_prompt(
            content,
            permission.unwrap_or_default(),
            model,
            est_tokens,
            Instant::now(),
        );
        self.history_version += 1;
    }

    /// Öffnet die erste/eine neue Assistant-Unterrunde des Turns (ein
    /// Wire-assistant); liefert die Event-ID als Mutations-Target.
    pub(crate) fn open_assistant(&mut self, reasoning: String, text: String) -> EventId {
        let parent = self.current_prompt_id();
        let id = self
            .chat
            .open_assistant(parent, reasoning, text, Instant::now());
        self.open_assistant_id = Some(id);
        self.history_version += 1;
        id
    }

    /// Öffnet – falls noch keine Assistant-Sub-Runde offen ist – eine neue Runde
    /// mit leerem Vorspann. Damit haben ankommende Text-/Reasoning-Chunks und
    /// Tool-Starts stets ein gültiges Mutations-Ziel (sonst gingen Antworten
    /// verloren). Wird vom Worker-Drain vor `append_text`/`append_reasoning`
    /// und `open_tool` aufgerufen.
    pub(crate) fn ensure_open_assistant(&mut self) {
        if self.open_assistant_id.is_none() {
            self.open_assistant(String::new(), String::new());
        }
    }

    /// Liefert `true`, wenn die offene Assistant-Runde eine Tool-Runde ist,
    /// deren Werkzeuge ALLE abgeschlossen sind (offene Tool-Liste leer, aber
    /// mindestens ein Tool-Kind registriert). Dann beginnt mit dem nächsten
    /// `Chunk`/`Reasoning`/`Usage` eine NEUE Runde – die alte muss vorher
    /// geschlossen werden (deren `pending_usage`/`pending_parts` gehören noch
    /// zu ihr).
    ///
    /// Hintergrund: Der Worker sendet ToolStart/ToolEnd einer Runde
    /// VERSCHACHTELT (`Start→End→Start→End`). Ein Abschluss direkt am ersten
    /// `ToolEnd` würde die Runde vorschnell schließen; die restlichen Tools
    /// hingen dann an einer frischen, falschen Sub-Runde und verlören ihre
    /// gemessenen Tokens. Stattdessen wird die Runde genau dann geschlossen,
    /// wenn die nächste Runde beginnt.
    pub(crate) fn is_open_tool_round_done(&self) -> bool {
        let Some(aid) = self.open_assistant_id else {
            return false;
        };
        if !self.open_tool_ids.is_empty() {
            return false;
        }
        self.chat
            .event(aid)
            .is_some_and(|ev| matches!(&ev.kind, EventKind::Assistant { tool_event_ids, .. } if !tool_event_ids.is_empty()))
    }

    /// Öffnet ein Tool-Event als Kind der offenen Assistant-Runde (parallel:
    /// mehrere können gleichzeitig offen sein).
    pub(crate) fn open_tool(
        &mut self,
        parent_id: Option<EventId>,
        tool_call_id: String,
        function_name: String,
        arguments: String,
        kind: chat::ToolKind,
    ) -> EventId {
        let id = self.chat.open_tool(
            parent_id,
            tool_call_id,
            function_name,
            arguments,
            String::new(), // Output wächst per append_tool_output
            kind,
            Instant::now(),
        );
        self.open_tool_ids.push(id);
        self.history_version += 1;
        id
    }

    /// Schließt die zuletzt geöffnete Assistant-Runde des Turns ab und setzt
    /// die abgeleiteten Token-Werte bzw. die Server-bestätigte Usage. Bei
    /// `aborted` wird automatisch ein `Abort`-Event direkt danach eingefügt.
    pub(crate) fn finish_assistant(&mut self, aborted: bool, usage: Option<llm::Usage>) {
        let Some(aid) = self.open_assistant_id.take() else {
            return;
        };
        // Noch offene Tool-Kinder vorher schließen (Notfall/Teillauf) – sie
        // bleiben mit "Execution aborted" erhalten.
        for tid in self.open_tool_ids.drain(..) {
            self.chat.finish_tool(tid, Instant::now());
        }
        // `pending_usage` (aus dem `Usage`-Event des Turns) hat Vorrang; der
        // `usage`-Parameter deckt den direkt übergebenen Wert ab.
        let usage = self.pending_usage.take().or(usage).unwrap_or_else(zero_usage);
        // `num_tokens` werden nicht hier, sondern in `derive_last_turn_tokens`
        // über den ganzen Turn verteilt (§4.3). `0` bis dahin.
        self.chat.finalize_assistant(aid, Instant::now(), usage, 0, 0, aborted);
        // Beim Streaming gemessene Completion-Token je Bereich an der Runde
        // festhalten (nur wenn tatsächlich Usage-Messung vorlag – sonst macht
        // `derive_last_turn_tokens` den proportionalen/Estimate-Fallback).
        if let Some(parts) = self.pending_parts.take() {
            if parts.reasoning + parts.content + parts.tool_calls.iter().copied().sum::<u64>() > 0 {
                self.chat.set_completion_parts(aid, Some(parts));
            }
        }
        // Token-Ableitung über den ganzen Turn: verteilt die vom Server
        // reporteten `prompt_tokens`/`completion_tokens` auf User-Prompt,
        // Assistant-Runden und Tools. Vorliegende Messwerte (`completion_parts`)
        // ersetzen dabei die proportionale Aufteilung der `completion_tokens`.
        // Läuft idempotent bei jedem Runden-Abschluss und heilt sich so zu den
        // finalen Werten.
        self.chat.derive_last_turn_tokens();
        self.history_version += 1;
        self.active_tool_clear();
        self.compacting = false;
        self.aborted = aborted;
    }

    /// Hängt Output an ein offenes Tool-Event an (Live-Ausgabe des `run`).
    pub(crate) fn append_tool_output(&mut self, id: EventId, chunk: &str) {
        self.chat.append_tool_output(id, chunk);
        self.history_version += 1;
    }

    /// Finalisiert ein offenes Tool-Event (`ToolEnd`): setzt den voller Output
    /// (für die API-Projektion), das UI-`ToolKind` und schließt es.
    pub(crate) fn finalize_tool_event(&mut self, id: EventId, activity: &llm::ToolActivity) {
        // Funktionsname + Argumente aus dem bereits geöffneten Event lesen
        // (`ToolEnd` trägt sie nicht; sie stecken im ToolStart-Ereignis).
        let (name, arguments, parent_id) = self
            .chat
            .event(id)
            .and_then(|ev| match &ev.kind {
                EventKind::Tool {
                    function_name,
                    arguments,
                    ..
                } => Some((function_name.clone(), arguments.clone(), ev.parent_id)),
                _ => None,
            })
            .unwrap_or_default();
        let kind = tool_kind_from_activity(&name, &arguments, activity);
        // Tokens: Manuelle `/run`-Tools (`parent_id = None`) laufen nicht durch
        // das Modell und brauchen keine Token-Werte → `0` (keine Anzeige).
        // LLM-Tools bekommen hier nur die Schätzung als Baseline (4 Zeichen ≈ 1
        // Token, Fallback aus §4.3); beim Runden-Abschluss überschreibt
        // `derive_last_turn_tokens` mit den usage-verteilten Werten.
        let (call_tokens, answer_tokens) = if parent_id.is_none() {
            (0, 0)
        } else {
            (
                llm::estimate_tokens(&name) + llm::estimate_tokens(&arguments),
                llm::estimate_tokens(&activity.output_full),
            )
        };
        self.chat.set_tool_final(
            id,
            activity.output_full.clone(),
            kind,
            call_tokens,
            answer_tokens,
            Instant::now(),
        );
        self.open_tool_ids.retain(|&t| t != id);
        self.live_tool = None;
        self.active_tool_label = None;
        self.history_version += 1;
    }

    /// Hängt einen Reasoning-Chunk an die offene Assistant-Runde an.
    pub(crate) fn append_reasoning(&mut self, chunk: &str) {
        if let Some(aid) = self.open_assistant_id {
            self.chat.append_reasoning(aid, chunk);
        }
    }

    /// Hängt einen Text-Chunk an die offene Assistant-Runde an.
    pub(crate) fn append_text(&mut self, chunk: &str) {
        if let Some(aid) = self.open_assistant_id {
            self.chat.append_text(aid, chunk);
        }
    }

    /// `id` des zuletzt eingefügten UserPrompt (parent der Turn-Unterrunden).
    fn current_prompt_id(&self) -> Option<EventId> {
        self.chat
            .order()
            .iter()
            .rev()
            .copied()
            .find(|id| {
                self.chat
                    .event(*id)
                    .is_some_and(|e| matches!(e.kind, EventKind::UserPrompt { .. }))
            })
    }

    /// Server-bestätigte Usage der letzten (abgeschlossenen) Assistant-Runde –
    /// Basis für `should_compact` und die Live-Context-Schätzbasis. Liefert
    /// `None`, falls kein abgeschlossener Turn mit Usage vorliegt.
    pub(crate) fn last_usage(&self) -> Option<llm::Usage> {
        self.chat
            .order()
            .iter()
            .rev()
            .filter_map(|id| self.chat.event(*id))
            .find_map(|ev| match &ev.kind {
                EventKind::Assistant {
                    reported_usage, ..
                } if ev.time_end.is_some() => Some(*reported_usage),
                _ => None,
            })
    }

    /// Aufräumen nach Turn-Ende: keine offenen Mutations-Targets mehr.
    fn active_tool_clear(&mut self) {
        self.open_assistant_id = None;
        self.open_tool_ids.clear();
        self.live_tool = None;
        self.active_tool_label = None;
    }

    /// Merkt eine gesendete Eingabe für den ↑/↓-Durchlauf. Leere Eingaben und
    /// direkt aufeinanderfolgende Duplikate werden übersprungen; danach ist der
    /// Durchlauf zurück am Draft (frische Eingabe).
    pub(crate) fn push_history(&mut self, text: &str) {
        let trimmed = text.trim().to_string();
        self.hist_cursor = None;
        self.hist_draft.clear();
        if trimmed.is_empty() {
            return;
        }
        if self.input_history.last().map(String::as_str) == Some(trimmed.as_str()) {
            return;
        }
        self.input_history.push(trimmed);
    }

    /// ↑ in der ersten Zeile: zur vorhergehenden (älteren) gesendeten Eingabe
    /// wechseln. Beim ersten Schritt wird der aktuelle Editor-Text als Draft
    /// gemerkt, zu dem man mit ↓ ganz zurückkehren kann.
    pub(crate) fn history_prev(&mut self) {
        let len = self.input_history.len();
        if len == 0 {
            return;
        }
        let idx = match self.hist_cursor {
            None => {
                self.hist_draft = self.editor.text_string();
                len - 1 // neueste Eingabe zuerst
            }
            Some(i) => {
                if i == 0 {
                    return; // älteste Eingabe erreicht
                }
                i - 1
            }
        };
        self.hist_cursor = Some(idx);
        self.editor.set_text(&self.input_history[idx]);
    }

    /// ↓ in der letzten Zeile: zur nächsten (neueren) gesendeten Eingabe
    /// wechseln; ganz zurück (älteste → Draft) stellt den ursprünglichen
    /// Editor-Text wieder her.
    pub(crate) fn history_next(&mut self) {
        let len = self.input_history.len();
        match self.hist_cursor {
            None => {} // schon beim Draft (entweder am Anfang oder am Ende)
            Some(i) if i + 1 < len => {
                self.hist_cursor = Some(i + 1);
                self.editor.set_text(&self.input_history[i + 1]);
            }
            Some(_) => {
                // neueste Eingabe erreicht → zurück zum Draft
                self.hist_cursor = None;
                let draft = std::mem::take(&mut self.hist_draft);
                self.editor.set_text(&draft);
            }
        }
    }

    /// Wendet eine vom Worker gelieferte Zusammenfassung auf den Verlauf an:
    /// alle Events bis zur `boundary` werden durch ein `Archive`-Event ersetzt,
    /// die letzten `keep` Turns bleiben erhalten. Alle IDs dahinter bleiben
    /// stabil (Render-Cache teilt sich entsprechend auf). `tokens` ist die
    /// Token-Zahl der Summary (aus dem Kompaktierungs-Aufruf abgeleitet).
    pub(crate) fn apply_compaction(&mut self, content: String, tokens: u64, keep: usize) {
        self.compacting = false;
        self.prompt_base = 0;
        let boundary = compact_boundary(self, keep);
        if boundary == 0 {
            return;
        }
        self.chat.compact(boundary, content, tokens);
        self.history_version += 1;
    }
}

fn zero_usage() -> llm::Usage {
    llm::Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: None,
    }
}

/// Baut das UI-`ToolKind` eines Tool-Events aus Funktionsname + Argumenten +
/// `ToolActivity`. Die Parameterzeilen (path/pattern/url/command/…) werden aus
/// der rohen Argument-JSON gelesen – auch für noch offene Tools gültig; die
/// Ergebnis-Daten (`exit_code`, Diff-Zeilen, read-Range, Trefferzahl) kommen
/// aus der `ToolActivity`. Das `ToolKind` trägt bewusst KEINEN Ausgabetext
/// (der steht nur in `Tool.output`) – nur Render-Zusätze.
pub(crate) fn tool_kind_from_activity(
    name: &str,
    arguments: &str,
    a: &llm::ToolActivity,
) -> crate::chat::ToolKind {
    use crate::chat::ToolKind;
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
    let arg = |k: &str| {
        args.get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    match name {
        "run" => ToolKind::Run {
            cwd: a.run.as_ref().map(|r| r.cwd.clone()).unwrap_or_default(),
            command: a
                .run
                .as_ref()
                .map(|r| r.command.clone())
                .unwrap_or_else(|| arg("command")),
            exit_code: a.run.as_ref().and_then(|r| r.exit_code),
        },
        "edit" => ToolKind::Edit {
            path: a
                .diff
                .as_ref()
                .map(|d| d.path.clone())
                .unwrap_or_else(|| arg("path")),
            rows: a.diff.as_ref().map(|d| d.rows.clone()).unwrap_or_default(),
        },
        "read" => ToolKind::Read {
            path: arg("path"),
            range: a.read.as_ref().map(|r| r.range.clone()).unwrap_or_default(),
        },
        "grep" => ToolKind::Grep {
            pattern: arg("pattern"),
            path: arg("path"),
            include: arg("include"),
            num_results: a.output_full.lines().count() as u32,
        },
        "glob" => ToolKind::Glob {
            pattern: arg("pattern"),
            num_results: a.output_full.lines().count() as u32,
        },
        "webfetch" => ToolKind::Webfetch {
            url: arg("url"),
            prompt: arg("prompt"),
        },
        "write" => ToolKind::Write { path: arg("path") },
        _ => ToolKind::Read {
            path: arg("path"),
            range: String::new(),
        },
    }
}

pub(crate) fn apply_default_channel(channels: &ChannelRegistry, session: &mut Session) {
    if let Some(name) = channels.default_channel_name().map(str::to_string) {
        if let Some(ch) = channels.get(&name) {
            session.set_channel(Some(ch));
        }
    }
    apply_channel_permission_default(session);
}

/// Kanalabhängiger Startwert der Berechtigung – nur solange der User sie noch
/// nicht selbst per Tab umgestellt hat (dann bleibt seine Wahl erhalten).
pub(crate) fn apply_channel_permission_default(s: &mut Session) {
    if !s.permission_touched {
        s.permission = s
            .channel
            .as_ref()
            .map(|ch| Permission::default_for(ch.kind()))
            .unwrap_or(Permission::Read);
    }
}

/// Anzahl LLM-relevanter Zeichen im Chat (ohne manuelle `/run`-Tools) –
/// Grundlage der 4-Zeichen-≈-1-Token-Heuristik (Kompaktierung + Live-Context).
/// Offene (wachsende) Events zählen mit ihrem aktuellen Stand.
pub(crate) fn prompt_chars(s: &Session) -> usize {
    let mut total = 0usize;
    for ev in s.chat.iter() {
        match &ev.kind {
            EventKind::UserPrompt { text, .. } => total += text.len(),
            EventKind::Assistant {
                reasoning,
                text,
                tool_event_ids,
                ..
            } => {
                total += text.len() + reasoning.len();
                // Tool-Outputs fließen in den Folge-Prompt ein (Differenz)
                for tid in tool_event_ids {
                    if let Some(t) = s.chat.event(*tid) {
                        if let EventKind::Tool { output, .. } = &t.kind {
                            total += output.len();
                        }
                    }
                }
            }
            EventKind::Archive { num_tokens, .. } => {
                // Gespeicherte Token-Zahl der Summary in Zeichen-Äquivalent
                // überführen (Rückrechnung des `estimate_tokens`: ≈ 4 Zeichen je
                // Token), damit die heuristische Schätzung konsistent bleibt.
                total += (*num_tokens as usize) * 4;
            }
            _ => {}
        }
    }
    total
}

/// Kompaktierungs-Grenze: rückwärts durch `order` die letzten `keep` Turns
/// (UserPrompt-Events) zählen; manuelle Tools zählen nicht. Liefert den
/// `order`-Index, ab dem die Survivors beginnen (0 = nichts zu ersetzen).
pub(crate) fn compact_boundary(s: &Session, keep: usize) -> usize {
    let order = s.chat.order();
    let mut seen_turns = 0usize;
    let mut boundary = order.len();
    for (i, id) in order.iter().enumerate().rev() {
        let manual = s
            .chat
            .event(*id)
            .is_some_and(|e| matches!(e.kind, EventKind::Tool { .. }) && e.parent_id.is_none());
        if manual {
            continue;
        }
        if let Some(ev) = s.chat.event(*id) {
            if matches!(ev.kind, EventKind::UserPrompt { .. }) {
                seen_turns += 1;
                if seen_turns > keep {
                    boundary = i;
                    break;
                }
            }
        }
    }
    boundary
}

/// Entscheidet vor dem Senden, ob der Kontext komprimiert werden soll: Sobald
/// die echten `total_tokens` des letzten Turns (oder ohne Usage eine grobe
/// Zeichen-Heuristik, 4 Zeichen ≈ 1 Token) den Anteil `compact_at` des
/// Kontextfensters erreichen UND genug alte Turns für eine Zusammenfassung
/// existieren. Bewusst `total_tokens` statt `prompt_tokens`: die Antwort des
/// letzten Turns ist Teil des NÄCHSTEN Kontextes (projiziert ab letzter
/// Summary), muss also mitgezählt werden.
pub(crate) fn should_compact(
    s: &Session,
    cfg: &Config,
    ep: &crate::config::ResolvedEndpoint,
) -> bool {
    let threshold = (ep.context_window as f64 * cfg.compact_at) as u64;
    let reached = match s.last_usage() {
        Some(u) => u.total_tokens >= threshold,
        None => (prompt_chars(s) as u64 / 4) >= threshold,
    };
    reached && compact_boundary(s, cfg.compact_keep_turns) > 0
}
