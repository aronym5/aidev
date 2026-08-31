//! OpenAI Wire-Format: WireMessage, Konvertierung, Thinking-Mode-Vertrag.

use serde::Serialize;

/// Nachricht im Wire-Format.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

