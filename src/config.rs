use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::Deserialize;

/// Konfiguration eines Host-Pfads für den Channel Builder.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PathEntry {
    /// Optionales Image für Podman-Kanäle zu diesem Pfad.
    #[serde(default)]
    pub image: Option<String>,
}

/// UID-/GID-Mapping, mit dem ein Podman-Run-Container gestartet wird.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PodmanUserMapping {
    /// Bisheriges Verhalten: `--userns=keep-id`.
    #[default]
    KeepId,
    /// Explizite `--uidmap`/`--gidmap` statt `--userns=keep-id`. Die
    /// Gast-UID/-GID stammt aus dem Image (beim Kanal-Aufbau per
    /// `podman run --rm <image> id` erfragt) und wird für Container-Start
    /// und exec (`--user`) verwendet.
    Uidmap,
}

/// Top-Level-Podman-Einstellungen.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct PodmanConfig {
    /// UID/GID-Mapping beim Container-Start (Default: `keep-id`).
    #[serde(default)]
    pub usermapping: PodmanUserMapping,
}

/// Beschreibung eines Kanals (Schnittstelle zum Dateisystem/der Shell).
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    /// `"podman"` (Container) oder `"local"` (direkt auf dem Host).
    #[serde(rename = "type")]
    pub kind: String,
    /// Run-Modus: Container-Basis-Image (`node:22`, …).
    #[serde(default)]
    pub image: Option<String>,
    /// Attach-Modus: Name einer bereits laufenden Verbindung.
    #[serde(default)]
    pub container: Option<String>,
    /// Run-Modus: expliziter Container-Name. Default (None):
    /// `aidev-<sanitisierter Kanalname>` – vom Channel Builder gesetzt auf
    /// `aidev-<reponame>-<branchname>-<imagename>` bzw. ohne Repo
    /// `aidev-<hostfoldername>-<imagename>`.
    #[serde(default)]
    pub run_container: Option<String>,
    /// Arbeitsverzeichnis im Container.
    #[serde(default = "default_workdir")]
    pub workdir: String,
    /// Host-Pfad des Workspace, der (versehentlich) gemountet ist – für die
    /// Datei-Operationen direkt über den Host.
    #[serde(default)]
    pub host_root: Option<String>,
    /// Schreibbarer `$HOME` für Werkzeug-Prozesse im Container (Run-Modus,
    /// z. B. npm/cargo-Caches). Default: `/tmp`.
    #[serde(default)]
    pub home: Option<String>,
}

impl ChannelConfig {
    /// Effektiver `$HOME` für Run-Container (Default `/tmp`).
    pub fn run_home(&self) -> String {
        match self.home.as_deref() {
            Some(h) if !h.trim().is_empty() => h.trim().to_string(),
            _ => "/tmp".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider + Modell-Konfiguration
// ---------------------------------------------------------------------------

/// Ein Provider (OpenAI, Anthropic, Ollama …): Endpunkt + optionaler Key.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optionaler HTTP `User-Agent` für Requests an diesen Provider. Fehlt er,
    /// wird ein generischer Default verwendet (für den zen-Provider
    /// `opencode-compatible aidev/{version}`, sonst `aidev/{version}`).
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// App-Version, automatisch aus Cargo übernommen (`package.version` in
/// Cargo.toml; aktuell z. B. `0.1.0`).
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Effektiver HTTP `User-Agent` für Requests an einen Provider:
/// - expliziter `user_agent` aus der Config (falls gesetzt),
/// - sonst für den zen-Provider `opencode-compatible aidev/{VERSION}`,
/// - sonst generisch `aidev/{VERSION}`.
pub(crate) fn effective_user_agent(provider: &ProviderConfig, provider_name: &str) -> String {
    match &provider.user_agent {
        Some(ua) if !ua.trim().is_empty() => ua.trim().to_string(),
        _ if provider_name == "zen" => format!("opencode-compatible aidev/{VERSION}"),
        _ => format!("aidev/{VERSION}"),
    }
}

/// Ein benanntes Modell in `[models.<alias>]`. Die Modell-ID enthält immer
/// den Provider-Prefix (`"openai/gpt-4o"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelConfig {
    /// `[models] fast = "openai/gpt-4o-mini"` – nur die Modell-ID.
    Plain(String),
    /// `[models.smart] id = "openai/gpt-4o"` mit optionalem context_window.
    Full {
        id: String,
        #[serde(default)]
        context_window: Option<u64>,
    },
}

impl ModelConfig {
    /// Die über die Leitung gehende Modell-ID (inkl. Provider-Prefix).
    pub fn id(&self) -> &str {
        match self {
            ModelConfig::Plain(id) => id,
            ModelConfig::Full { id, .. } => id,
        }
    }

    /// Ohne `/`-Aufteilung nicht sinnvoll – nur in Tests gebraucht.
    #[cfg(test)]
    pub fn provider_name(&self) -> Option<&str> {
        self.id().split_once('/').map(|(p, _)| p)
    }

    /// Ohne `/`-Aufteilung nicht sinnvoll – nur in Tests gebraucht.
    #[cfg(test)]
    pub fn model_name(&self) -> &str {
        self.id()
            .split_once('/')
            .map(|(_, m)| m)
            .unwrap_or(self.id())
    }

    /// Optionales Kontextfenster dieses Modells in Tokens.
    pub fn context_window(&self) -> Option<u64> {
        match self {
            ModelConfig::Plain(_) => None,
            ModelConfig::Full { context_window, .. } => *context_window,
        }
    }
}

// ---------------------------------------------------------------------------
// ResolvedEndpoint – vollständig aufgelöste Request-Informationen
// ---------------------------------------------------------------------------

/// Völlig aufgelöste Verbindungsinformationen für einen API-Request.
/// Wird einmalig aufgelöst und dann an HTTP-Funktionen übergeben.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// Vollständige Modell-ID inkl. Provider-Prefix (z.B. `"openai/gpt-4o"`).
    /// Wird für Anzeige in UI und Signatur verwendet.
    pub model: String,
    /// Modellname ohne Provider-Prefix (z.B. `"gpt-4o"`).
    /// Wird im JSON-Request an den LLM-Server geschickt.
    pub api_model: String,
    /// Basis-URL des Providers (ohne Trailing-Slash).
    pub base_url: String,
    /// API-Key oder leerer String bei lokalen Diensten.
    pub api_key: String,
    /// Effektiver HTTP `User-Agent` für Requests an diesen Provider (aus der
    /// Config, sonst zen-/generischer Default).
    pub user_agent: String,
    /// Optionales Kontextfenster (nur für Kompaktierungsschwelle).
    pub context_window: u64,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn default_workdir() -> String {
    "/app".to_string()
}

fn default_timeout_secs() -> u64 {
    500
}

/// Darstellung von Status-Emoji (✅, ❌, ⚠️ …) in der Konsole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolMode {
    /// Ersetzen durch universell verfügbare Breite-1-Symbole (✓, ✗, ● …) mit
    /// semantischer Farbe – konsistent unabhängig von der Terminal-Schriftart.
    #[default]
    Glyph,
    /// Original-Emoji behalten (Terminal muss die Glyphen mitbringen).
    Emoji,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Default-Modell (immer `"provider/name"`).
    #[serde(default = "default_model")]
    pub model: String,
    /// Provider-Definitionen: Schlüssel = Name, der als Prefix in Modell-IDs dient.
    #[serde(default)]
    pub provider: HashMap<String, ProviderConfig>,
    /// Maximalschutzwert an Werkzeug-Runden pro Turn. Bei Erreichen bekommt
    /// das Modell eine letzte Runde OHNE Werkzeuge mit der Aufforderung,
    /// abschließend zu antworten (statt hart abgebrochen zu werden).
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: u64,
    /// Timeout für Kommandos in Sekunden – **global**, gilt einheitlich für
    /// alle Kanäle (nicht mehr pro Kanal konfigurierbar).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Optionaler Kanal-Name, den neue Sessions anlegen.
    #[serde(default)]
    pub default_channel: Option<String>,
    /// Benannte Kanäle (`[channel.<name>]`), wählbar pro Session.
    #[serde(default, rename = "channel")]
    pub channels: HashMap<String, ChannelConfig>,
    /// Benannte Modelle (`[models.<alias>]`), wählbar pro Session per `/model`.
    /// Reihenfolge entspricht der config.toml.
    #[serde(default)]
    pub models: IndexMap<String, ModelConfig>,
    /// Darstellung der Status-Emoji: `"glyph"` (Standard) oder `"emoji"`.
    #[serde(default)]
    pub symbols: SymbolMode,
    /// Größe des Modell-Kontextfensters in Tokens – dient nur als Schwelle
    /// für die automatische Kompaktierung, nicht als hartes Limit.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Anteil von `context_window`, ab dem die Historie automatisch
    /// zusammengefasst wird (0.8 = ab 80 % Auslastung).
    #[serde(default = "default_compact_at")]
    pub compact_at: f64,
    /// Anzahl der letzten Turns (User+Assistant), die bei der Kompaktierung
    /// unangetastet bleiben – die Zusammenfassung ersetzt nur ältere
    /// Nachrichten.
    #[serde(default = "default_compact_keep_turns")]
    pub compact_keep_turns: usize,
    /// Token-Budget (`max_tokens`) für die Zusammenfassung – Obergrenze,
    /// nicht Zielgröße.
    #[serde(default = "default_compact_summary_tokens")]
    pub compact_summary_tokens: u64,
    /// Bei `context_length`-Fehlern einmalig automatisch komprimieren und die
    /// Anfrage wiederholen.
    #[serde(default = "default_compact_auto")]
    pub compact_auto: bool,
    /// Maus-Reporting aktivieren (Scroll-Rad im Chat).  Wenn `true`, meldet
    /// das Terminal Maus-Events an die Anwendung (`\x1b[?1000h`), was das
    /// Scrollen mit dem Mausrad ermöglicht.  Der Nachteil: Textmarkierung und
    /// Einfügen mit der mittleren Maustaste funktionieren dann in vielen
    /// Terminals nicht mehr, weil die Button-Events abgefangen werden.  Default:
    /// `false` (Markierung + Paste funktionieren, Scroll via PgUp/PgDn).
    #[serde(default)]
    pub mouse: bool,
    /// Host-Pfade für den Channel Builder (`[path "..."]` in der Config).
    #[serde(default, rename = "path")]
    pub paths: HashMap<String, PathEntry>,
    /// Podman-einstellungen (Top-Level, z. B. UID-Mapping beim Container-Start).
    #[serde(default)]
    pub podman: PodmanConfig,
}

fn default_model() -> String {
    "zen/big-pickle".to_string()
}

fn default_max_tool_rounds() -> u64 {
    100
}

fn default_context_window() -> u64 {
    200_000
}

fn default_compact_at() -> f64 {
    0.8
}

fn default_compact_keep_turns() -> usize {
    3
}

fn default_compact_summary_tokens() -> u64 {
    4_000
}

fn default_compact_auto() -> bool {
    true
}

fn default_provider() -> HashMap<String, ProviderConfig> {
    let mut m = HashMap::new();
    m.insert(
        "zen".to_string(),
        ProviderConfig {
            base_url: "https://opencode.ai/zen/v1".to_string(),
            api_key: Some("public".to_string()),
            user_agent: None,
        },
    );
    m
}

fn default_models() -> IndexMap<String, ModelConfig> {
    let mut m = IndexMap::new();
    m.insert(
        "pig-pickle".to_string(),
        ModelConfig::Plain("zen/big-pickle".to_string()),
    );
    m
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: default_model(),
            provider: default_provider(),
            max_tool_rounds: default_max_tool_rounds(),
            timeout_secs: default_timeout_secs(),
            default_channel: None,
            channels: HashMap::new(),
            models: default_models(),
            symbols: SymbolMode::Glyph,
            context_window: default_context_window(),
            compact_at: default_compact_at(),
            compact_keep_turns: default_compact_keep_turns(),
            compact_summary_tokens: default_compact_summary_tokens(),
            compact_auto: default_compact_auto(),
            mouse: false,
            paths: HashMap::new(),
            podman: PodmanConfig::default(),
        }
    }
}

impl Config {
    /// Lädt die Config – Präzedenz: $XDG_CONFIG_HOME, ~/.config/…, sonst Defaults.
    /// Eine `config.toml` im aktuellen Verzeichnis wird bewusst NICHT geladen:
    /// fremde Repositories könnten sie einschleusen und so Endpunkt/Schlüssel
    /// und Kanäle (inkl. Host-Zugriff) unbemerkt umbiegen.
    pub fn load() -> Self {
        for path in candidate_paths() {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            match toml::from_str::<Self>(&content) {
                Ok(cfg) => {
                    eprintln!("[aidev] Config loaded: {}", path.display());
                    return cfg.with_defaults();
                }
                Err(err) => eprintln!(
                    "[aidev] Warning: {} unreadable ({}), trying next source…",
                    path.display(),
                    err
                ),
            }
        }
        eprintln!("[aidev] Keine Config gefunden, nutze Defaults.");
        Config::default()
    }

    /// Liste der konfigurierten Modell-Aliase in config.toml-Reihenfolge.
    pub fn model_names(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// Kontextfenster für einen Alias – Override des Eintrags oder der globale Wert.
    pub fn context_window_for(&self, alias: Option<&str>) -> u64 {
        match alias.and_then(|a| self.models.get(a)) {
            Some(entry) => entry
                .context_window()
                .filter(|w| *w > 0)
                .unwrap_or(self.context_window),
            None => self.context_window,
        }
    }

    /// Löst einen Modell-Alias (oder den Default) zu einem vollständigen
    /// `ResolvedEndpoint` auf. Der Provider-Name wird aus der Modell-ID
    /// extrahiert und die Verbindungsinformationen aus `[provider.<name>]`
    /// geholt.
    pub fn resolve(&self, alias: Option<&str>) -> Result<ResolvedEndpoint, String> {
        let model_id = match alias {
            Some(a) => self
                .models
                .get(a)
                .ok_or_else(|| format!("Unbekannter Modell-Alias: {a}"))?
                .id(),
            None => &self.model,
        };
        self.resolve_id(model_id)
    }

    /// Löst eine direkte Modell-ID (z.B. `"openai/gpt-4o"`) zu einem
    /// `ResolvedEndpoint` auf – unabhängig von Aliassen.
    pub fn resolve_id(&self, model_id: &str) -> Result<ResolvedEndpoint, String> {
        let (provider_name, model_name) = model_id
            .split_once('/')
            .ok_or_else(|| format!("Modell-ID muss Format 'provider/name' haben: {model_id}"))?;

        let provider = self.provider.get(provider_name).ok_or_else(|| {
            format!("Unbekannter Provider '{provider_name}' in Modell '{model_id}'")
        })?;

        let model_cfg = self.models.values().find(|m| m.id() == model_id);

        Ok(ResolvedEndpoint {
            model: model_id.to_string(),
            api_model: model_name.to_string(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            api_key: provider.api_key.clone().unwrap_or_default(),
            user_agent: effective_user_agent(provider, provider_name),
            context_window: model_cfg
                .and_then(|m| m.context_window())
                .filter(|w| *w > 0)
                .unwrap_or(self.context_window),
        })
    }

    /// Ersetzt leere Felder durch Defaults.
    fn with_defaults(self) -> Self {
        let d = Config::default();
        Config {
            model: if self.model.trim().is_empty() {
                d.model
            } else {
                self.model.trim().to_string()
            },
            provider: self.provider,
            max_tool_rounds: if self.max_tool_rounds == 0 {
                d.max_tool_rounds
            } else {
                self.max_tool_rounds
            },
            timeout_secs: if self.timeout_secs == 0 {
                d.timeout_secs
            } else {
                self.timeout_secs
            },
            default_channel: self.default_channel.filter(|c| !c.trim().is_empty()),
            channels: self.channels,
            models: self.models,
            symbols: self.symbols,
            context_window: if self.context_window == 0 {
                d.context_window
            } else {
                self.context_window
            },
            compact_at: if self.compact_at <= 0.0 || self.compact_at > 1.0 {
                d.compact_at
            } else {
                self.compact_at
            },
            compact_keep_turns: if self.compact_keep_turns == 0 {
                d.compact_keep_turns
            } else {
                self.compact_keep_turns
            },
            compact_summary_tokens: if self.compact_summary_tokens == 0 {
                d.compact_summary_tokens
            } else {
                self.compact_summary_tokens
            },
            compact_auto: self.compact_auto,
            mouse: self.mouse,
            paths: self.paths,
            podman: self.podman,
        }
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            paths.push(PathBuf::from(xdg).join("aidev").join("config.toml"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("aidev")
                .join("config.toml"),
        );
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        toml::from_str(
            r#"
            model = "opencode/big-pickle"

            [provider.opencode]
            base_url = "https://opencode.ai/zen/v1"
            api_key  = "public"

            [provider.openai]
            base_url = "https://api.openai.com/v1"
            api_key  = "sk-test"

            [provider.ollama]
            base_url = "http://localhost:11434/v1"

            [models]
            fast = "openai/gpt-4o-mini"

            [models.smart]
            id = "opencode/big-pickle"
            context_window = 131072

            [models.local]
            id = "ollama/llama3"

            [models.code]
            id = "openai/o3"
        "#,
        )
        .expect("Test-Config lesbar")
    }

    #[test]
    fn max_tool_runden_hat_default_und_lasst_sich_setzen() {
        let cfg: Config = toml::from_str("").expect("leere TOML nutzt Defaults");
        assert_eq!(cfg.max_tool_rounds, 100, "Default");
        let cfg2: Config = toml::from_str("max_tool_rounds = 4").expect("TOML lesbar");
        assert_eq!(cfg2.max_tool_rounds, 4, "konfigurierbar");
        assert_eq!(cfg2.with_defaults().max_tool_rounds, 4);
        let zero: Config = toml::from_str("max_tool_rounds = 0").expect("TOML lesbar");
        assert_eq!(zero.with_defaults().max_tool_rounds, 100, "0 -> Default");
    }

    #[test]
    fn kanal_aus_toml_geparst() {
        let toml_str = r#"
            default_channel = "build"

            [provider.opencode]
            base_url = "https://example.com/v1"

            [channel.build]
            type = "podman"
            image = "node:22"
            host_root = "/home/me/projekt"
            workdir = "/app"

            [channel.ci]
            type = "podman"
            container = "aidev-ws"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("TOML lesbar");
        assert_eq!(cfg.default_channel.as_deref(), Some("build"));
        let build = &cfg.channels["build"];
        assert_eq!(build.kind, "podman");
        assert_eq!(build.image.as_deref(), Some("node:22"));
        assert_eq!(build.workdir, "/app");
        let ci = &cfg.channels["ci"];
        assert_eq!(ci.container.as_deref(), Some("aidev-ws"));
        assert!(ci.image.is_none());
        assert_eq!(ci.workdir, "/app");
        assert_eq!(cfg.timeout_secs, 500, "globaler Default");
    }

    #[test]
    fn symbol_modus_laesst_sich_konfigurieren() {
        let cfg: Config = toml::from_str("").expect("leere TOML nutzt Defaults");
        assert_eq!(cfg.symbols, SymbolMode::Glyph, "Default: Glyph");

        let glyph: Config = toml::from_str("symbols = \"glyph\"").expect("TOML lesbar");
        assert_eq!(glyph.symbols, SymbolMode::Glyph);

        let emoji: Config = toml::from_str("symbols = \"emoji\"").expect("TOML lesbar");
        assert_eq!(emoji.symbols, SymbolMode::Emoji);

        assert!(toml::from_str::<Config>("symbols = \"moji\"").is_err());
        assert!(toml::from_str::<Config>("symbols = 7").is_err());
    }

    #[test]
    fn kompaktierung_defaults_sind_sinnvoll() {
        let cfg: Config = toml::from_str("").expect("leere TOML nutzt Defaults");
        assert_eq!(cfg.context_window, 200_000);
        assert!((cfg.compact_at - 0.8).abs() < 1e-9);
        assert_eq!(cfg.compact_keep_turns, 3);
        assert_eq!(cfg.compact_summary_tokens, 4_000);
        assert!(cfg.compact_auto);

        let nulldaten: Config =
            toml::from_str("context_window = 0\ncompact_at = 2\ncompact_keep_turns = 0\ncompact_summary_tokens = 0\ncompact_auto = false")
                .expect("TOML lesbar");
        let d = nulldaten.with_defaults();
        assert_eq!(d.context_window, 200_000, "0 -> Default");
        assert!(
            (d.compact_at - 0.8).abs() < 1e-9,
            "außerhalb (0,1] -> Default"
        );
        assert_eq!(d.compact_keep_turns, 3, "0 -> Default");
        assert_eq!(d.compact_summary_tokens, 4_000, "0 -> Default");
        assert!(!d.compact_auto, "false bleibt false (kein Default-Zwang)");

        let gesetzt: Config = toml::from_str(
            "context_window = 32000\ncompact_at = 0.9\ncompact_keep_turns = 5\ncompact_summary_tokens = 999\ncompact_auto = false",
        )
        .expect("TOML lesbar");
        assert_eq!(gesetzt.context_window, 32_000);
        assert!((gesetzt.compact_at - 0.9).abs() < 1e-9);
        assert_eq!(gesetzt.compact_keep_turns, 5);
        assert_eq!(gesetzt.compact_summary_tokens, 999);
        assert!(!gesetzt.compact_auto);
    }

    #[test]
    fn path_entries_aus_toml_geparst() {
        let cfg: Config = toml::from_str("").expect("leere TOML nutzt Defaults");
        assert!(cfg.paths.is_empty(), "Default: leer");

        let toml_str = r#"
            [provider.opencode]
            base_url = "https://example.com/v1"

            [path."/home/me/projekt"]
            image = "node:22"

            [path."/tmp/worktree-test"]
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("TOML lesbar");
        assert_eq!(cfg.paths.len(), 2);
        let proj = &cfg.paths["/home/me/projekt"];
        assert_eq!(proj.image.as_deref(), Some("node:22"));
        let tmp = &cfg.paths["/tmp/worktree-test"];
        assert!(tmp.image.is_none(), "ohne Image");
    }

    #[test]
    fn podman_usermapping_default_und_werte() {
        // Kein [podman]-Block → Default keep-id.
        let cfg: Config = toml::from_str("").expect("leere TOML");
        assert_eq!(
            cfg.podman.usermapping,
            crate::config::PodmanUserMapping::KeepId
        );
        // Explizit uidmap.
        let cfg: Config = toml::from_str(
            r#"
            [podman]
            usermapping = "uidmap"
            "#,
        )
        .expect("TOML lesbar");
        assert_eq!(cfg.podman.usermapping, crate::config::PodmanUserMapping::Uidmap);
        // Explizit keep-id.
        let cfg: Config = toml::from_str(
            r#"
            [podman]
            usermapping = "keep-id"
            "#,
        )
        .expect("TOML lesbar");
        assert_eq!(
            cfg.podman.usermapping,
            crate::config::PodmanUserMapping::KeepId
        );
        // Unbekannter Wert → Parse-Fehler.
        assert!(toml::from_str::<Config>(
            r#"
            [podman]
            usermapping = "bogus"
            "#
        )
        .is_err());
    }

    #[test]
    fn modelle_aus_toml_geparst_kurz_und_lang() {
        let cfg = test_config();
        assert_eq!(
            cfg.model_names(),
            vec![
                "fast".to_string(),
                "smart".to_string(),
                "local".to_string(),
                "code".to_string(),
            ],
            "Aliase in config.toml-Reihenfolge"
        );
        assert_eq!(cfg.models["fast"].id(), "openai/gpt-4o-mini", "Kurzschrift");
        assert_eq!(cfg.models["fast"].context_window(), None);
        assert_eq!(cfg.models["fast"].provider_name(), Some("openai"));
        assert_eq!(cfg.models["fast"].model_name(), "gpt-4o-mini");

        let smart = &cfg.models["smart"];
        assert_eq!(smart.id(), "opencode/big-pickle");
        assert_eq!(smart.context_window(), Some(131_072));
        assert_eq!(smart.provider_name(), Some("opencode"));

        let local = &cfg.models["local"];
        assert_eq!(local.id(), "ollama/llama3");
        assert_eq!(local.provider_name(), Some("ollama"));
    }

    #[test]
    fn resolve_station() {
        let cfg = test_config();

        // Default-Modell
        let ep = cfg.resolve(None).expect("Default-Modell auflösbar");
        assert_eq!(ep.model, "opencode/big-pickle");
        assert_eq!(ep.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(ep.api_key, "public");

        // Alias mit eigenem Provider
        let ep = cfg.resolve(Some("local")).expect("Alias auflösbar");
        assert_eq!(ep.model, "ollama/llama3");
        assert_eq!(ep.base_url, "http://localhost:11434/v1");
        assert_eq!(ep.api_key, "", "Ollama ohne Key");

        // Alias mit context_window Override
        let ep = cfg.resolve(Some("smart")).expect("Smart auflösbar");
        assert_eq!(ep.context_window, 131_072);

        // Alias ohne context_window → globaler Default
        let ep = cfg.resolve(Some("fast")).expect("Fast auflösbar");
        assert_eq!(ep.context_window, 200_000);

        // Unbekannter Alias
        assert!(cfg.resolve(Some("nope")).is_err());

        // Direkte ID
        let ep = cfg
            .resolve_id("openai/gpt-4o")
            .expect("Direkte ID auflösbar");
        assert_eq!(ep.base_url, "https://api.openai.com/v1");
        assert_eq!(ep.api_key, "sk-test");

        // Unbekannter Provider
        assert!(cfg.resolve_id("huggingface/llama3").is_err());

        // ID ohne Provider-Prefix
        assert!(cfg.resolve_id("gpt-4o").is_err());
    }

    #[test]
    fn model_auswahl_resolved_ueberschreibt_nur_gesetztes() {
        let toml_str = r#"
            model = "opencode/default-m"

            [provider.opencode]
            base_url = "https://opencode.example.com/v1"
            api_key = "global"

            [provider.ollama]
            base_url = "http://localhost:11434/v1"

            [models.fast]
            id = "opencode/qwen-coder"
            context_window = 32000

            [models.local]
            id = "ollama/llama3"
        "#;
        let cfg: Config = toml::from_str(toml_str).expect("TOML lesbar");

        // Ohne Alias: Default
        let ep = cfg.resolve(None).expect("resolve");
        assert_eq!(ep.model, "opencode/default-m");
        assert_eq!(ep.base_url, "https://opencode.example.com/v1");

        // Kurzeintrag: ID + Kontextfenster-Override
        let ep = cfg.resolve(Some("fast")).expect("resolve fast");
        assert_eq!(ep.model, "opencode/qwen-coder");
        assert_eq!(ep.context_window, 32_000);
        assert_eq!(ep.api_key, "global");

        // Tabelleneintrag: Ollama ohne Key
        let ep = cfg.resolve(Some("local")).expect("resolve local");
        assert_eq!(ep.model, "ollama/llama3");
        assert_eq!(ep.api_key, "");
        assert_eq!(ep.context_window, 200_000, "globaler Default");

        // Unbekannter Alias → Fehler
        assert!(cfg.resolve(Some("nope")).is_err());
    }

    #[test]
    fn default_model_ist_provider_name斜线() {
        let cfg: Config = toml::from_str("").expect("leere TOML nutzt Defaults");
        assert!(
            cfg.model.contains('/'),
            "Default-Modell muss Provider-Prefix haben: {}",
            cfg.model
        );
    }

    #[test]
    fn default_config_hat_zen_provider_und_pig_pickle_modell() {
        let cfg = Config::default();
        // Default-Modell ist zen/big-pickle
        assert_eq!(cfg.model, "zen/big-pickle");
        // Provider "zen" ist vorhanden und korrekt konfiguriert
        let zen = cfg
            .provider
            .get("zen")
            .expect("Provider 'zen' muss in Default vorhanden sein");
        assert_eq!(zen.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(zen.api_key.as_deref(), Some("public"));
        // Modell-Alias "pig-pickle" zeigt auf zen/big-pickle
        assert_eq!(cfg.models["pig-pickle"].id(), "zen/big-pickle");
        // max_tool_rounds und timeout
        assert_eq!(cfg.max_tool_rounds, 100);
        assert_eq!(cfg.timeout_secs, 500);
        // Default-Modell ist auflösbar
        let ep = cfg.resolve(None).expect("Default-Modell auflösbar");
        assert_eq!(ep.model, "zen/big-pickle");
        assert_eq!(ep.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(ep.api_key, "public");
    }

    #[test]
    fn user_agent_defaults_zen_und_generisch() {
        // zen ohne Config-User-Agent → zen-spezifischer Default.
        let zen = ProviderConfig {
            base_url: "x".into(),
            api_key: None,
            user_agent: None,
        };
        assert_eq!(
            effective_user_agent(&zen, "zen"),
            format!("opencode-compatible aidev/{VERSION}")
        );
        // anderer Provider ohne Config-User-Agent → generischer Default.
        let other = ProviderConfig {
            base_url: "x".into(),
            api_key: None,
            user_agent: None,
        };
        assert_eq!(
            effective_user_agent(&other, "openai"),
            format!("aidev/{VERSION}")
        );
        // expliziter Config-User-Agent hat Vorrang (auch bei zen).
        let custom = ProviderConfig {
            base_url: "x".into(),
            api_key: None,
            user_agent: Some("mein-agent/1.0".into()),
        };
        assert_eq!(effective_user_agent(&custom, "zen"), "mein-agent/1.0");
        // leerer/whitespace-Config-Wert fällt auf den Default zurück.
        let blank = ProviderConfig {
            base_url: "x".into(),
            api_key: None,
            user_agent: Some("   ".into()),
        };
        assert_eq!(
            effective_user_agent(&blank, "zen"),
            format!("opencode-compatible aidev/{VERSION}")
        );
    }

    #[test]
    fn resolve_ubernimmt_provider_user_agent() {
        let mut cfg = Config::default();
        // expliziter User-Agent für einen nicht-zen-Provider.
        cfg.provider.insert(
            "custom".to_string(),
            ProviderConfig {
                base_url: "https://example.org".into(),
                api_key: None,
                user_agent: Some("speziell/2.0".into()),
            },
        );
        cfg.models.insert(
            "c".to_string(),
            ModelConfig::Plain("custom/modell".to_string()),
        );
        let ep = cfg.resolve(Some("c")).expect("auflösbar");
        assert_eq!(ep.user_agent, "speziell/2.0");
    }
}
