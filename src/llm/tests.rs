//! Tests für die LLM-Schicht: Tool-Definitionen, Delta-Akkumulation,
//! Wire-Verträge, Hilfe-Funktionen und Kompaktierungs-Grenzen.
//!
//! Nach der Migration auf das Event-Log (`chat`) testet dieses Modul nur noch
//! die Architektur-unabhängige Logik (Werkzeuge, Draht-Format, Helfer) – nicht
//! mehr das alte `ChatMessage`-Layout.

use super::*;
use crate::channel::{Channel, ChannelKind, RunOut, SearchResult};
use crate::config::{Config, ProviderConfig};
use crate::llm;
use crate::perm::Permission;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── Tool-Delta-Akkumulation (tools_def) ───────────────────────────────────

#[test]
fn tool_call_deltas_werden_akkumuliert() {
    let mut accs: Vec<ToolCallAcc> = Vec::new();
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"id":"call_1","type":"function",
                "function":{"name":"read","arguments":"{\"path\":"}}]}),
    );
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"function":{"arguments":"\"Cargo.toml\"}"}}]}),
    );
    assert_eq!(accs.len(), 1);
    assert_eq!(accs[0].id, "call_1");
    assert_eq!(accs[0].name, "read");
    assert_eq!(accs[0].arguments, "{\"path\":\"Cargo.toml\"}");
}

#[test]
fn mehrere_tool_indizes_werden_getrennt_gesammelt() {
    let mut accs: Vec<ToolCallAcc> = Vec::new();
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"grep","arguments":"{\"pattern\":\"x\"}"}}]}),
    );
    apply_tool_delta(
        &mut accs,
        &json!({"tool_calls":[{"index":1,"id":"b","function":{"name":"read","arguments":"{\"path\":\"y\"}"}}]}),
    );
    assert_eq!(accs.len(), 2);
}

// ── Tool-Definitionen (tools_def) ─────────────────────────────────────────

#[test]
fn tool_definitions_enthalten_alle_werkzeuge() {
    let defs = tool_definitions(Permission::Execute, &[], true);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    for want in ["grep", "read", "glob", "webfetch", "write", "bash", "edit"] {
        assert!(names.contains(&want), "Werkzeug fehlt: {want}");
    }
}

#[test]
fn tool_definitions_folgen_der_berechtigung() {
    let defs = |p: Permission| {
        tool_definitions(p, &[], true)
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<String>>()
    };
    assert_eq!(defs(Permission::Read), ["grep", "read", "glob", "webfetch"]);
    assert_eq!(
        defs(Permission::Write),
        ["grep", "read", "glob", "webfetch", "edit", "write"]
    );
    assert_eq!(
        defs(Permission::Execute),
        ["grep", "read", "glob", "webfetch", "edit", "write", "bash"]
    );
}

#[test]
fn sanitize_arguments_normalisiert_auf_gueltiges_json() {
    // Bereits gültiges JSON bleibt unverändert.
    assert_eq!(sanitize_arguments("{\"a\":1}"), "{\"a\":1}");
    // Ungültiges/leeres → {} (keine naive Quote-Umschreibung).
    assert_eq!(sanitize_arguments(""), "{}");
    assert_eq!(sanitize_arguments("{'path':'Cargo.toml'}"), "{}");
}

// ── Force-Tools (tools_def) ────────────────────────────────────────────────

#[test]
fn tool_definitions_mit_force_tools_fuegt_dummies_hinzu() {
    // Gebundener Kanal, Read-Permission: read ist erlaubt, bash nicht →
    // bash als Dummy.
    let defs = tool_definitions(Permission::Read, &["read".into(), "bash".into()], true);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    // read ist ein normales (vollständiges) Tool.
    assert!(
        names.contains(&"read"),
        "read muss als normales Tool vorhanden sein"
    );
    // bash ist als Dummy vorhanden.
    assert!(
        names.contains(&"bash"),
        "bash muss als Dummy vorhanden sein"
    );
    // grep/glob/webfetch sind ebenfalls vorhanden (Permission::Read).
    assert!(names.contains(&"grep"));
    assert!(names.contains(&"glob"));
    assert!(names.contains(&"webfetch"));
    // write/edit sind NICHT vorhanden (keine Write-Permission, kein force_tools).
    assert!(!names.contains(&"write"));
    assert!(!names.contains(&"edit"));
}

#[test]
fn tool_definitions_force_tools_bereits_erlaubt_kein_duplikat() {
    // Bei Execute-Permission: bash ist bereits erlaubt → kein Dummy.
    let defs = tool_definitions(Permission::Execute, &["bash".into()], true);
    let bash_count = defs
        .iter()
        .filter(|d| d["function"]["name"].as_str() == Some("bash"))
        .count();
    assert_eq!(
        bash_count, 1,
        "bash darf nur EINMAL vorkommen (nicht als Dummy)"
    );
}

#[test]
fn tool_definitions_ohne_force_tools_gleich_wie_bisher() {
    // Gebundener Kanal, ohne force_tools: identisches Verhalten wie vorher.
    let defs = tool_definitions(Permission::Read, &[], true);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(names, ["grep", "read", "glob", "webfetch"]);
}

// ── Kanallose Session (tools_def) ───────────────────────────────────────────

#[test]
fn tool_definitions_ohne_kanal_nur_webfetch_plus_force_dummies() {
    // Kanallose Session (kein Verzeichnis/Shell): nur webfetch bleibt als
    // volles Werkzeug übrig; die force_tools des zen-Providers (read, bash)
    // kommen als Dummies. grep/glob/write/edit werden gar nicht angeboten.
    let defs = tool_definitions(Permission::Read, &["read".into(), "bash".into()], false);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(names, ["webfetch", "read", "bash"]);
    // webfetch bleibt ein normales (vollständiges) Tool.
    let wf = defs
        .iter()
        .find(|d| d["function"]["name"].as_str() == Some("webfetch"))
        .expect("webfetch vorhanden");
    assert_ne!(
        wf["function"]["description"].as_str(),
        Some("This tool is disabled."),
        "webfetch ohne Kanal ist voll funktional"
    );
    // read/bash sind als Dummies ("disabled") markiert.
    for dummy in &["read", "bash"] {
        let d = defs
            .iter()
            .find(|d| d["function"]["name"].as_str() == Some(*dummy))
            .unwrap_or_else(|| panic!("{dummy} muss als Dummy vorhanden sein"));
        assert_eq!(
            d["function"]["description"].as_str(),
            Some("This tool is disabled."),
            "{dummy} muss Dummy-Beschreibung haben"
        );
    }
}

#[test]
fn tool_definitions_ohne_kanal_und_ohne_force_nur_webfetch() {
    // Ohne Kanal UND ohne force_tools bleibt nur webfetch übrig.
    let defs = tool_definitions(Permission::Read, &[], false);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(names, ["webfetch"]);
}

#[test]
fn tool_definitions_ohne_kanal_ignoriert_hoehere_permission() {
    // Auch mit Execute-Permission: ohne Kanal keine grep/glob/bash/write/edit
    // als volle Werkzeuge – nur webfetch plus force-Dummies.
    let defs = tool_definitions(Permission::Execute, &["read".into(), "bash".into()], false);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(names, ["webfetch", "read", "bash"]);
}

#[test]
fn dummy_tool_hat_minimale_definion_mit_pflichtparameter() {
    let d = super::dummy_tool("bash");
    assert_eq!(d["function"]["name"], "bash");
    assert_eq!(d["function"]["description"], "This tool is disabled.");
    // required-Array muss den Parameter enthalten.
    let required = d["function"]["parameters"]["required"]
        .as_array()
        .expect("required ist ein Array");
    assert_eq!(required.len(), 1);
    assert_eq!(required[0], "command");
    // Properties muss den Parameter enthalten.
    let props = d["function"]["parameters"]["properties"]
        .as_object()
        .expect("properties ist ein Objekt");
    assert!(props.contains_key("command"));
}

#[test]
fn config_parse_force_tools_aus_toml() {
    let cfg: crate::config::Config = toml::from_str(
        r#"
        [provider.test]
        base_url = "https://example.com/v1"
        force_tools = ["read", "bash"]
        "#,
    )
    .expect("TOML lesbar");
    let p = &cfg.provider["test"];
    assert_eq!(p.force_tools, vec!["read", "bash"]);
}

#[test]
fn config_parse_force_tools_default_ist_leer() {
    let cfg: crate::config::Config = toml::from_str(
        r#"
        [provider.test]
        base_url = "https://example.com/v1"
        "#,
    )
    .expect("TOML lesbar");
    let p = &cfg.provider["test"];
    assert!(p.force_tools.is_empty(), "Default muss leer sein");
}

#[test]
fn resolved_endpoint_uebernimmt_force_tools() {
    let cfg: crate::config::Config = toml::from_str(
        r#"
        model = "test/m"

        [provider.test]
        base_url = "https://example.com/v1"
        force_tools = ["read", "bash"]
        "#,
    )
    .expect("TOML lesbar");
    let ep = cfg.resolve(None).expect("resolve");
    assert_eq!(ep.force_tools, vec!["read", "bash"]);
}

#[test]
fn default_provider_zen_hat_force_tools() {
    let cfg = crate::config::Config::default();
    let zen = cfg.provider.get("zen").expect("zen-Provider");
    assert_eq!(zen.force_tools, vec!["read", "bash"]);
}

#[test]
fn tool_definitions_alle_force_tools_als_dummies() {
    // Read-Permission + alle 7 Tools als force → write/edit/bash als Dummies.
    let force: Vec<String> = ["read", "bash", "write", "edit", "grep", "glob", "webfetch"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let defs = tool_definitions(Permission::Read, &force, true);
    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    // Alle 7 müssen vorhanden sein.
    assert_eq!(names.len(), 7);
    for want in &["grep", "read", "glob", "webfetch", "write", "edit", "bash"] {
        assert!(names.contains(want), "{want} fehlt");
    }
    // Die 3 nicht-permissions-erlaubten müssen Dummies sein (Beschreibung = "disabled").
    for dummy_name in &["write", "edit", "bash"] {
        let d = defs
            .iter()
            .find(|d| d["function"]["name"].as_str() == Some(*dummy_name))
            .unwrap_or_else(|| panic!("{dummy_name} muss vorhanden sein"));
        assert_eq!(
            d["function"]["description"].as_str(),
            Some("This tool is disabled."),
            "{dummy_name} muss Dummy-Beschreibung haben"
        );
    }
}

// ── Wire-Verträge (wire) ──────────────────────────────────────────────────

#[test]
fn ensure_reasoning_fuellt_leeres_feld_fuer_tool_calls_nach() {
    let assistant = WireMessage {
        role: "assistant".into(),
        content: Some("Antwort".into()),
        reasoning_content: None,
        tool_calls: Some(vec![WireToolCall {
            id: "call_1".into(),
            ty: "function".into(),
            function: WireFunction {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }]),
        tool_call_id: None,
        tokens: WireTokens::default(),
    };
    let norm = ensure_reasoning_for_tool_calls(std::slice::from_ref(&assistant));
    assert_eq!(norm[0].reasoning_content.as_deref(), Some(""));
    // Nicht-Tool-Turns bleiben unangetastet.
    let mut plain = assistant;
    plain.tool_calls = None;
    plain.reasoning_content = None;
    let norm2 = ensure_reasoning_for_tool_calls(&[plain]);
    assert_eq!(norm2[0].reasoning_content, None);
}

// ── Token-Schätzung (mod.rs) ──────────────────────────────────────────────

#[test]
fn estimate_tokens_rundet_auf_vier_zeichen() {
    assert_eq!(llm::estimate_tokens(""), 1);
    assert_eq!(llm::estimate_tokens("abcd"), 2);
    assert_eq!(llm::estimate_tokens("abcdefgh"), 3);
}

// ── HTTP-Parsing / Retry (http) ───────────────────────────────────────────

#[test]
fn parse_usage_liest_tokens() {
    let j = json!({"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}});
    let u = parse_usage(&j).expect("Usage vorhanden");
    assert_eq!(u.prompt_tokens, 10);
    assert_eq!(u.completion_tokens, 4);
    assert_eq!(u.total_tokens, 14);
    assert_eq!(parse_usage(&json!({"x":1})), None);
}

#[test]
fn retry_backoff_waechst_und_jitter_bleibt_in_grenzen() {
    assert_eq!(retry_delay(0), Duration::from_secs(2));
    let d1 = retry_delay(1);
    let d2 = retry_delay(3);
    assert!(d2 > d1, "Backoff muss wachsen");
}

// ── SSE-Fragmentierung großer Tool-Argumente (http) ─────────────────────────
//
// Manche OpenAI-kompatiblen Endpunkte/Proxys zerlegen lange `edit`-/`write`-
// Argumente mit mehrzeiligen Blöcken über mehrere `data:`-Zeilen. Der
// Fragment-Puffer muss solche Nutzlasten wieder zu einem gültigen Event
// zusammensetzen, statt sie als „unvollständig“ zu verwerfen (→ abgeschnittene
// `arguments` → „Argument … missing“).

#[test]
fn sse_fragment_in_string_wert_wird_ohne_verfaelschung_zusammengesetzt() {
    // Der Provider bricht ein `edit`-Event mitten im `new`-String-Wert über
    // zwei `data:`-Zeilen um. Wir bauen das Event per serde_json (garantiert
    // korrektes Escaping), serialisieren den `new`-Wert – der einen echten
    // Zeilenumbruch enthält – und splitten an einer Stelle mitten in diesem
    // Wert. Beim Zusammensetzen darf KEIN Trennzeichen in den String geraten,
    // sonst wiche der `new`-Wert von den beiden Originalen ab.
    let full = tool_call_event(&ToolArgs {
        path: "a.txt",
        old: "alt",
        new: "zwei\nzeilen\nuebersprung",
        id: "c1",
    });

    // Schnitt mitten in `...new\":\"zwei` – also mitten im neuen Wert.
    let cut = full.find("{\\\"path").unwrap()
        + "{\\\"path\\\":\\\"a.txt\\\",\\\"old\\\":\\\"alt\\\",\\\"new\\\":\\\"zwei".len();
    let part1 = &full[..cut];
    let part2 = &full[cut..];

    let mut pending = String::new();
    assert!(
        super::http::accumulate_sse_event(&mut pending, part1).is_none(),
        "erstes Fragment muss noch unvollständig sein"
    );
    let ev = super::http::accumulate_sse_event(&mut pending, part2)
        .expect("zweites Fragment schließt Event ab");
    let args = ev["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("arguments als String");
    // Der `new`-Wert besteht exakt aus der Aneinanderreihung der beiden
    // Fragment-Stücke, ohne eingefügtes Trennzeichen. Im rohen `arguments`-
    // String steht der Zeilenumbruch als JSON-Escapesequenz (`\n` als zwei
    // Zeichen) – das ist der unverfälschte Wert, den das spätere
    // `sanitize_arguments` / der `edit`-Aufruf erhält.
    assert_eq!(
        args,
        r#"{"path":"a.txt","old":"alt","new":"zwei\nzeilen\nuebersprung"}"#
    );

    // Puffer ist danach leer – das nächste (unabhängige) Event kommt sauber durch.
    assert!(pending.is_empty());
    let ok = super::http::accumulate_sse_event(
        &mut pending,
        r#"{"choices":[],"usage":{"completion_tokens":1}}"#,
    );
    assert!(
        ok.is_some(),
        "nächstes eigenständiges Event muss parsebar sein"
    );
}

#[test]
fn sse_split_zwischen_json_tokens_wird_zusammengesetzt() {
    // Schnitt zwischen zwei JSON-Tokens des `arguments`-Strings – hier direkt
    // vor `"new"`. Das Fragment-Paar muss wieder zu einem gültigen Event
    // verbunden werden; in diesem Fall wäre selbst ein `\n` als Whitespace
    // unschädlich, aber der leere Join löst es bereits verlustfrei.
    let full = tool_call_event(&ToolArgs {
        path: "a.txt",
        old: "alt",
        new: "neu",
        id: "c1",
    });
    let cut = full.find("\\\"new\\\"").unwrap();
    let part1 = &full[..cut];
    let part2 = &full[cut..];

    let mut pending = String::new();
    assert!(super::http::accumulate_sse_event(&mut pending, part1).is_none());
    let ev = super::http::accumulate_sse_event(&mut pending, part2).expect("zusammen gültig");
    assert_eq!(
        ev["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str(),
        Some(r#"{"path":"a.txt","old":"alt","new":"neu"}"#)
    );
    assert!(pending.is_empty());
}

/// Baut ein Chat-Completions-Delta-Event für einen `edit`-Aufruf.
///
/// Die `arguments` werden als String mit fester Feld-Reihenfolge
/// (`path`, `old`, `new`) aufgebaut und dann als JSON-String in den
/// `delta.tool_calls[0].function.arguments`-Slot gesetzt. Das escapert
/// `serde_json` beim Serialisieren korrekt (verschachteltes JSON im String).
struct ToolArgs<'a> {
    path: &'a str,
    old: &'a str,
    new: &'a str,
    id: &'a str,
}

fn tool_call_event(a: &ToolArgs) -> String {
    // Argumente-Inhalt als roher Text mit stabiler Feldreihenfolge. Jeder Wert
    // wird einzeln korrekt als JSON-String escapt (`serde_json::to_string`
    // liefert `"…"` inkl. Quotes und Escaping).
    let esc = |v: &str| serde_json::to_string(v).expect("korrekter JSON-String");
    let args_raw = format!(
        "{{\"path\":{},\"old\":{},\"new\":{}}}",
        esc(a.path),
        esc(a.old),
        esc(a.new)
    );
    serde_json::json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": a.id,
                    "function": { "name": "edit", "arguments": args_raw }
                }]
            }
        }]
    })
    .to_string()
}

// Die `.done`-Argumente eines Responses-Streams dürfen die per Deltas
// akkumulierten nur ersetzen, wenn letztere unvollständig sind – sonst gingen
// die über die Deltas bereits gebuchten Token verloren bzw. würden doppelt
// zählen.
#[test]
fn abgeschnittene_tool_argumente_gelten_als_unvollstaendig() {
    // Nichts angekommen.
    assert!(super::http::arguments_incomplete(""));
    assert!(super::http::arguments_incomplete("   "));
    // Präfix eines Objekts – der Fall eines abgebrochenen Delta-Stroms: `path`
    // und `old` sind da, `new` fehlt noch. Ein solches Präfix kann nie
    // gültiges JSON sein (die äußere `{` bliebe ungeöffnet).
    assert!(super::http::arguments_incomplete(
        r#"{"path":"a.txt","old":"alt""#
    ));
    assert!(super::http::arguments_incomplete(
        r#"{"path":"a.txt","old":"alt","new""#
    ));
    assert!(super::http::arguments_incomplete(
        r#"{"path":"a.txt","old":"alt","new":""#
    ));
    // Ungültiges bzw. nachgeschobenes JSON.
    assert!(super::http::arguments_incomplete("{'path':'a.txt'}"));
    assert!(super::http::arguments_incomplete(r#"{"path":"a.txt"}x"#));
}

#[test]
fn vollstaendige_tool_argumente_bleiben_unangetastet() {
    assert!(!super::http::arguments_incomplete(
        r#"{"path":"a.txt","old":"alt","new":"neu"}"#
    ));
    assert!(!super::http::arguments_incomplete(
        "  {\"path\":\"a.txt\"}  "
    ));
}

// ── Edit-Werkzeug: Happy Path + Argument-Prüfung (tools_exec) ────────────────
//
// Diese Tests rufen `run_tool_live("edit", …)` direkt auf – mit allen drei
// Argumenten (`path`/`old`/`new`), wie es ein Modell tut. Der Fall „alle drei
// da, aber `new` fehlt“ ist damit abgedeckt: `new` als Nicht-String wird
// korrekt abgelehnt, `new` als String wird angewendet.

/// In-Memory-Kanal als Test-Double: hält ein einzelnes „Arbeitsverzeichnis"
/// als Datei-Gemisch und implementiert nur das Nötigste, damit `run_tool_live`
/// für `edit` funktioniert (read + write). Alle übrigen Kanal-Methoden sind
/// Stubs, die in diesen Tests nicht aufgerufen werden.
struct MockChannel {
    files: Mutex<HashMap<String, String>>,
    /// Letzter an `grep` übergebener Suchpfad (kanalrelativ) – zum Prüfen der
    /// Absolutpfad-Abbildung.
    last_grep_path: Mutex<Option<String>>,
    /// Letztes an `glob` übergebenes Muster – zum Prüfen der Abbildung.
    last_glob_pattern: Mutex<Option<String>>,
}

impl MockChannel {
    fn new(files: HashMap<String, String>) -> Self {
        Self {
            files: Mutex::new(files),
            last_grep_path: Mutex::new(None),
            last_glob_pattern: Mutex::new(None),
        }
    }

    fn last_grep_path(&self) -> Option<String> {
        self.last_grep_path.lock().unwrap().clone()
    }

    fn last_glob_pattern(&self) -> Option<String> {
        self.last_glob_pattern.lock().unwrap().clone()
    }
}

impl Channel for MockChannel {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Local
    }
    fn root(&self) -> String {
        "/mock".to_string()
    }
    fn read(&self, rel: &Path) -> Result<String, String> {
        let files = self.files.lock().unwrap();
        files
            .get(rel.to_str().unwrap_or(""))
            .cloned()
            .ok_or_else(|| format!("Datei fehlt: {}", rel.display()))
    }
    fn write(&self, rel: &Path, content: &str) -> Result<(), String> {
        let mut files = self.files.lock().unwrap();
        files.insert(rel.to_str().unwrap_or("").to_string(), content.to_string());
        Ok(())
    }
    fn abs_root(&self) -> Option<String> {
        // Mount-Punkt des gemockten Arbeitsverzeichnisses – entspricht `root()`.
        Some("/mock".to_string())
    }
    fn glob(&self, pattern: &str, _rel: &Path) -> Result<Vec<String>, String> {
        *self.last_glob_pattern.lock().unwrap() = Some(pattern.to_string());
        Ok(Vec::new())
    }
    fn grep(
        &self,
        _pattern: &str,
        rel: &Path,
        _include: Option<&str>,
        _context_lines: usize,
    ) -> Result<SearchResult, String> {
        *self.last_grep_path.lock().unwrap() = Some(rel.to_string_lossy().into_owned());
        Ok(SearchResult {
            matches: Vec::new(),
            note: None,
            raw: None,
            match_count: 0,
        })
    }
    fn run(&self, _cmd: &str, _args: &[String], _rel_cwd: &Path) -> Result<RunOut, String> {
        Ok(RunOut {
            exit_code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
    fn dup(&self) -> Result<Arc<dyn Channel>, String> {
        let files = self.files.lock().unwrap().clone();
        Ok(Arc::new(MockChannel::new(files)))
    }
}

#[test]
fn edit_happy_path_wendet_austausch_an() {
    let mut files = HashMap::new();
    files.insert(
        "a.txt".to_string(),
        "alt-text zeile\nzweite zeile\n".to_string(),
    );
    let ch = MockChannel::new(files);

    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"a.txt","old":"alt-text zeile","new":"NEU-TEXT"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("Changed: a.txt"), "Text: {}", out.text);
    assert_eq!(
        ch.read(Path::new("a.txt")).unwrap(),
        "NEU-TEXT\nzweite zeile\n"
    );
    assert!(out.diff.is_some(), "edit liefert einen Diff");
}

#[test]
fn edit_happy_path_mit_replace_all() {
    let mut files = HashMap::new();
    files.insert("b.txt".to_string(), "x\nx\n".to_string());
    let ch = MockChannel::new(files);

    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"b.txt","old":"x","new":"y","replace_all":true}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("Changed: b.txt"), "Text: {}", out.text);
    assert_eq!(ch.read(Path::new("b.txt")).unwrap(), "y\ny\n");
}

#[test]
fn edit_ohne_new_wird_als_missing_gemeldet() {
    let mut files = HashMap::new();
    files.insert("a.txt".to_string(), "alt-text zeile\n".to_string());
    let ch = MockChannel::new(files);
    // `new` fehlt komplett.
    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"a.txt","old":"alt-text zeile"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert_eq!(
        out.text,
        r#"ERROR: Argument "new" missing or not a string."#
    );

    // `new` ist vorhanden, aber kein String (z. B. `null`) → dieselbe Ablehnung.
    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"a.txt","old":"alt-text zeile","new":null}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert_eq!(
        out.text,
        r#"ERROR: Argument "new" missing or not a string."#
    );
}

// ── Absolute Pfade (read/write/edit): nur unter dem Mount-Punkt ─────────────
//
// Der MockChannel deklariert `/mock` als Mount-Punkt der Arbeitskopie
// (`abs_root`). Absolute Tool-Pfade darunter werden um den Mount-Punkt gekürzt
// und wie ein relativer Pfad behandelt; alles andere wird abgelehnt und die
// Datei-Operation gar nicht erst ausgeführt.

#[test]
fn read_absoluter_pfad_im_mount_wird_gelesen() {
    let mut files = HashMap::new();
    files.insert("a.txt".to_string(), "inhalt\n".to_string());
    let ch = MockChannel::new(files);
    let out = super::tools_exec::run_tool_live(
        "read",
        r#"{"path":"/mock/a.txt"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.contains("inhalt"), "Text: {}", out.text);
}

#[test]
fn read_absoluter_pfad_ausserhalb_wird_abgelehnt() {
    let ch = MockChannel::new(HashMap::new());
    let out = super::tools_exec::run_tool_live(
        "read",
        r#"{"path":"/etc/passwd"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("ERROR:"), "Text: {}", out.text);
    assert!(out.text.contains("/etc/passwd"), "Text: {}", out.text);
}

#[test]
fn write_absoluter_pfad_im_mount_wird_geschrieben() {
    let ch = MockChannel::new(HashMap::new());
    let out = super::tools_exec::run_tool_live(
        "write",
        r#"{"path":"/mock/sub/x.txt","content":"hi"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("Written:"), "Text: {}", out.text);
    assert_eq!(ch.read(Path::new("sub/x.txt")).unwrap(), "hi");
}

#[test]
fn edit_absoluter_pfad_ausserhalb_laesst_datei_unveraendert() {
    let mut files = HashMap::new();
    files.insert("a.txt".to_string(), "alt\n".to_string());
    let ch = MockChannel::new(files);
    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"/etc/a.txt","old":"alt","new":"neu"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("ERROR:"), "Text: {}", out.text);
    // Nichts geschrieben – die vorhandene Datei bleibt unberührt.
    assert_eq!(ch.read(Path::new("a.txt")).unwrap(), "alt\n");
}

#[test]
fn edit_absoluter_pfad_im_mount_wird_angewendet() {
    let mut files = HashMap::new();
    files.insert("a.txt".to_string(), "alt\nzweite\n".to_string());
    let ch = MockChannel::new(files);
    let out = super::tools_exec::run_tool_live(
        "edit",
        r#"{"path":"/mock/a.txt","old":"alt","new":"neu"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("Changed:"), "Text: {}", out.text);
    assert_eq!(ch.read(Path::new("a.txt")).unwrap(), "neu\nzweite\n");
}

// `grep`/`glob` nutzen dieselbe Abbildung: bei `grep` ist es das `path`-Argument,
// bei `glob` der `pattern` selbst. Der MockChannel protokolliert die zuletzt
// gesehenen Argumente, damit die Ableitung (Absolutpfad → relativ) prüfbar ist.

#[test]
fn grep_absoluter_pfad_im_mount_wird_gekuerzt() {
    let ch = MockChannel::new(HashMap::new());
    let _ = super::tools_exec::run_tool_live(
        "grep",
        r#"{"pattern":"x","path":"/mock/src","content":0}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert_eq!(ch.last_grep_path(), Some("src".to_string()));
}

#[test]
fn grep_absoluter_pfad_ausserhalb_wird_abgelehnt() {
    let ch = MockChannel::new(HashMap::new());
    let out = super::tools_exec::run_tool_live(
        "grep",
        r#"{"pattern":"x","path":"/etc"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("ERROR:"), "Text: {}", out.text);
    // Kein Suchlauf – der Pfad wurde gar nicht erst an den Kanal gegeben.
    assert_eq!(ch.last_grep_path(), None);
}

#[test]
fn glob_absolutes_muster_im_mount_wird_gekuerzt() {
    let ch = MockChannel::new(HashMap::new());
    let _ = super::tools_exec::run_tool_live(
        "glob",
        r#"{"pattern":"/mock/src/**/*.rs"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert_eq!(ch.last_glob_pattern(), Some("src/**/*.rs".to_string()));
}

#[test]
fn glob_absolutes_muster_ausserhalb_wird_abgelehnt() {
    let ch = MockChannel::new(HashMap::new());
    let out = super::tools_exec::run_tool_live(
        "glob",
        r#"{"pattern":"/etc/**/*.conf"}"#,
        &ch,
        &mut |_| {},
        None,
    );
    assert!(out.text.starts_with("ERROR:"), "Text: {}", out.text);
    assert_eq!(ch.last_glob_pattern(), None);
}

// ── Kompaktierungs-Grenzen (compact) ──────────────────────────────────────

#[test]
fn wire_compact_boundary_zaehlt_turns_an_user_nachrichten() {
    let w = |role: &str| WireMessage {
        role: role.into(),
        content: Some("x".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens::default(),
    };
    // Turn 1 mit Werkzeug-Runde: user → assistant(tool_calls) → tool → tool
    let mut tool = w("assistant");
    tool.tool_calls = Some(vec![WireToolCall {
        id: "c".into(),
        ty: "function".into(),
        function: WireFunction {
            name: "read".into(),
            arguments: "{}".into(),
        },
    }]);
    let msgs = vec![
        w("user"),
        tool,
        w("tool"),
        w("tool"),
        w("user"),
        w("assistant"),
        w("user"),
    ];
    // Die Zählung läuft über `user`-Nachrichten (identisch zu
    // `compact_boundary` auf der Session-Seite): eine mehrteilige Tool-Runde
    // ist Teil ihres Turns und wird komplett mit archiviert.
    // Semantik: `keep_turns = k` ⇒ GENAU die letzten k Turns überleben.
    // keep=1 → nur turn3 (Pending user3, Index 6) bleibt; Turn 1+2 werden
    // archiviert.
    assert_eq!(wire_compact_boundary(&msgs, 1), 6);
    // keep=2 → die letzten 2 User (turn2 + Pending user3) bleiben, Turn 1 weg.
    assert_eq!(wire_compact_boundary(&msgs, 2), 4);
    // keep größer als die Turn-Zahl → nichts zu archivieren.
    assert_eq!(wire_compact_boundary(&msgs, 9), 0);
}

#[test]
fn wire_compact_boundary_erlaubt_schnitt_an_letzter_user_nachricht() {
    // Der Fall aus dem echten Kompaktierungs-Log (2026-09-29): der letzte
    // `user` liegt NICHT am Ende der Liste – nach ihm folgen noch tool/assistant-
    // Nachrichten der finalen Runde. Früher (Semantik `keep=k` ⇒ k+1 überlebende
    // Turns) war `keep=1` auf den vorletzten User beschränkt; jetzt muss
    // `keep=1` GENAU die letzte User-Nachricht treffen.
    let w = |role: &str| WireMessage {
        role: role.into(),
        content: Some("x".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens::default(),
    };
    // 5 User bei Index 0,1,5,127,194 – danach folgen weitere tool/assistant-
    // Nachrichten (bis 232), damit die letzte User-Nachricht nicht am Ende steht.
    let mut msgs: Vec<WireMessage> = Vec::new();
    let user_idx = [0usize, 1, 5, 127, 194];
    let mut ui = 0;
    for i in 0..233 {
        if ui < user_idx.len() && i == user_idx[ui] {
            msgs.push(w("user"));
            ui += 1;
        } else {
            msgs.push(w(if i % 2 == 0 { "tool" } else { "assistant" }));
        }
    }
    // keep=1 → Schnitt genau an der letzten User-Nachricht (Index 194).
    assert_eq!(wire_compact_boundary(&msgs, 1), 194);
    // keep=2 → Schnitt an der vorletzten User-Nachricht (127).
    assert_eq!(wire_compact_boundary(&msgs, 2), 127);
    // keep=3 → an der drittletzten (5); keep=4 → an der viertletzten (1).
    assert_eq!(wire_compact_boundary(&msgs, 3), 5);
    assert_eq!(wire_compact_boundary(&msgs, 4), 1);
    // keep=5 (users-1) → nur der älteste User (0) wird archiviert.
    assert_eq!(wire_compact_boundary(&msgs, 5), 0);
}

#[test]
fn context_length_fehler_wird_erkannt() {
    assert!(looks_like_context_error(
        "Request too large: maximum context length"
    ));
    assert!(looks_like_context_error("prompt is too long for the model"));
    assert!(!looks_like_context_error("rate limit exceeded"));
}

#[test]
fn kompaktierung_zu_kurze_historie_meldet_abbruch() {
    let client = shared_client();
    let cfg = test_config("http://127.0.0.1:1");
    let ep = cfg.resolve(None).unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Ohne Turn-Grenzen gibt es nichts zu kompaktieren.
    let msgs = vec![WireMessage {
        role: "user".into(),
        content: Some("nur eine".into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens::default(),
    }];
    let err = compact_chat_messages(
        0,
        client,
        &cfg,
        &ep,
        &msgs,
        &cancel,
        CompactTrigger::Proactive,
        None,
    )
    .unwrap_err();
    assert!(!err.is_empty());
}

// ── Kompaktierungs-Planung & -Entscheidung ──────────────────────────────

/// Testnachricht mit Text – bewertet wie die echte Projektion
/// (`api_messages`): bestätigte Zahl, sonst Schätzung über den Text. Ohne das
/// hätten die Nachrichten hier 0 Tokens und die Schnitt-Tests prüften nichts.
fn wm(role: &str, content: &str) -> WireMessage {
    WireMessage {
        role: role.into(),
        content: Some(content.into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
        tokens: WireTokens {
            content: WireTokens::part(0, content),
            ..Default::default()
        },
    }
}

/// `turns` abwechselnd `user`/`assistant`.
fn turns(n: usize, chars: usize) -> Vec<WireMessage> {
    let mut out = Vec::new();
    for i in 0..n {
        out.push(wm("user", &"a".repeat(chars + i)));
        out.push(wm("assistant", &"b".repeat(chars)));
    }
    out
}

#[test]
fn plan_candidates_listet_schnitte_mit_kontextgroessen() {
    // 6 Turns → 6 user → größtes sinnvolles keep = 5 (`users - 1`: nur der
    // älteste Turn wird archiviert, vgl. `possible_max_keep`). Hier wird der
    // Bereich bewusst auf keep=1..4 geprüft.
    let msgs = turns(6, 100);
    let cands = plan_candidates(&msgs, 4);
    assert_eq!(cands.len(), 4, "keep=1..4 liefern je einen echten Schnitt");
    // Keep größer → Schnitt früher → weniger archiviert, weniger fällt weg,
    // mehr bleibt.
    for w in cands.windows(2) {
        assert!(
            w[0].boundary > w[1].boundary,
            "boundary sinkt mit keep ({:?} → {:?})",
            w[0],
            w[1]
        );
        assert!(
            w[0].dropped_tokens > w[1].dropped_tokens,
            "dropped sinkt mit keep"
        );
        assert!(w[0].kept_tokens < w[1].kept_tokens, "kept steigt mit keep");
    }
    // keep=4: Schnitt am 4.-letzten User → die ersten 4 Nachrichten (Turn 0+1)
    // werden archiviert.
    assert_eq!(cands[3].archived_msgs, 4);
    assert!(cands[3].dropped_tokens > 0);
    assert!(cands[3].kept_tokens > 0);
}

#[test]
fn plan_candidates_ohne_moeglichen_schnitt_leer() {
    // Nur 1 user → kein Kandidat mit echtem Schnitt (mind. 2 nötig: keep=1
    // ließe nur den letzten Turn überleben → nichts zu archivieren).
    assert_eq!(plan_candidates(&turns(1, 10), 8).len(), 0);
    assert_eq!(
        plan_candidates(&turns(2, 10), 8).len(),
        1,
        "keep=1 → nur letzter Turn weg, einer bleibt"
    );
}

/// Wire-Projektion wie nach einer Kompaktierung: die Summary der letzten
/// Kompaktierung steht als erste `user`-Nachricht, danach `n` Turns.
fn turns_after_summary(n: usize, chars: usize) -> Vec<WireMessage> {
    let mut out = vec![wm(
        "user",
        &format!(
            "[Compressed history - 12 earlier messages]\n\n{}",
            "s".repeat(chars)
        ),
    )];
    out.extend(turns(n, chars));
    out
}

#[test]
fn can_compact_folgt_den_benoetigten_user_nachrichten() {
    // Ein einziger (z. B. riesiger, gerade laufender) Query: kein Schnitt.
    assert!(!can_compact(&turns(1, 10)), "ein User → kein Schnitt");
    // Ab drei Turns (ein archivierbarer, zwei überlebende) ist ein Schnitt möglich.
    assert!(can_compact(&turns(3, 10)), "drei User → Schnitt möglich");
}

#[test]
fn can_compact_nicht_unmittelbar_nach_kompaktierung() {
    // Summary + 1 Query (wie direkt nach `/compact`).
    assert!(
        !can_compact(&turns_after_summary(1, 10)),
        "Summary + ein Query → nichts Neues zu schneiden"
    );
    // Summary + 2 Turns: `keep_turns = 1` (nur der letzte Turn überlebt) ist
    // jetzt ein echter, nicht-gesperrter Schnitt – es wird ein neuer Turn (der
    // zweite) zusammen mit der alten Summary archiviert, keine Wiederholung.
    assert!(
        can_compact(&turns_after_summary(2, 10)),
        "Summary + zwei Turns → keep=1 (nur letzter Turn) möglich"
    );
    // Summary + 3 Turns: ein weiterer Turn lässt sich neu archivieren.
    assert!(
        can_compact(&turns_after_summary(3, 10)),
        "Summary + drei Turns → Schnitt möglich"
    );
}

#[test]
fn decide_keep_waehlt_schnitt_naechst_am_freiziel() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 100_000;
    cfg.compact_keep_ratio = 0.2; // Ziel: ~20 000 T bleiben, ~80 000 T frei
    cfg.compact_summary_tokens = 1_000;
    // 6 gleich große Turns (~10 002 T je Turn; `keep=k` behält genau k davon):
    // Ziel 20 000 T liegt konkret zwischen keep=1 (1 Turn, ~10 000 T) und
    // keep=2 (2 Turns, ~20 004 T) – näher ist keep=2.
    let msgs = turns(6, 20_000);
    let (keep, cands) = decide_keep(&msgs, &cfg, CompactTrigger::AutoTurn, cfg.context_window)
        .expect("Schnitt vorhanden");
    assert_eq!(keep, 2, "Schnitt, dessen Tail dem Ziel am nächsten liegt");
    let chosen = cands.iter().find(|c| c.keep_turns == keep).unwrap();
    let kept = chosen.kept_tokens;
    let total = cands
        .iter()
        .map(|c| c.dropped_tokens + c.kept_tokens)
        .next()
        .unwrap();
    assert!(
        kept.abs_diff(20_000) < 1_000,
        "erhaltener Tail ~{kept} T nahe am Ziel (~20 % des Fensters)"
    );
    // 80 % *des Fensters* frei werden ist nur möglich, wenn der Kontext auch
    // gefüllt war; maßgeblich ist der Zielwert selbst – hier bleiben ~20 % des
    // Fensters stehen, der Rest der ~60 000 T Historie fällt weg.
    assert_eq!(chosen.dropped_tokens + kept, total);
    assert!(
        chosen.dropped_tokens > 39_000,
        "mehr als die Hälfte fällt weg"
    );
    assert!(
        kept < 20_000 + 1_000,
        "Ziel wird nicht nach oben überschritten (Ziel {kept})"
    );
    // Ausdrücklich NICHT mehr „maximaler Erhalt“ (z. B. keep=4/5).
    assert_ne!(
        keep, 4,
        "Kriterium ist das Freiziel, nicht der größte Erhalt"
    );
}

#[test]
fn decide_keep_waehlt_staerksten_schnitt_wenn_ziel_unerreichbar() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 10_000;
    cfg.compact_keep_ratio = 0.2; // Ziel 2 000 T
    cfg.compact_summary_tokens = 1_000;
    // Riesige Historie: selbst der stärkste Schnitt (keep=1) lässt ~20 000 T
    // stehen, das Ziel 2 000 T ist unerreichbar → es wird so weit wie
    // möglich geschnitten.
    let msgs = turns(6, 40_000);
    let (keep, _cands) = decide_keep(&msgs, &cfg, CompactTrigger::AutoTurn, cfg.context_window)
        .expect("Schnitt vorhanden");
    assert_eq!(keep, 1, "unerreichbares Ziel → stärkster Schnitt");
    let (keep, _cands) = decide_keep(&msgs, &cfg, CompactTrigger::Manual, cfg.context_window)
        .expect("Schnitt vorhanden");
    assert_eq!(keep, 1, "unerreichbares Ziel → stärkster Schnitt (manuell)");
}

#[test]
fn decide_keep_waehlt_mehr_erhalt_wenn_ziel_bereits_unterschritten() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 100_000;
    cfg.compact_keep_ratio = 0.2; // Ziel 20 000 T
                                  // Winzige Historie (~2400 T gesamt): das Ziel ist unerreichbar, alles liegt
                                  // weit darunter → es wird der schwächste Schnitt gewählt (maximaler
                                  // Erhalt, `possible_max_keep = users - 1 = 5`), also nur der älteste Turn
                                  // archiviert.
    let msgs = turns(6, 100);
    let (keep, _cands) = decide_keep(&msgs, &cfg, CompactTrigger::AutoTurn, cfg.context_window)
        .expect("Schnitt vorhanden");
    assert_eq!(keep, 5, "Ziel weit unterboten → größtes zulässiges keep");
}

#[test]
fn decide_keep_schneidet_nie_wieder_an_der_letzten_stelle() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 100_000;
    cfg.compact_keep_ratio = 0.2; // Ziel 20 000 T
    cfg.compact_summary_tokens = 500;
    // Summary am Kopf + 4 Turns → 5 `user`-Nachrichten → `max_keep = 4`. Der
    // Kandidat `keep=4` schneidet unmittelbar hinter der Summary, also exakt an
    // der Stelle der letzten Kompaktierung: er würde nichts als die bereits
    // komprimierte Historie erneut zusammenfassen und ist ausgeschlossen.
    let msgs = turns_after_summary(4, 5_000);
    let (keep, cands) = decide_keep(&msgs, &cfg, CompactTrigger::AutoTurn, cfg.context_window)
        .expect("Schnitt vorhanden");
    let blocked = cands
        .iter()
        .find(|c| c.previous_cut)
        .expect("Kandidat der letzten Schnittstelle markiert");
    assert_eq!(blocked.keep_turns, 4, "letzte Schnittstelle = größtes keep");
    assert_eq!(blocked.archived_msgs, 1, "dort liegt nur die Summary");
    assert_ne!(keep, blocked.keep_turns, "nie erneut dort schneiden");
    assert_eq!(keep, 3, "nächststärkster erlaubter Schnitt");
    // Für alle nicht-reaktiven Auslöser gilt dieselbe Sperre.
    for t in [CompactTrigger::Proactive, CompactTrigger::Manual] {
        let (k, _) = decide_keep(&msgs, &cfg, t, cfg.context_window).expect("Schnitt vorhanden");
        assert_eq!(k, keep, "Sperre gilt auch für {t:?}");
    }
    // Der Kandidat bleibt im Protokoll sichtbar, nur markiert.
    assert!(cands.iter().all(|c| c.keep_turns <= 4));
}

#[test]
fn decide_keep_ohne_moeglichen_schnitt_liefert_keinen() {
    let cfg = test_config("http://127.0.0.1:1");
    // Summary + 1 Query (wie direkt nach `/compact`): jeder mögliche Schnitt
    // wäre eine Wiederholung der letzten Kompaktierung → nichts zu tun.
    assert!(
        decide_keep(
            &turns_after_summary(1, 10),
            &cfg,
            CompactTrigger::AutoTurn,
            100_000,
        )
        .is_none(),
        "nur die Stelle der letzten Kompaktierung infrage → kein Schnitt"
    );
    // Auch eine leere/einturnige Historie liefert keinen Schnitt.
    assert!(decide_keep(&turns(1, 10), &cfg, CompactTrigger::AutoTurn, 100_000).is_none());
}

#[test]
fn decide_keep_reaktiv_waehlt_staerksten_schnitt() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 100_000;
    cfg.compact_at = 0.8;
    cfg.compact_keep_ratio = 0.2;
    cfg.compact_summary_tokens = 500;
    // Bei einem context_length-Fehler zählt das VOLLE Fenster (nicht das
    // Freiziel und nicht die 80%-Schwelle): unter allen Kandidaten, die
    // kept+budget ins Fenster bringen, wählt Reactive das kleinste keep
    // (stärkster Schnitt) – hier 1.
    let msgs = turns(6, 20_000);
    let (keep, _) = decide_keep(&msgs, &cfg, CompactTrigger::Reactive, cfg.context_window)
        .expect("Schnitt vorhanden");
    assert_eq!(
        keep, 1,
        "Reactive wählt das kleinste keep unter dem vollen Fenster"
    );
}

#[test]
fn decide_keep_reaktiv_faellt_auf_staerksten_schnitt_zurueck() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 10_000;
    cfg.compact_keep_ratio = 0.2;
    cfg.compact_summary_tokens = 1_000;
    // Selbst der stärkste Schnitt passt nicht ins volle Fenster → Rückfall auf
    // den stärksten Schnitt überhaupt (die Tokengrößen sind Schätzungen –
    // ein Besserungsversuch ist besser als keiner).
    let msgs = turns(6, 40_000);
    let (keep, _) = decide_keep(&msgs, &cfg, CompactTrigger::Reactive, cfg.context_window)
        .expect("Rückfall liefert den stärksten Schnitt");
    assert_eq!(keep, 1, "Rückfall auf den stärksten Schnitt (keep=1)");
}

#[test]
fn kompaktierungs_protokoll_enthaelt_ausloeser_randbedingungen_und_entscheidung() {
    let mut cfg = test_config("http://127.0.0.1:1");
    cfg.context_window = 100_000;
    cfg.compact_at = 0.8;
    cfg.compact_summary_tokens = 500;
    let msgs = turns(6, 200);
    let (keep, cands) = decide_keep(&msgs, &cfg, CompactTrigger::AutoTurn, cfg.context_window)
        .expect("Schnitt vorhanden");
    let chosen = cands
        .iter()
        .find(|c| c.keep_turns == keep)
        .copied()
        .unwrap();
    let summary = "dateien gesichtet; entscheidung: refactor in zwei schritten";
    let log = CompactionLog {
        trigger: CompactTrigger::AutoTurn,
        model: "test/m".into(),
        base_url: "http://127.0.0.1:1".into(),
        url: "http://127.0.0.1:1/chat/completions".into(),
        context_window: cfg.context_window,
        compact_at: cfg.compact_at,
        keep_ratio: cfg.compact_keep_ratio,
        target_tokens: 20_000,
        summary_budget: cfg.compact_summary_tokens,
        threshold: (cfg.context_window as f64 * cfg.compact_at) as u64,
        current_tokens: 93_000,
        decided_keep: keep,
        candidates: cands,
        chosen,
        archived_msgs: 6,
        summary: summary.into(),
        summary_tokens: 17,
        overview: overview_from_wire(&msgs),
        request: serde_json::json!({"model": "m", "max_tokens": 500}),
        response: r#"{"choices":[{"message":{"content":"ok"}}]}"#.into(),
    };
    let files: std::collections::HashMap<String, String> =
        render_compaction_log(&log).into_iter().collect();
    for want in [
        "meta.txt",
        "overview.txt",
        "plan.txt",
        "summary.txt",
        "request.json",
        "response.txt",
    ] {
        assert!(files.contains_key(want), "Protokoll fehlt {want}");
    }

    let meta = &files["meta.txt"];
    // Auslöser + Randbedingungen + Entscheidung + erreichte Summary.
    assert!(meta.contains("auto (nach Turn)"), "Auslöser: {meta}");
    assert!(meta.contains("context_window:  100000"));
    assert!(meta.contains("compact_at:      0.8"));
    assert!(meta.contains("threshold:       80000"));
    assert!(meta.contains("keep_ratio:      0.2"));
    assert!(meta.contains("target_tokens:   20000"));
    assert!(meta.contains("current_tokens:  93000"));
    assert!(meta.contains(&format!("decided_keep:    {keep}")));
    assert!(
        meta.contains("summary_tokens:  17"),
        "erreichte Summary: {meta}"
    );

    // Kandidaten-Tabelle mit gewählter Markierung + Kontextgrößen
    // (wegfallend/bleibend) + Abstand zum Ziel.
    let plan = &files["plan.txt"];
    assert!(
        plan.contains(&format!("keep={keep}")),
        "gewählte Zeile: {plan}"
    );
    assert!(plan.contains("<-- gewählt"));
    assert!(plan.contains("T weg"));
    assert!(plan.contains("T bleiben"));
    assert!(plan.contains("Abstand zum Ziel"), "Zielabstand: {plan}");

    // Kurzübersicht: Rollen der Historie.
    let overview = &files["overview.txt"];
    assert!(overview.contains("user") && overview.contains("assistant"));

    // Summary mit Tokenlänge (erreichtes Ergebnis).
    let sum = &files["summary.txt"];
    assert!(sum.contains("summary_tokens: 17"));
    assert!(sum.contains("archived_msgs:  6"));
    assert!(sum.contains("dateien gesichtet"));
}

/// Lokale (identische) Fassung der Protokoll-Übersicht über die
/// Wire-Nachrichten – `message_overview` selbst ist privat.
fn overview_from_wire(msgs: &[WireMessage]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, m) in msgs.iter().enumerate() {
        let _ = writeln!(
            out,
            "[{i:3}] {:<10} {:>6} T  {}",
            m.role,
            m.tokens.total(),
            m.content.as_deref().unwrap_or_default()
        );
    }
    out
}

// ── Helfer (helpers) ──────────────────────────────────────────────────────

#[test]
fn truncate_bleibt_kurz_unveraendert() {
    assert_eq!(truncate("", 10), "");
    assert_eq!(truncate("kurz", 10), "kurz");
}

#[test]
fn with_debug_haengt_pfad_als_eigene_zeile_an() {
    assert_eq!(with_debug("Kurz".into(), None), "Kurz");
    assert!(with_debug("Kurz".into(), Some("/tmp/x.json".into())).contains("/tmp/x.json"));
}

#[test]
fn server_error_summary_extrahiert_kurze_einzeilige_meldung() {
    let raw = r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#;
    let s = server_error_summary(raw, 100);
    assert!(s.contains("bad request"));
}

#[test]
fn reasoning_contract_hint_erklaert_nur_die_einschlaegige_meldung() {
    let hint = reasoning_contract_hint("error: reasoning_content must be passed back");
    assert!(hint.contains("Note:"), "sollte den Hinweis anhängen");
    assert_eq!(
        reasoning_contract_hint("ganz anderes problem"),
        "ganz anderes problem",
        "andere Meldung bleibt unverändert"
    );
}

#[test]
fn civil_from_days_liefert_lesbares_datum() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(31), (1970, 2, 1));
}

// ── Completion-Verteilung beim Streaming (http) ────────────────────────────

#[test]
fn distribute_weights_summiert_exakt_mit_rest_im_letzten() {
    // Die Summe ist IMMER exakt `total` – Rundungsreste schluckt der letzte
    // positive Anteil. Das gilt auch bei ungleichen Gewichten (0/1/…) und 0.
    for (total, weights) in [
        (100, vec![30, 30, 40]),
        (1, vec![1, 1, 1]),
        (7, vec![2, 2, 2]),
        (5, vec![1, 0, 1]),
        (0, vec![10]),
        (9, vec![7]),
    ] {
        let out = distribute_weights(total, &weights);
        assert_eq!(out.len(), weights.len());
        assert_eq!(
            out.iter().copied().sum::<u64>(),
            total,
            "Summe exakt für {total} / {weights:?}: {out:?}"
        );
    }
}

#[test]
fn accumulator_verteilt_inkrement_proportional_und_setzt_zurueck() {
    let mut acc = RoundPartsAccumulator::default();
    // Bereich 1: viel reasoning.
    acc.track_reasoning(100);
    // usage mit Inkrement 20 → geht komplett ins reasoning.
    acc.apply_usage(20);
    // Bereich 2: reasoning nochmal + content.
    acc.track_reasoning(25);
    acc.track_content(75);
    // usage mit Inkrement 40 → 25:75 ⇒ 10|30.
    acc.apply_usage(60);
    let parts = acc.parts();
    assert_eq!((parts.reasoning, parts.content), (30, 30));
}

#[test]
fn accumulator_reset_erlaubt_mehrere_usage_events() {
    let mut acc = RoundPartsAccumulator::default();
    // Erst nur reasoning, dann nur content – je eigenes usage.
    acc.track_reasoning(50);
    acc.apply_usage(10); // 10 → reasoning
    acc.track_content(50);
    acc.apply_usage(30); // 20 → content (inc = 30-10)
    let parts = acc.parts();
    assert_eq!(parts.reasoning, 10, "reasoning bleibt exakt");
    assert_eq!(parts.content, 20, "content wird danach gemessen");
}

#[test]
fn accumulator_tool_calls_werden_pro_index_verteilt() {
    let mut acc = RoundPartsAccumulator::default();
    acc.track_content(100);
    acc.track_tool(0, 25);
    acc.track_tool(1, 75);
    // usage-Inkrement 40 über content 100 + call0 25 + call1 75 (=200):
    // content 20, call0 5, call1 15.
    acc.apply_usage(40);
    let parts = acc.parts();
    assert_eq!(parts.content, 20);
    assert_eq!(parts.tool_calls, vec![5, 15]);
    // Summe exakt.
    assert_eq!(
        parts.reasoning + parts.content + parts.tool_calls.iter().copied().sum::<u64>(),
        40
    );
}

#[test]
fn accumulator_nur_finales_usage_verteilt_ueber_ganze_runde() {
    // Server liefert nur EIN usage am Ende → alles wird über die gesamt-
    // gemessenen Bytes verteilt (≈ bisherige proportionale Aufteilung).
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(60);
    acc.track_content(40);
    acc.track_tool(0, 100);
    acc.apply_usage(200);
    let parts = acc.parts();
    // 60:40:100 über 200 ⇒ 60|40|100.
    assert_eq!(
        (parts.reasoning, parts.content, parts.tool_calls.as_slice()),
        (60, 40, &[100][..])
    );
}

#[test]
fn accumulator_null_inkrement_verteilt_nichts() {
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(10);
    acc.track_content(90);
    acc.apply_usage(0);
    let parts = acc.parts();
    assert_eq!((parts.reasoning, parts.content), (0, 0));
    // parts-Objekt ist Default (leer).
    assert!(parts.tool_calls.is_empty());
}

#[test]
fn accumulator_tool_kopf_mit_null_bytes_geht_an_aktive_sektion() {
    // Tool-Kopf-Delta (id/name, leere Argumente) erzeugt 0 messbare Bytes,
    // aber der Server zählt dafür Completion-Tokens. Über die aktive Sektion
    // landet das Inkrement trotzdem beim Tool-Call statt verloren zu gehen.
    let mut acc = RoundPartsAccumulator::default();
    acc.track_reasoning(100);
    acc.apply_usage(20); // reasoning + 20
                         // active = Reasoning; das Tool-Kopf-Delta überschreibt es auf Slot 0.
    acc.track_tool(0, 0);
    acc.apply_usage(38); // inc = 18, keine Bytes → komplett an Tool 0
    let parts = acc.parts();
    assert_eq!(parts.reasoning, 20);
    assert_eq!(parts.tool_calls, vec![18]);
}

// ── Test-Helper ───────────────────────────────────────────────────────────

/// Test-Config mit einem einzigen Provider für gegebene URL.
fn test_config(base_url: &str) -> Config {
    let mut provider = HashMap::new();
    provider.insert(
        "test".to_string(),
        ProviderConfig {
            base_url: base_url.to_string(),
            api_key: Some("test".into()),
            user_agent: None,
            force_tools: Vec::new(),
        },
    );
    Config {
        model: "test/m".into(),
        provider,
        ..Config::default()
    }
}
