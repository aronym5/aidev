//! API-Shape-Abstraktion: **Chat Completions** (`/chat/completions`, `messages`)
//! vs **Responses API** (`/responses`, `input`).
//!
//! Beide sind Draht-inkompatible OpenAI-APIs; dieses Modul kümmert sich um
//! - das Erkennen der richtigen Shape (Modell-Endpunkt-Probe + enger Fallback),
//! - den Request-Body in beiden Formaten,
//! - das Lesen nicht-streamender Antworten in beiden Formaten.
//!
//! Die SSE-Stream-Verarbeitung je Shape liegt in `http.rs` (`stream_chat` /
//! `stream_responses`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::tools_def::tool_definitions;
use super::wire::{ensure_reasoning_for_tool_calls, WireMessage};
use super::Usage;
use crate::config::ResolvedEndpoint;
use crate::perm::Permission;

/// Welche OpenAI-API ein Endpunkt spricht.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiShape {
    /// `POST /chat/completions` – breiteste Kompatibilität (OSS, Azure, …).
    ChatCompletions,
    /// `POST /responses` – neuere OpenAI-API (`input`/`input_text`).
    Responses,
}

impl ApiShape {
    pub(crate) fn endpoint_path(self) -> &'static str {
        match self {
            ApiShape::ChatCompletions => "/chat/completions",
            ApiShape::Responses => "/responses",
        }
    }

    pub(crate) fn flipped(self) -> ApiShape {
        match self {
            ApiShape::ChatCompletions => ApiShape::Responses,
            ApiShape::Responses => ApiShape::ChatCompletions,
        }
    }
}

// ---------------------------------------------------------------------------
// Shape-Erkennung (Probe über den Modell-Endpunkt, gecacht pro Endpunkt)
// ---------------------------------------------------------------------------

/// Ergebnis der Shape-Erkennung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShapeInfo {
    pub(crate) shape: ApiShape,
    /// `true`, wenn die Shape aus Modell-Metadaten (oder einem bestätigten
    /// Request) stammt; `false`, wenn sie nur ein Default ohne Signal ist.
    pub(crate) determined: bool,
}

/// Cache: `(base_url, api_model) → ShapeInfo`.
static SHAPE_CACHE: OnceLock<Mutex<HashMap<(String, String), ShapeInfo>>> = OnceLock::new();

fn shape_cache() -> &'static Mutex<HashMap<(String, String), ShapeInfo>> {
    SHAPE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ermittelt die API-Shape für einen Endpunkt (gecacht):
///
/// 1. Cache-Treffer → sofort.
/// 2. Probe `GET {base}/models/{model}` (bzw. `GET {base}/models`): Liefert der
///    Server explizit `supports_responses` (bzw. `supported_uses` ⇒ responses)
///    UND NICHT zugleich Chat Completions, wird Responses gewählt.
/// 3. Alles andere (Feld fehlt / Probe schlägt fehl / OSS-Server) → Default
///    Chat Completions (breiteste Kompatibilität) – mit `determined = false`,
///    damit bei einem server-seitigen Fehler noch die andere Shape probiert
///    werden kann.
pub(crate) fn resolve_shape_info(
    client: &reqwest::blocking::Client,
    ep: &ResolvedEndpoint,
) -> ShapeInfo {
    let key = (ep.base_url.clone(), ep.api_model.clone());
    if let Some(info) = shape_cache().lock().ok().and_then(|m| m.get(&key).copied()) {
        return info;
    }
    let info = match probe_shape(client, ep) {
        Some(shape) => ShapeInfo { shape, determined: true },
        None => ShapeInfo {
            shape: ApiShape::ChatCompletions,
            determined: false,
        },
    };
    if let Ok(mut m) = shape_cache().lock() {
        m.insert(key, info);
    }
    info
}

/// Merkt sich die Shape, die tatsächlich funktioniert hat (nach erfolgreichem
/// Request in der umgeschalteten Shape), als **bestimmt** – nachfolgende
/// Requests gehen dann direkt in dieses Format, ohne erneut zu raten.
pub(crate) fn remember_shape(ep: &ResolvedEndpoint, shape: ApiShape) {
    let key = (ep.base_url.clone(), ep.api_model.clone());
    let info = ShapeInfo { shape, determined: true };
    if let Ok(mut m) = shape_cache().lock() {
        m.insert(key, info);
    }
}

/// Soll bei einem fehlgeschlagenen Request die ANDERE Shape probiert werden?
///
/// Zwei Fälle:
/// - **Eindeutiger Format-Fehler** (`looks_like_wrong_api`): explizites Signal
///   des Servers, dass er das andere Format will.
/// - **Default ohne Signal + Server-Fehler (5xx)**: Wir haben nur geraten
///   (keine Modell-Metadaten) und der Server antwortet generisch – dann kann
///   ein Responses-/Chat-Server dahinterstehen, der das gesendete Format nicht
///   kennt. Ein Versuch in der anderen Shape ist ein günstiger, einmaliger
///   Test. Bewusst NICHT bei Auth-/Rate-Limit-/bereits-eindeutigen Fehlern.
pub(crate) fn should_try_other_shape(
    status: reqwest::StatusCode,
    raw: &str,
    shape: ApiShape,
    determined: bool,
) -> bool {
    if looks_like_wrong_api(raw, shape) {
        return true;
    }
    !determined && status.is_server_error()
}

fn probe_shape(client: &reqwest::blocking::Client, ep: &ResolvedEndpoint) -> Option<ApiShape> {
    let base = ep.base_url.trim_end_matches('/');

    // 1) Single-Model-Abruf ist am genauesten.
    let single_url = format!("{base}/models/{}", ep.api_model);
    if let Some(v) = probe_one(client, ep, &single_url) {
        return Some(v);
    }
    // 2) Fallback: Modell-Liste nach unserem Modell durchsuchen.
    let list_url = format!("{base}/models");
    if let Some(v) = probe_list(client, ep, &list_url) {
        return Some(v);
    }
    None
}

fn probe_one(client: &reqwest::blocking::Client, ep: &ResolvedEndpoint, url: &str) -> Option<ApiShape> {
    let resp = client
        .get(url)
        .bearer_auth(&ep.api_key)
        .header("user-agent", &ep.user_agent)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().ok()?;
    decide_from_meta(&v)
}

fn probe_list(client: &reqwest::blocking::Client, ep: &ResolvedEndpoint, url: &str) -> Option<ApiShape> {
    let resp = client
        .get(url)
        .bearer_auth(&ep.api_key)
        .header("user-agent", &ep.user_agent)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().ok()?;
    let data = v.get("data").and_then(|d| d.as_array())?;
    let entry = data
        .iter()
        .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(ep.api_model.as_str()))?;
    decide_from_meta(entry)
}

/// Konservativ: Nur dann Responses, wenn die Modell-Metadaten das **explizit**
/// sagen UND Chat Completions nicht ausdrücklich angegeben sind. Liefert
/// `None`, wenn die Antwort KEINE Fähigkeitsangaben enthält (z. B. ein
/// schlichter Modell-Listen-Eintrag ohne `supports_*`/`supported_uses`) – das
/// bedeutet „unbekannt“ und aktiviert den Unknown-Fallback (5xx → andere API
/// probieren), statt Chat als gesichert anzunehmen.
fn decide_from_meta(v: &Value) -> Option<ApiShape> {
    let supports_responses = v.get("supports_responses").and_then(|x| x.as_bool());
    let supports_chat = v.get("supports_chat_completions").and_then(|x| x.as_bool());
    let has_uses = v.get("supported_uses").is_some();
    let used: Vec<&str> = v
        .get("supported_uses")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
        .unwrap_or_default();

    // Keine einzige Fähigkeits-Angabe → unbekannt (kein eindeutiges Signal).
    if supports_responses.is_none() && supports_chat.is_none() && !has_uses {
        return None;
    }

    let uses_responses = used.contains(&"responses");
    let uses_chat = used.iter().any(|u| *u == "chat.completions" || *u == "chat_completions");

    if supports_responses == Some(true) && supports_chat == Some(false) {
        return Some(ApiShape::Responses);
    }
    if uses_responses && !uses_chat {
        return Some(ApiShape::Responses);
    }
    // Eindeutig Chat genannt (oder beides) → Chat Completions (Default).
    Some(ApiShape::ChatCompletions)
}

/// Erkennt an der Roh-Fehlermeldung eines HTTP-Fehlers, dass der Endpunkt die
/// ANDERE API-Shape erwartet. Zwei Signale, jeweils bewusst eng gefasst, um
/// echte Fehler (Auth, Schema, Rate-Limit …) NICHT als Format-Probleme zu
/// maskieren und keine doppelten Requests ohne Grund auszulösen:
///
/// - textuelle Hinweise (`input_text` / `responses`+`"input"` bzw. `messages`),
/// - das **Responses-Fehler-Envelope** `{"type":"error", …}`: Chat-Completions-
///   Fehler tragen ihr `type` NICHT auf oberster Ebene – ein Top-Level
///   `"type":"error"` (z. B. bei einem generischen 500) stammt also mit hoher
///   Wahrscheinlichkeit von einem Responses-Server und ist ein starkes Signal,
///   das Chat-Pendant zu probieren.
pub(crate) fn looks_like_wrong_api(raw: &str, used: ApiShape) -> bool {
    let l = raw.to_lowercase();
    match used {
        ApiShape::ChatCompletions => {
            if l.contains("input_text") || (l.contains("responses") && l.contains("\"input\"")) {
                return true;
            }
            // Top-Level `type == "error"` in der Antwort-JSON → Responses-Envelope.
            is_responses_error_envelope(raw)
        }
        ApiShape::Responses => {
            // Server will Chat Completions: spricht von `messages`.
            (l.contains("chat/completions") || l.contains("chat completions"))
                && l.contains("messages")
        }
    }
}

/// `true`, wenn die Roh-Antwort JSON mit Top-Level-Feld `"type":"error"` ist
/// (Responses-API-Fehlerformat). Unkritisch bei unparsbarem/nicht-objektförmigem
/// Body (`false`).
fn is_responses_error_envelope(raw: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    v.get("type").and_then(|t| t.as_str()) == Some("error")
}

// ---------------------------------------------------------------------------
// Request-Body in beiden Formaten
// ---------------------------------------------------------------------------

/// Baut URL + Body für eine Anfrage in der gewählten Shape.
/// `max_output_tokens` ist nur für nicht-streamende (Kompaktierungs-)Aufrufe.
pub(crate) fn build_body(
    ep: &ResolvedEndpoint,
    shape: ApiShape,
    msgs: &[WireMessage],
    with_tools: bool,
    permission: Permission,
    stream: bool,
    max_output_tokens: Option<u64>,
) -> (String, Value) {
    let base = ep.base_url.trim_end_matches('/');
    let url = format!("{base}{}", shape.endpoint_path());
    let body = match shape {
        ApiShape::ChatCompletions => chat_body(ep, msgs, with_tools, permission, stream, max_output_tokens),
        ApiShape::Responses => responses_body(ep, msgs, with_tools, permission, stream, max_output_tokens),
    };
    (url, body)
}

fn chat_body(
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    with_tools: bool,
    permission: Permission,
    stream: bool,
    max_output_tokens: Option<u64>,
) -> Value {
    let mut body = json!({
        "model": ep.api_model,
        "stream": stream,
        // Thinking-Mode-Vertrag: assistant-Tool-Call-Nachrichten tragen
        // `reasoning_content` (auch leer) – sonst 400.
        "messages": ensure_reasoning_for_tool_calls(msgs),
    });
    if let Some(toks) = max_output_tokens {
        body["max_tokens"] = json!(toks);
    }
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if with_tools {
        body["tools"] = json!(tool_definitions(permission));
    }
    body
}

fn responses_body(
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    with_tools: bool,
    permission: Permission,
    stream: bool,
    max_output_tokens: Option<u64>,
) -> Value {
    let mut body = json!({
        "model": ep.api_model,
        "stream": stream,
        "input": responses_input(msgs),
    });
    if let Some(toks) = max_output_tokens {
        // Responses nennt das Feld `max_output_tokens` (statt `max_tokens`).
        body["max_output_tokens"] = json!(toks);
    }
    if with_tools {
        body["tools"] = json!(responses_tools(permission));
    }
    body
}

/// Wandelt `WireMessage`s in Responses-`input`-Items.
fn responses_input(msgs: &[WireMessage]) -> Vec<Value> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m.role.as_str() {
            "system" | "developer" => {
                if let Some(c) = &m.content {
                    out.push(json!({
                        "role": m.role,
                        "content": [{"type": "input_text", "text": c}],
                    }));
                }
            }
            "user" if m.tool_call_id.is_some() => {
                // Tool-Ergebnis → eigenes Item mit passender `call_id`.
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "output": m.content.clone().unwrap_or_default(),
                }));
            }
            "user" => {
                if let Some(c) = &m.content {
                    out.push(json!({
                        "role": "user",
                        "content": [{"type": "input_text", "text": c}],
                    }));
                }
            }
            "assistant" => {
                let content: Value = m
                    .content
                    .as_ref()
                    .map(|c| json!([{ "type": "output_text", "text": c }]))
                    .unwrap_or_else(|| json!([]));
                if let Some(calls) = &m.tool_calls {
                    // Erst die Assistant-Message, dann je ein `function_call`-Item.
                    out.push(json!({ "role": "assistant", "content": content }));
                    for tc in calls {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                } else {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
            }
            // legacy "function"/unbekannte Rollen: nicht sendbar → überspringen.
            _ => {}
        }
    }
    out
}

/// Konvertiert die (Chat-förmigen) Tool-Definitionen in das Responses-Format.
fn responses_tools(permission: Permission) -> Vec<Value> {
    tool_definitions(permission)
        .into_iter()
        .map(|t| {
            let f = &t["function"];
            json!({
                "type": "function",
                "name": f["name"],
                "description": f["description"],
                "parameters": f["parameters"],
                "strict": false,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Nicht-streamende Antworten (Kompaktierung)
// ---------------------------------------------------------------------------

/// Extrahiert den Text aus einer nicht-streamenden Antwort beider Shapes.
pub(crate) fn content_from_nonstream(shape: ApiShape, v: &Value) -> Option<String> {
    match shape {
        ApiShape::ChatCompletions => chat_content(v),
        ApiShape::Responses => responses_text(v),
    }
}

/// Extrahiert eine Fehlermeldung aus einer Antwort beider Shapes (falls
/// vorhanden) – für `{"error": …}` (Chat) bzw. `{"error": …}`/failed (Responses).
pub(crate) fn _error_from_nonstream(_v: &Value) -> Option<String> {
    None
}

fn chat_content(v: &Value) -> Option<String> {
    let content = v.get("choices")?.get(0)?.get("message")?.get("content")?;
    match content {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let mut buf = String::new();
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                        buf.push_str(t);
                    }
                }
            }
            (!buf.is_empty()).then_some(buf)
        }
        _ => None,
    }
}

/// Aggregierter Antworttext der Responses API: `output_text` (falls gesetzt)
/// oder über die `output`-Items summieren (message-Items mit output_text-Parts).
pub(crate) fn responses_text(v: &Value) -> Option<String> {
    if let Some(t) = v.get("output_text").and_then(|x| x.as_str()) {
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let mut buf = String::new();
    if let Some(out) = v.get("output").and_then(|x| x.as_array()) {
        for item in out {
            if item.get("type").and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                for part in content {
                    if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                        if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                            buf.push_str(t);
                        }
                    }
                }
            }
        }
    }
    (!buf.is_empty()).then_some(buf)
}

// ---------------------------------------------------------------------------
// Usage (beide Formate)
// ---------------------------------------------------------------------------

/// Liest `usage` aus Chat- (`prompt_tokens`/`completion_tokens`) und
/// Responses-Antworten (`input_tokens`/`output_tokens`). `total_tokens` ist in
/// beiden enthalten; fehlt es, wird aus input+output gerechnet.
pub(crate) fn parse_usage(json: &Value) -> Option<Usage> {
    let usage = json.get("usage")?.as_object()?;

    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let prompt_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("input_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);
    let completion_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| usage.get("output_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0);

    if total_tokens == 0 && prompt_tokens == 0 && completion_tokens == 0 {
        return None;
    }
    let total_tokens = if total_tokens > 0 {
        total_tokens
    } else {
        prompt_tokens + completion_tokens
    };

    let cached_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
        });

    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens,
    })
}

