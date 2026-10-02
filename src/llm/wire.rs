//! OpenAI Wire-Format: WireMessage, Konvertierung, Thinking-Mode-Vertrag.

use serde::Serialize;

use crate::llm::estimate_tokens;

/// Token-Bewertung einer Wire-Nachricht, **je Bestandteil**: pro Teil die
/// bestätigte (servergemessene, aus dem Event-Log abgeleitete) Zahl – sonst
/// die Zeichenschätzung über genau diesen Text. Damit ist jede Nachricht
/// vollständig bewertet: es wird kein Bestandteil weggelassen, weil er keine
/// eigene Messung hat. Das war die alte Lücke: Reasoning und Tool-Aufrufe
/// zählten im Schätzzweig gar nicht, wodurch Nachrichten mit leuchtendem Text
/// und großem Reasoning (bzw. reine Tool-Calls) systematisch zu klein
/// gewertet wurden – und damit auch der Kompaktierungs-Schnitt, der auf
/// diesen Zahlen entscheidet.
///
/// Nicht serialisiert (`skip`): die Zahlen gehören der lokalen Planung
/// (Kompaktierung/Protokoll), nicht dem Request an den Server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub(crate) struct WireTokens {
    /// Reasoning-Anteil einer `assistant`-Nachricht.
    pub(crate) reasoning: u64,
    /// Textanteil: `assistant`-Text, `user`-Eingabe, Zusammenfassung.
    pub(crate) content: u64,
    /// Der vom Modell erzeugte Tool-Aufruf (Funktionsname + Argumente).
    /// Steht im Request am `assistant` (`tool_calls`), wird hier aber an der
    /// zugehörigen `tool`-Nachricht verbucht – wie in der Übersicht, wo die
    /// Werkzeugzeile „Aufruf + Ergebnis“ in einer Zahl zeigt. So zählt er
    /// genau einmal und steht beim richtigen Thread.
    pub(crate) call: u64,
    /// Ergebnis/Antwort einer `tool`-Nachricht.
    pub(crate) output: u64,
}

impl WireTokens {
    /// Bestätigte Zahl, sonst die Schätzung über `text`. Leerer Text kostet 0 –
    /// statt pauschal „1 T pro Nachricht“ (das waren die 1-T-Zeilen reiner
    /// Tool-Call-Anker in `overview.txt`).
    pub(crate) fn part(measured: u64, text: &str) -> u64 {
        if measured > 0 {
            measured
        } else if text.is_empty() {
            0
        } else {
            estimate_tokens(text)
        }
    }

    /// Summe aller Bestandteile – die Tokenzahl der Nachricht.
    pub(crate) fn total(&self) -> u64 {
        self.reasoning + self.content + self.call + self.output
    }
}

/// Nachricht im Wire-Format.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub(crate) struct WireMessage {
    pub(crate) role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    /// Token-Bewertung je Bestandteil: bestätigte Zahl aus dem Event-Log, sonst
    /// Schätzung (siehe `WireTokens`). Existiert nur für die Kompaktierungs-
    /// Planung und das Protokoll, wird NICHT an den Server serialisiert
    /// (`skip`). Jeder Bestandteil ist immer bewertet – nichts wird
    /// weggelassen.
    #[serde(skip)]
    pub(crate) tokens: WireTokens,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) ty: String,
    pub(crate) function: WireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct WireFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Thinking-Mode-Vertrag: assistant-mit-tool_calls MUSS reasoning_content tragen.
pub(crate) fn ensure_reasoning_for_tool_calls(msgs: &[WireMessage]) -> Vec<WireMessage> {
    msgs.iter()
        .map(|m| {
            if m.role == "assistant" && m.tool_calls.is_some() && m.reasoning_content.is_none() {
                WireMessage {
                    reasoning_content: Some(String::new()),
                    ..m.clone()
                }
            } else {
                m.clone()
            }
        })
        .collect()
}
