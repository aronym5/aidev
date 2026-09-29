//! API-Shape-Abstraktion: **Chat Completions** (`/chat/completions`, `messages`)
//! vs **Responses API** (`/responses`, `input`).
//!
//! Beide sind Draht-inkompatible OpenAI-APIs; dieses Modul kümmert sich um
//! - die Shape-Bestimmung (gecacht pro Endpunkt, sonst Default Chat Completions
//!   + enger Fehler-Fallback),
//! - den Request-Body in beiden Formaten,
//! - das Lesen nicht-streamender Antworten in beiden Formaten.
//!
//! Die SSE-Stream-Verarbeitung je Shape liegt in `http.rs` (`stream_chat` /
//! `stream_responses`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::tools_def::{force_tool_definitions, tool_definitions};
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
// Shape-Bestimmung (gecacht pro Endpunkt, sonst Default Chat Completions)
// ---------------------------------------------------------------------------

/// Ergebnis der Shape-Erkennung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShapeInfo {
    pub(crate) shape: ApiShape,
    /// `true`, wenn die Shape durch einen erfolgreichen Request bestätigt
    /// wurde; `false`, wenn sie nur geraten (Default) ist.
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
/// 2. Kein Cache-Eintrag → Default **Chat Completions** (breiteste
///    Kompatibilität) – mit `determined = false`, damit bei einem
///    server-seitigen Fehler noch die andere Shape probiert werden kann.
pub(crate) fn resolve_shape_info(ep: &ResolvedEndpoint) -> ShapeInfo {
    let key = (ep.base_url.clone(), ep.api_model.clone());
    if let Some(info) = shape_cache().lock().ok().and_then(|m| m.get(&key).copied()) {
        return info;
    }
    let info = ShapeInfo {
        shape: ApiShape::ChatCompletions,
        determined: false,
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
    let info = ShapeInfo {
        shape,
        determined: true,
    };
    if let Ok(mut m) = shape_cache().lock() {
        m.insert(key, info);
    }
}

/// Soll bei einem fehlgeschlagenen Request die ANDERE Shape probiert werden?
///
/// **HARTE INVARIANTE:** Eine einmal *bestätigte* Shape (`determined == true` –
/// ein erfolgreicher Request hat sie in den Cache geschrieben) wird **nie**
/// wieder verändert, unabhängig von Statuscode und Fehlertext. Der Cache ist
/// der Ort, an dem wir etwas *wissen*; eine Fehlermeldung ist nur eine
/// Beobachtung, und ein Provider-Fehler (Free-Tier-Sperre, 500, Auth) sagt
/// nichts darüber, welche API der Endpunkt spricht. Ohne diese Regel hat ein
/// einzelner unpassender Fehler die Shape verdreht: der Zusammenfassungs-
/// Aufruf (non-streaming, ohne `tools`) bekam vom Free Tier
/// `403 {"type":"error",…}`, das Envelope wurde als "der Server will die
/// Responses-API" gelesen, und der Retry auf `/responses` endete in einem
/// namenlosen 500, der als Endpunkt-Problem daherkam.
///
/// Nur eine **geratene** Shape (kein Cache-Eintrag, Default Chat Completions)
/// darf einmalig die andere Shape probieren – und auch das nur bei einem
/// Signal, das wirklich die Form betrifft:
///
/// - **Eindeutiger Format-Hinweis** (`looks_like_wrong_api`): die Meldung
///   nennt die jeweils andere API, oder der Endpunkt lehnt den Pfad selbst ab
///   (404/405/415) und antwortet im Fehlerformat der anderen API.
/// - **Default ohne Signal + Server-Fehler (5xx)**: dann kann ein Server
///   dahinterstehen, der das gesendete Format nicht kennt. Bewusst NICHT bei
///   Auth-/Rate-Limit-/bereits-eindeutigen Fehlern.
pub(crate) fn should_try_other_shape(
    status: reqwest::StatusCode,
    raw: &str,
    shape: ApiShape,
    determined: bool,
) -> bool {
    if determined {
        return false; // bestätigt – die Shape bleibt, für immer
    }
    if looks_like_wrong_api(status, raw, shape) {
        return true;
    }
    status.is_server_error()
}

/// Erkennt an der Roh-Fehlermeldung eines HTTP-Fehlers, dass der Endpunkt die
/// ANDERE API-Shape erwartet. Zwei Signale, jeweils bewusst eng gefasst, um
/// echte Fehler (Auth, Free Tier, Schema, Rate-Limit …) NICHT als
/// Format-Probleme zu maskieren und keine doppelten Requests ohne Grund zu
/// auslösen:
///
/// - **textuelle Hinweise** (`input_text` / `responses`+`"input"` bzw.
///   `messages`+`chat/completions`) – die Meldung benennt die API, die
///   gemeint war. Stärkstes Signal, gilt bei jedem Status.
/// - **Responses-Fehler-Envelope `{"type":"error", …}` NUR bei Pfad-Fehlern
///   (404/405/415)**: Chat-Completions-Fehler tragen ihr `type` nicht auf
///   oberster Ebene, ein Top-Level `"type":"error"` stammt also mit hoher
///   Wahrscheinlichkeit von einem Responses-Server – aber nur, wenn die
///   Beschwerde den *Pfad* betrifft. Ein eingewickelter Provider-Fehler
///   (`FreeTierError`, generisches `Internal server error`) steckt in derselben
///   Hülle und sagt über die API-Form **nichts** aus. Ihn als Format-Problem zu
///   lesen schickt den Retry auf einen Pfad, den der Endpunkt für dieses Modell
///   gar nicht bedient (Live-Beleg: zen/`big-pickle` – Chat-Completions 403
///   `FreeTierError`, `/responses` 500 "Internal server error").
pub(crate) fn looks_like_wrong_api(status: reqwest::StatusCode, raw: &str, used: ApiShape) -> bool {
    let l = raw.to_lowercase();
    match used {
        ApiShape::ChatCompletions => {
            if l.contains("input_text") || (l.contains("responses") && l.contains("\"input\"")) {
                return true;
            }
            // Top-Level `type == "error"` → Responses-Envelope, aber nur als
            // Signal, wenn der Endpunkt selbst beanstandet wurde.
            is_path_error(status) && is_responses_error_envelope(raw)
        }
        ApiShape::Responses => {
            // Server will Chat Completions: spricht von `messages`.
            (l.contains("chat/completions") || l.contains("chat completions"))
                && l.contains("messages")
        }
    }
}

/// Client-Fehler, die den **Pfad** selbst betreffen (nicht die Anfrage):
/// 404 unbekannter Endpunkt, 405 falsche Methode, 415 falscher Medientyp.
fn is_path_error(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 404 | 405 | 415)
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

/// Welche Werkzeugdefinitionen ein Request anbietet.
///
/// Der Umfang ist bewusst als *Scope* modelliert und nicht als zwei Booleans –
/// die drei Fälle lassen sich so nicht verwechseln:
///
/// - [`None`](ToolOffer::None): gar kein `tools`-Feld.
/// - [`Session`](ToolOffer::Session): der Chat-Pfad – mit Kanal die volle,
///   permissions-gefilterte Menge, ohne Kanal nur `webfetch`, dazu die
///   `force_tools` als Dummies.
/// - [`ForceOnly`](ToolOffer::ForceOnly): die Kompaktierung – **ausschließlich**
///   die `force_tools` als Dummies. Weder Kanal-Zustand noch `webfetch`
///   beeinflussen das; ist `force_tools` für den Provider leer, entfällt das
///   Feld ganz. Zweck: Der Provider (zen/opencode) beantwortet die
///   Zusammenfassung nur, wenn `tools` nicht-leer ist, die Zusammenfassung
///   selbst soll aber keine echten Werkzeuge sehen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolOffer {
    None,
    Session {
        permission: Permission,
        has_channel: bool,
    },
    ForceOnly,
}

impl ToolOffer {
    /// Die Definitionen für diesen Scope – leer heißt: kein `tools`-Feld.
    fn definitions(self, ep: &ResolvedEndpoint) -> Vec<Value> {
        match self {
            ToolOffer::None => Vec::new(),
            ToolOffer::Session {
                permission,
                has_channel,
            } => tool_definitions(permission, &ep.force_tools, has_channel),
            ToolOffer::ForceOnly => force_tool_definitions(&ep.force_tools),
        }
    }
}

/// Baut URL + Body für eine Anfrage in der gewählten Shape.
/// `tools` bestimmt den Werkzeugumfang (siehe [`ToolOffer`]),
/// `max_output_tokens` ist die Obergrenze für die Länge einer Zusammenfassung.
pub(crate) fn build_body(
    ep: &ResolvedEndpoint,
    shape: ApiShape,
    msgs: &[WireMessage],
    tools: ToolOffer,
    stream: bool,
    max_output_tokens: Option<u64>,
) -> (String, Value) {
    let base = ep.base_url.trim_end_matches('/');
    let url = format!("{base}{}", shape.endpoint_path());
    let body = match shape {
        ApiShape::ChatCompletions => chat_body(ep, msgs, tools, stream, max_output_tokens),
        ApiShape::Responses => responses_body(ep, msgs, tools, stream, max_output_tokens),
    };
    (url, body)
}

/// Setzt `tools` nur, wenn der Scope welche liefert – ein leeres Array würde
/// vom Provider als „Werkzeuge angeboten, aber keine" gelesen und ist für den
/// Free-Tier-Nachweis wertlos.
fn put_tools(body: &mut Value, tools: ToolOffer, ep: &ResolvedEndpoint, to_responses: bool) {
    let defs = tools.definitions(ep);
    if defs.is_empty() {
        return;
    }
    let defs = if to_responses {
        into_responses_tools(defs)
    } else {
        defs
    };
    body["tools"] = json!(defs);
}

fn chat_body(
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    tools: ToolOffer,
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
    put_tools(&mut body, tools, ep, false);
    body
}

fn responses_body(
    ep: &ResolvedEndpoint,
    msgs: &[WireMessage],
    tools: ToolOffer,
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
    put_tools(&mut body, tools, ep, true);
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
            "tool" => {
                // Tool-Ergebnis (role „tool“): im Responses-Format wird es wie das
                // User-Pendant zum `function_call_output`-Item. Ohne diesen Arm
                // gingen Tool-Antworten verloren und der Endpunkt bekäme
                // `function_call`-Items ohne passendes Output → 400.
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
                // Thinking-Mode-Vertrag (analog zur Chat-Shape): assistant-Tool-
                // Call-Nachrichten müssen das `reasoning_content` der Runde
                // zurücktragen (auch leer) – sonst 400 beim Endpunkt. Einfache
                // Assistant-Antworten ohne Tool-Calls brauchen es nicht.
                if m.tool_calls.is_some() {
                    let reasoning = m.reasoning_content.clone().unwrap_or_default();
                    out.push(json!({
                        "role": "assistant",
                        "content": content,
                        "reasoning_content": reasoning,
                    }));
                } else {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
                if let Some(calls) = &m.tool_calls {
                    for tc in calls {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                }
            }
            // legacy "function"/unbekannte Rollen: nicht sendbar → überspringen.
            _ => {}
        }
    }
    out
}

/// Konvertiert Chat-förmige Tool-Definitionen in das Responses-Format.
fn into_responses_tools(chat_form: Vec<Value>) -> Vec<Value> {
    chat_form
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
///
/// Beim Responses-Streaming liegt das Objekt im `response.completed`-Event
/// verschachtelt unter `response.usage` (nicht auf Top-Level) – hier ebenfalls
/// abgedeckt, damit die live `UsageUpdate`-Meldung und die Token-Ableitung auch
/// im Responses-Protokoll greifen.
pub(crate) fn parse_usage(json: &Value) -> Option<Usage> {
    let usage = json
        .get("usage")
        .or_else(|| json.get("response").and_then(|r| r.get("usage")))?
        .as_object()?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{WireFunction, WireMessage, WireToolCall};

    fn wire(
        role: &str,
        content: Option<&str>,
        reasoning: Option<&str>,
        tool_calls: Option<Vec<WireToolCall>>,
        tool_call_id: Option<&str>,
    ) -> WireMessage {
        WireMessage {
            role: role.into(),
            content: content.map(str::to_string),
            reasoning_content: reasoning.map(str::to_string),
            tool_calls,
            tool_call_id: tool_call_id.map(str::to_string),
            num_tokens: None,
        }
    }
    fn call(id: &str, name: &str, args: &str) -> WireToolCall {
        WireToolCall {
            id: id.into(),
            ty: "function".into(),
            function: WireFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    #[test]
    fn responses_input_traegt_tool_output_und_reasoning_zurueck() {
        // Assistant-Tool-Call-Runde OHNE sichtbaren Text und OHNE reasoning:
        // der Thinking-Mode-Vertrag verlangt `reasoning_content` (leer), und
        // die Tool-Antworten (role „tool“) müssen als `function_call_output`
        // übertragen werden – sonst 400 / verlorene Tool-Ergebnisse.
        let msgs = vec![
            wire("user", Some("hallo"), None, None, None),
            wire(
                "assistant",
                None,
                None,
                Some(vec![call("call_1", "read", "{\"path\":\"x\"}")]),
                None,
            ),
            wire("tool", Some("[inhalt]"), None, None, Some("call_1")),
        ];
        let input = responses_input(&msgs);
        // user, assistant (mit reasoning_content), function_call, tool-output
        assert_eq!(input.len(), 4);
        let assistant = &input[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "", "leer zurücksenden");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "[inhalt]");
    }

    #[test]
    fn responses_input_laesst_reasoning_bei_reiner_antwort_weg() {
        let msgs = vec![
            wire("user", Some("hallo"), None, None, None),
            wire("assistant", Some("hi"), None, None, None),
        ];
        let input = responses_input(&msgs);
        assert_eq!(input.len(), 2);
        assert!(input[1].get("reasoning_content").is_none());
    }

    #[test]
    fn parse_usage_liest_verschachteltes_responses_usage() {
        // `response.completed`-Event: usage liegt unter `response.usage`
        // (Responses-Streaming), nicht auf Top-Level.
        let j = json!({"type":"response.completed","response":{
            "status":"completed",
            "usage":{
                "input_tokens":10,
                "output_tokens":4,
                "total_tokens":14,
                "input_tokens_details":{"cached_tokens":3}
            }
        }});
        let u = parse_usage(&j).expect("usage vorhanden");
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 4);
        assert_eq!(u.total_tokens, 14);
        assert_eq!(u.cached_tokens, Some(3));

        // Top-Level usage (Chat/nicht-streamend) funktioniert weiterhin.
        let chat = json!({"usage":{
            "prompt_tokens":10,
            "completion_tokens":4,
            "total_tokens":14,
            "prompt_tokens_details":{"cached_tokens":5}
        }});
        let u2 = parse_usage(&chat).expect("usage vorhanden");
        assert_eq!(u2.prompt_tokens, 10);
        assert_eq!(u2.completion_tokens, 4);
        assert_eq!(u2.total_tokens, 14);
        assert_eq!(u2.cached_tokens, Some(5));

        // Ohne usage → None (kein Fehl-/Null-Ereignis).
        assert_eq!(
            parse_usage(&json!({"type":"response.output_text.delta"})),
            None
        );
    }

    // ── Shape-Wechsel: harte Invariante bei bestätigter Shape ─────────────

    /// Body, den ein Free-Tier-Gateway in die Responses-Fehlerhülle packt
    /// (Live-Beleg zen/`big-pickle`, 403 auf `/chat/completions`).
    const FREE_TIER_403: &str = r#"{"type":"error","error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode"}}"#;
    /// Der namenlose 500er, den derselbe Endpunkt auf `/responses` liefert.
    const GENERIC_500: &str =
        r#"{"type":"error","error":{"type":"error","message":"Internal server error"}}"#;
    /// Ein Responses-Server, der den Chat-Pfad nicht kennt (404 + Envelope).
    const RESPONSES_ONLY_404: &str = r#"{"type":"error","error":{"message":"unknown endpoint"}}"#;

    #[test]
    fn bestaetigte_shape_wird_nie_gewechselt() {
        use reqwest::StatusCode;
        // HARTE REGEL: `determined == true` → kein Wechsel, egal was der Server
        // sagt. Genau dieser Fehlpfad hat den Zusammenfassungs-Aufruf von der
        // funktionierenden Chat-Shape auf `/responses` umgeleitet, wo der
        // Endpunkt das Modell gar nicht bedient (500 ohne Aussage).
        for (status, raw) in [
            (StatusCode::FORBIDDEN, FREE_TIER_403),
            (StatusCode::INTERNAL_SERVER_ERROR, GENERIC_500),
            (StatusCode::NOT_FOUND, RESPONSES_ONLY_404),
            (
                StatusCode::BAD_REQUEST,
                r#"{"type":"error","error":{"message":"input_text expected"}}"#,
            ),
        ] {
            assert!(
                !should_try_other_shape(status, raw, ApiShape::ChatCompletions, true),
                "bestätigte Chat-Shape darf bei {status} nicht wechseln: {raw}"
            );
            assert!(
                !should_try_other_shape(status, raw, ApiShape::Responses, true),
                "bestätigte Responses-Shape darf bei {status} nicht wechseln: {raw}"
            );
        }
    }

    #[test]
    fn geraetete_shape_wechselt_nur_echten_format_signalen() {
        use reqwest::StatusCode;
        // Provider-Fehler in der Responses-Hülle: KEIN Format-Signal → bei
        // geratener Shape kein Wechsel (403 ist kein 5xx, 500 bleibt 5xx).
        assert!(!should_try_other_shape(
            StatusCode::FORBIDDEN,
            FREE_TIER_403,
            ApiShape::ChatCompletions,
            false
        ));
        // Pfad-Fehler MIT Envelope → starker Hinweis, dass ein Responses-Server
        // dahintersteckt → Wechsel ist richtig.
        assert!(should_try_other_shape(
            StatusCode::NOT_FOUND,
            RESPONSES_ONLY_404,
            ApiShape::ChatCompletions,
            false
        ));
        // Textueller Hinweis wirkt bei jedem Status.
        assert!(should_try_other_shape(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"unknown field input_text"}}"#,
            ApiShape::ChatCompletions,
            false
        ));
        // Gegenrichtung: Responses-Server will `messages`.
        assert!(should_try_other_shape(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"unknown path /responses, use /v1/chat/completions with messages"}}"#,
            ApiShape::Responses,
            false
        ));
        // Generischer 5xx bei gerateter Shape → einmal probieren (alt, gewollt).
        assert!(should_try_other_shape(
            StatusCode::INTERNAL_SERVER_ERROR,
            GENERIC_500,
            ApiShape::ChatCompletions,
            false
        ));
        // 403 ohne Formbezug bleibt auch bei gerateter Shape unangetastet.
        assert!(!should_try_other_shape(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"Invalid API key"}}"#,
            ApiShape::ChatCompletions,
            false
        ));
    }
}
