use std::path::PathBuf;
use std::sync::mpsc;

use crate::channel::ChannelKind;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// LLM-Antwort wird gestreamt (open assistant wächst).
    WaitingForLLM,
    /// Werkzeug-Ausführung läuft (open tool wächst).
    WaitingForTool,
}

/// Gewählter Umgang mit `execute`-Prompts auf **Local**-Kanälen. Gilt für den
/// Rest des Programmlaufs: In `Ask` wird vor dem Absenden eine Bestätigung
/// eingeholt (Hinweis, dass lokale Ausführung gefährlich sein kann), `Trusted`
/// und `ConfirmEach` sind gemerkte Entscheidungen, die nicht erneut erfragt
/// werden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalExecMode {
    /// Noch keine Wahl getroffen: vor dem Absenden mit `execute` auf einem
    /// Local-Kanal erscheint ein Bestätigungsdialog.
    #[default]
    Ask,
    /// Der User übernimmt die Verantwortung für die Sicherheit (z. B. läuft
    /// alles in einer Sandbox) – es wird nie wieder gefragt.
    Trusted,
    /// Der Prompt wird abgeschickt, aber jeder `run`-Werkzeugaufruf wird
    /// einzeln per Dialog bestätigt.
    ConfirmEach,
}

/// Offener Bestätigungsdialog VOR dem Absenden mit `execute` auf einem
/// Local-Kanal („lokale Ausführung kann gefährlich sein“). `session` ist die
/// Session, deren Prompt abgeschickt werden soll; `cursor` wählt eine der drei
/// Optionen (0 verantworten/Sandbox, 1 einzeln bestätigen, 2 abbrechen).
pub struct PreSendConfirm {
    pub session: usize,
    pub cursor: usize,
}

/// Offene Bestätigung für EINEN `run`-Werkzeugaufruf auf einem Local-Kanal im
/// `ConfirmEach`-Modus. Der Worker blockiert, bis über `reply` geantwortet
/// wurde (`true` = ausführen, `false` = ablehnen).
pub struct ExecConfirm {
    /// Kurzbeschreibung des Aufrufs (z. B. `run cargo test`).
    pub label: String,
    /// Kommando samt Argumenten (Rohform) für die Anzeige.
    pub command: String,
    pub cursor: usize,
    pub reply: mpsc::Sender<bool>,
}

/// Einzelner Fund im Beenden-Bestätigungsdialog: ein Kanal samt seines
/// Containers und den (ungekürzten) Einzelbefunden.
pub(crate) struct StopConfirmEntry {
    /// Kanalname (Label des Kanals, zur Einordnung).
    pub channel: String,
    /// Podman-Containername – nur bei eigenen Run-Containern relevant, die
    /// beim Beenden gestoppt würden.
    pub container: Option<String>,
    /// Einzelne Befund-Zeilen (z. B. Container-Layer-Änderungen, Git-Änderungen).
    pub notes: Vec<String>,
}

/// Offener Bestätigungsdialog vor dem Beenden, wenn selbst gestartete
/// Run-Container (bzw. deren Arbeitskopien) noch **wesentliche, ungesicherte
/// Änderungen** enthalten und durch das Ende gestoppt würden. Statt die letzte
/// Session sofort zu beenden bzw. das Programm zu quitten, fragt aidev nach:
/// `cursor` 0 = trotzdem beenden (& Container stoppen), 1 = abbrechen
/// (Default, sicher).
pub struct StopConfirm {
    /// Gefundene Änderungen je betroffenem Kanal/Container.
    pub entries: Vec<StopConfirmEntry>,
    /// Anzahl weiterer Container mit Änderungen, die nicht mehr einzeln
    /// aufgeführt werden (Dialog-Überladung vermeiden). 0 = alle aufgeführt.
    pub more: usize,
    pub cursor: usize,
}

/// Zwischenspeicher für `/branch`-Daten solange ein Bestätigungsdialog offen ist.
pub(crate) struct BranchPending {
    pub channel_kind: ChannelKind,
    pub channel_root: PathBuf,
    pub history: crate::chat::Chat,
    pub branch_name: String,
    pub has_worktree: bool,
    /// Podman-Laufzeit-Parameter `(image, workdir, home)` des bestehenden
    /// Kanals – für die Erzeugung eines gleich konfigurierten Folge-Kanals
    /// (auch bei vom ChannelBuilder erzeugten Kanälen).
    pub podman_spec: Option<(String, String, String)>,
}
pub(crate) struct BranchConfirm {
    pub summary: String,
    pub cursor: usize,
    /// Options-Vektor: 4 Einträge (kein Worktree) oder 3 (Worktree vorhanden).
    pub options: Vec<&'static str>,
}
/// Phasen des Kanal-Schließens – egal ob aus dem Picker (Entf) oder beim
/// Schließen einer Session (Ctrl+D / /end): erst geprüft, ob eine aktive
/// Session auf dem Kanal arbeitet, dann Worktree und schließlich Container
/// (wie der Programm-Ende-Dialog `StopConfirm`, aber für genau einen Kanal) –
/// alles gekapselt in `close_channel_*` und identisch für beide Wege.
pub enum ChannelClosePhase {
    /// Eine aktive Session arbeitet noch auf dem Kanal – erst bestätigen.
    /// `cursor` 0 = trotzdem schließen, 1 = abbrechen (Default, sicher).
    ActiveConfirm { cursor: usize },
    /// Worktree hat uncommittete Änderungen (wie beim Session-Schließen).
    /// `cursor` 0 = Abbrechen, 1 = Worktree löschen, 2 = Behalten,
    /// 3 = Committen & löschen.
    Worktree {
        summary: String,
        cursor: usize,
        options: Vec<&'static str>,
    },
    /// Container hat wesentliche Änderungen (wie `StopConfirm`, ein Kanal).
    /// `cursor` 0 = Kanal schließen & Container stoppen, 1 = abbrechen (Default).
    Container { notes: Vec<String>, cursor: usize },
}

/// Woher der Kanal-Schließ-Vorgang kommt – steuert, was nach dem Aufräumen
/// passiert und ob der Kanal aus der Registry entfernt wird.
#[derive(PartialEq)]
pub enum CloseKind {
    /// Aus dem Channel-Picker (Entf): Kanal wird aus der Registry entfernt,
    /// danach wird der Picker neu geöffnet.
    Picker,
    /// Beim Schließen einer Session (Ctrl+D / /end): danach wird die Session
    /// beendet (`finish_close_session`).
    Session,
}

/// Offener Dialog zum Schließen eines Kanals (Entf im Picker ODER Ctrl+D/`/end`
/// einer Session) – dieselbe, gekapselte Aufräum-Logik für beide Wege.
pub struct ChannelClose {
    pub name: String,
    pub kind: CloseKind,
    pub phase: ChannelClosePhase,
}

/// Optionen für den Worktree-Teil des Kanal-Schließens – identisch zum
/// Session-Schließen (cancel zuerst, dann löschen/behalten/commit+delete).
pub const CHANNEL_CLOSE_WORKTREE_OPTIONS: &[&str] = &[
    "Cancel – keep channel open",
    "Continue – delete worktree & close channel",
    "Keep – keep worktree & close channel",
    "Commit & delete – create snapshot, then delete",
];

/// Offener Options-Dialog (Ctrl+O). `cursor` wählt die aktive Option.
pub struct OptionsDialog {
    pub cursor: usize,
}

/// Hilfsfunktion für Auswahl-Dialoge: liefert den neuen Cursor nach
/// Pfeil-hoch/-runter (bzw. j/k), begrenzt auf `max` (inklusiv).
pub(crate) fn step_cursor(down: bool, cursor: usize, max: usize) -> usize {
    if down {
        cursor.saturating_add(1).min(max)
    } else {
        cursor.saturating_sub(1)
    }
}
