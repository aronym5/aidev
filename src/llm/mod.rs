//! LLM-Kommunikation: Worker, Kompaktierung, Streaming.

use std::sync::mpsc::Sender;
use std::time::Instant;


/// Eine ausgeführte Werkzeug-Option – für den Abschluss im Event-Log.
///
/// Trägt nur noch das, was das Event-Log beim Abschluss braucht: den vollen
/// Ergebnistext (Modell + API-Projektion) und die UI-Render-Zusätze. Label,
/// Status und gekürzte Anzeige-Ausgabe kommen im neuen Layout aus dem
/// Chat-Event bzw. dem `ToolStart`-Label, nicht aus diesem Event.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolActivity {
    /// VOLLER Ergebnistext, wie er an das Modell zurückgeht (`result.text`) –
    /// Grundlage für das `Tool.output` im Event-Log (genau einmal, für die
    /// API-Projektion).
    pub output_full: String,
    /// Bei `run` mit Ausgabe: abgesetzte Konsolen-Box für die Chat-Anzeige.
    pub run: Option<RunInfo>,
    /// Bei `edit`: zweispaltige Diff-Darstellung für die Chat-Anzeige.
    pub diff: Option<crate::diff::DiffInfo>,
    /// Bei `read` als Fenster-Lesung: die kompakt gelesenen Zeilennummern.
    pub read: Option<ReadInfo>,
}

/// Grobe Token-Schätzung für Text (4 Zeichen ≈ 1 Token), wie in der
/// Live-Context-Anzeige der Statuszeile.
pub(crate) fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64 / 4) + 1
}

/// Detail eines `run`-Aufrufs für die Konsolen-Darstellung im Chat.
#[derive(Debug, Clone, PartialEq)]
pub struct RunInfo {
    /// Anzeige-Wurzel des Aufrufs (Arbeitsverzeichnis), z. B. ein Pfad.
    pub cwd: String,
    /// Kommando samt Argumenten (Rohform) – Header der Konsolen-Box.
    pub command: String,
    /// Ausgabe (stdout+stderr) für die Anzeige, gekürzt auf `RUN_CONSOLE_CAP`.
    /// Über dem Limit bleibt das Ende vollständig (ganze Zeilen), vorne wird
    /// abgeschnitten und mit „…“ markiert – das Modell bekam den vollen Text.
    pub output: String,
    /// Exit-Code; `None` bedeutet Timeout/abgebrochen.
    pub exit_code: Option<i32>,
}

/// Detail eines `read`-Aufrufs: die Zeilennummern des gelesenen Fensters
/// (kompakt, z. B. `12-34`). Wird nur gesetzt, wenn das Fenster NICHT die
/// ganze Datei umfasst (offset/limit oder die 2000-Zeilen-Decke); über
/// `tool_kind_from_activity` landet die Range im `ToolKind::Read` der Anzeige.
/// Ein komplettes Lesen bleibt ohne Zusatz. Der Dateiinhalt geht nur an das
/// Modell, nicht in die Chat-Anzeige.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadInfo {
    /// Die gelesenen Zeilennummern des Fensters, kompakt `12-34` bzw. `12`.
    pub range: String,
}

/// Empfangs-'Ende' (Sender) für die Antwort der UI auf eine Einzel-Bestätigung
/// eines `run`-Aufrufs (Local-Kanal im `ConfirmEach`-Modus). `mpsc::Sender`
/// ist `Clone`, aber nicht `PartialEq` – deshalb manuelle Implementierungen,
/// damit `WorkerEvent` weiterhin `Clone + PartialEq` ableiten kann.
#[derive(Debug)]
pub struct ExecConfirmReply(pub Sender<bool>);

impl Clone for ExecConfirmReply {
    fn clone(&self) -> Self {
        ExecConfirmReply(self.0.clone())
    }
}

impl PartialEq for ExecConfirmReply {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// Event, das der Worker-Thread zurück zum Haupt-Thread sendet.
/// Das erste Feld ist immer die ID der zugehörigen Konversation.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerEvent {
    /// Gedanken-/Reasoning-Fragment (falls der Endpunkt `reasoning_content` liefert).
    Reasoning(usize, String),
    Chunk(usize, String),
    Usage(usize, Usage, CompletionParts),
    /// MID-STREAM, sobald der Endpunkt während des Streamings ein `usage`-Event
    /// liefert: `total_tokens` der laufenden Runde (serverbestätigt, kumulativ).
    /// Die Statusleiste zeigt es direkt als aktuelle Kontext-Größe an, statt
    /// erst beim Runden-Abschluss auf den finalen `Usage` zu warten.
    UsageUpdate(usize, u64),
    /// Eine erfolgreiche HTTP-Antwort des LLM-Endpunkts ist eingetroffen;
    /// `headers` sind deren Response-Header (Name, Wert). Die Session merkt
    /// sich den neuesten Satz für den `Alt+H`-Dialog.
    HttpHeaders(usize, Vec<(String, String)>),
    /// Ein Werkzeug beginnt zu laufen – mit voller Identität (tool_call_id,
    /// Funktionsname, rohe Argument-JSON) plus Anzeige-Label. Der Client öffnet
    /// daraus das `Tool`-Event im Event-Log; `ToolOutput`/`ToolEnd` finalisieren es.
    ToolStart {
        session: usize,
        tool_call_id: String,
        function_name: String,
        arguments: String,
        label: String,
    },
    /// Zwischenausgabe eines laufenden `run`-Werkzeugs (stdout+stderr, in
    /// Ankunftsreihenfolge) – die UI zeigt sie live in der Konsolen-Box, bevor
    /// das Werkzeug mit `ToolEnd` abgeschlossen wird.
    ToolOutput(usize, String),
    /// Ein Werkzeug ist beendet (Ergebnis im Session-Tool-Log).
    ToolEnd(usize, ToolActivity),
    /// Ein `run`-Werkzeugaufruf auf einem Local-Kanal braucht im
    /// `ConfirmEach`-Modus die Freigabe des Users. `label` ist die
    /// Kurzbeschreibung, `command` die Rohform für die Anzeige. Über `reply`
    /// muss mit `true` (ausführen) bzw. `false` (ablehnen) geantwortet werden –
    /// der Worker blockiert, bis die Antwort da ist (oder der Sender fällt weg).
    ExecConfirm(usize, String, String, ExecConfirmReply),
    /// Vor einem Retry (429/5xx/Netzwerkfehler): `retry_at` ist der Zeitpunkt,
    /// zu dem der nächste Versuch startet. Die UI zeigt in der Statuszeile eine
    /// Kurzfassung des Fehlers samt Countdown, `phase` bleibt `Streaming`.
    Retrying(usize, String, Instant),
    Done(usize),
    Error(usize, String),
    Cancelled(usize),
    /// Die Kontext-Kompaktierung läuft (separater Zusammenfassungs-Aufruf) –
    /// Statuszeile „⟲ Compacting context…“.
    Compacting(usize),
    /// Kompaktierung abgeschlossen. `content` ist die fertige
    /// Zusammenfassungs-User-Nachricht (inkl. Marker `[Compressed history – N
    /// earlier messages]`), die als `Archive`-Event an der Kompaktierungs-
    /// grenze eingefügt wird: die älteren Nachrichten bleiben im Speicher/der
    /// UI erhalten, werden aber von `api_messages` nicht mehr mitgesendet.
    /// `tokens` ist die Token-Zahl der Summary (`completion_tokens` des
    /// Kompaktierungs-Aufrufs, Fallback: Zeichen-Schätzung).
    Compacted(usize, String, u64),
    /// Hintergrund-Laden des Channel Builders abgeschlossen: (images_with_wd, container_info, worktrees)
    BuilderLoaded(
        Vec<(String, Option<String>)>,
        Option<crate::channel::builder::ContainerInfo>,
        Vec<crate::channel::builder::WorktreeEntry>,
    ),
    /// Modell-Liste von einem Provider abgerufen: `(modell_id, demand)`.
    ModelsRefreshed(Vec<(String, Option<u64>)>),
}

/// Token-Verbrauch der letzten Antwort (via `stream_options.include_usage`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Gecachte Tokens (Cache-Hit), falls der Endpunkt diese Info liefert.
    pub cached_tokens: Option<u64>,
}

/// Per-Sektion gemessene Completion-Tokens einer einzelnen HTTP-Antwort
/// (Runde). Wird beim SSE-Delta-Verarbeiten aus den usage-Inkrementen + den
/// Byte-Längen der einzelnen Bereiche abgeleitet (siehe
/// `http::RoundPartsAccumulator`); der letzte Bereich erhält den Rundungsrest,
/// damit die Summe exakt dem verteilten usage-Inkrement entspricht.
///
/// `completion_tokens` einer Runde werden so genau ihrer Quelle zugeordnet
/// (reasoning / content / je tool_call), statt sie nachträglich proportional
/// zur Zeichenlänge aufzuteilen. Tool-Kopfdeltas (id/name, 0 Argument-Bytes)
/// werden über die „aktive Sektion" trotzdem dem jeweiligen Tool-Call
/// zugerechnet. `prompt_tokens`/`total_tokens`/`cached_tokens` bleiben dagegen
/// kumulierte Endwerte und sind in `Usage` unverändert.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompletionParts {
    pub reasoning: u64,
    pub content: u64,
    /// Je Tool-Call der Runde, in der Reihenfolge der `tool_calls` der Antwort.
    pub tool_calls: Vec<u64>,
}

mod api;
mod compact;
mod helpers;
mod http;
mod ident;
mod tools_def;
mod tools_exec;
mod webfetch;
mod wire;
mod worker;

// Externe API: von app.rs / ui.rs genutzt
pub(crate) use compact::spawn_compact;
pub(crate) use http::shared_client;
pub(crate) use wire::{WireFunction, WireMessage, WireToolCall};
pub(crate) use worker::{spawn_user_run, spawn_worker};

// Test-Hilfsre-exports: nur für das Testmodul (tests.rs) dieses Moduls
#[cfg(test)]
pub(crate) use compact::{compact_chat_messages, looks_like_context_error, wire_compact_boundary};
#[cfg(test)]
pub(crate) use helpers::{
    civil_from_days, reasoning_contract_hint, server_error_summary, truncate, with_debug,
};
#[cfg(test)]
pub(crate) use http::{
    accumulate_sse_event, distribute_weights, parse_usage, retry_delay, RoundPartsAccumulator,
};
#[cfg(test)]
pub(crate) use tools_def::{apply_tool_delta, sanitize_arguments, tool_definitions, ToolCallAcc};
#[cfg(test)]
pub(crate) use wire::ensure_reasoning_for_tool_calls;

#[cfg(test)]
mod tests;
