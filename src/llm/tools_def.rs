//! Tool-Definitionen, Delta-Akkumulation, Step-Enum.

use serde_json::{json, Value};

use super::{CompletionParts, Usage};
use crate::perm::Permission;

// ---------------------------------------------------------------------------
// Werkzeug-Definitionen (OpenAI function-calling)
// ---------------------------------------------------------------------------

pub(crate) const TOOL_RESULT_CAP: usize = 8000;
/// Anzeige-Limit für die Ausgabe in der Run-Konsolen-Box.
pub(crate) const RUN_CONSOLE_CAP: usize = 4000;
/// Maximale Zeilenzahl, die `read` pro Aufruf liefert (Decke für große Fenster,
/// wie bei anderen Harnesses). Größere Dateien liest das Modell mit
/// offset/limit als Fenster.
pub(crate) const MAX_READ_LINES: usize = 2000;
/// Maximale Länge einer einzelnen von `read` gelieferten Zeile. Längere Zeilen
/// (z. B. Log-/Minified-Zeilen) werden an dieser Grenze gekürzt und markiert,
/// damit der Output auch bei pathologischen Zeilen begrenzt bleibt.
pub(crate) const MAX_READ_LINE_CHARS: usize = 4096;
/// Decke für Pfade, die das `glob`-Werkzeug liefert; darüber erscheint ein
/// „weitere …“-Hinweis.
pub(crate) const GLOB_RESULT_CAP: usize = 200;
/// Max. Dateien im Trefferzahl-Modus des `grep`-Werkzeugs (content = 0);
/// darüber erscheint ein „weitere …“-Hinweis.
pub(crate) const GREP_FILE_CAP: usize = 200;
/// Maximale Kontextzeilen je Treffer (`grep`-Parameter `content`).
pub(crate) const MAX_GREP_CONTEXT: usize = 10;

/// User-Agent für das `webfetch`-Werkzeug (externer Webseiten-Abruf ohne
/// Provider-Kontext). Die eigentlichen LLM-API-Requests (Chat + Kompaktierung)
/// verwenden den provider-spezifischen User-Agent aus der Config
/// (`ResolvedEndpoint::user_agent`).
pub(crate) const USER_AGENT: &str =
    "opencode/1.18.16 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

/// Beschreibt die Werkzeuge, die das Modell über den Kanal aufrufen darf.
/// Nur gesendet, wenn die Session einen Kanal gebunden hat; gefiltert nach der
/// gewählten Berechtigung (`Permission::tools`) – so bekommt das Modell bei
/// `read` nur `grep`/`read`/`glob`/`webfetch` zu sehen.
pub(crate) fn tool_definitions(permission: Permission) -> Vec<Value> {
    vec![
        tool(
            "grep",
            "Full-text search in the project via ripgrep. With content>0 (default 1) it adds N context lines around each match as raw match output. content=0 returns per file only the match count (\"path:count\"). Use include to filter by file pattern.",
            json!({"type":"object","properties":{
                "pattern":{"type":"string","description":"Search pattern (ripgrep regex)."},
                "path":{"type":"string","description":"File or directory to search, relative to the working directory (default: working directory, search recursively)."},
                "include":{"type":"string","description":"Glob pattern to filter files (e.g. \"*.rs\", \"*.{ts,tsx}\"). Optional."},
                "content":{"type":"integer","description":"Context lines per match (0-10, default 1). 0 = match counts only."}
            },"required":["pattern"]}),
        ),
        tool(
            "read",
            "Reads a file relative to the working directory. Lines are 1-based and always prefixed with their line number. Without offset/limit it reads the whole file (up to 2000 lines); for larger files use offset/limit to page through.",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "offset":{"type":"integer","description":"1-based start line (optional, default 1 = file start)."},
                "limit":{"type":"integer","description":"Number of lines to read (optional, default to file end; max 2000)."}
            },"required":["path"]}),
        ),
        tool(
            "glob",
            "Finds files/directories via a glob pattern. Patterns without \"/\" match the file name at any depth (e.g. \"*.rs\"); with \"/\" the path is matched relative to the working directory (\"*\" also crosses \"/\", e.g. \"src/**/*.rs\"). Returns sorted paths relative to the working directory; directories end with \"/\".",
            json!({"type":"object","properties":{
                "pattern":{"type":"string","description":"Glob pattern, e.g. \"*.rs\" or \"src/**/*.ts\"."}
            },"required":["pattern"]}),
        ),
        tool(
            "webfetch",
            "Fetches a URL and returns its content as text (HTML is converted to readable text; very large pages are truncated). Scheme-less URLs count as https; http is upgraded to https. In prompt, briefly describe what you expect from the page - both appear in the history for the user.",
            json!({"type":"object","properties":{
                "url":{"type":"string","description":"The URL to fetch."},
                "prompt":{"type":"string","description":"What you expect from/inspect on the page (brief)."}
            },"required":["url","prompt"]}),
        ),
        tool(
            "edit",
            "Replaces an exact text section in an existing file relative to the working directory. `old` must match the file exactly (including indentation/whitespace); it may span multiple lines. By default only the first occurrence is replaced; use replace_all=true for all. Re-read afterwards to check context.",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "old":{"type":"string","description":"Exact text to replace (incl. indentation), may span multiple lines."},
                "new":{"type":"string","description":"Replacement text (may span multiple lines)."},
                "replace_all":{"type":"boolean","description":"Optional: replace all occurrences instead of just the first."}
            },"required":["path","old","new"]}),
        ),
        tool(
            "write",
            "Writes a file relative to the working directory (creates missing directories).",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "content":{"type":"string"}
            },"required":["path","content"]}),
        ),
        tool(
            "run",
            "Runs a shell command in your working directory with full shell (bash if available, else sh) evaluation (pipes, redirections, variables, chaining like \"&&\"/\";\" and logic).",
            json!({"type":"object","properties":{
                "command":{"type":"string","description":"Full shell expression, e.g. \"cargo build -j2 && cargo test\"."}
            },"required":["command"]}),
        ),
    ]
    .into_iter()
    .filter(|t| {
        t["function"]["name"]
            .as_str()
            .is_some_and(|name| permission.allows(name))
    })
    .collect()
}

pub(crate) fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
}

/// Akkumulierter Werkzeug-Aufruf aus den `delta.tool_calls`-Fragmenten.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ToolCallAcc {
    pub(crate) index: usize,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Erfasst `delta.tool_calls`-Fragmente (index-weise).
pub(crate) fn apply_tool_delta(accs: &mut Vec<ToolCallAcc>, delta: &Value) {
    let Some(arr) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
        return;
    };
    for d in arr {
        let index = d
            .get("index")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);
        let slot = match accs.iter_mut().find(|a| a.index == index) {
            Some(s) => s,
            None => {
                accs.push(ToolCallAcc {
                    index,
                    ..ToolCallAcc::default()
                });
                accs.iter_mut()
                    .find(|a| a.index == index)
                    .expect("just inserted")
            }
        };
        if let Some(id) = d.get("id").and_then(|v| v.as_str()) {
            if slot.id.is_empty() {
                slot.id = id.to_string();
            }
        }
        if let Some(f) = d.get("function") {
            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                if slot.name.is_empty() {
                    slot.name = name.to_string();
                }
            }
            if let Some(args) = f.get("arguments").and_then(|v| v.as_str()) {
                slot.arguments.push_str(args);
            }
        }
    }
}

/// Normalisiert die `function.arguments` eines Tool-Calls zu gültigem JSON.
pub(crate) fn sanitize_arguments(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "{}".to_string();
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }
    "{}".to_string()
}

/// Fertige Werkzeug-Aufrufe einer Antwort-Runde.
#[derive(Debug, Clone)]
pub(crate) struct ToolInvocation {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Ergebnis einer einzelnen HTTP-Runde im Konversations-Loop.
pub(crate) enum Step {
    Final {
        usage: Option<Usage>,
        parts: CompletionParts,
    },
    Tools {
        assistant: super::wire::WireMessage,
        tools: Vec<ToolInvocation>,
        usage: Option<Usage>,
        parts: CompletionParts,
    },
    Cancelled,
    Err(String),
}
