//! Zentrale Verwaltung der bekannten Modelle – analog zu `ChannelRegistry`.
//!
//! Die Registry wird beim Start aus `config.models` befüllt und beim
//! Refresh (`/model` → `r`) mit den abgerufenen Modell-IDs aktualisiert.
//! Der Status jedes Modells (in Config / beim Refresh gefunden) bestimmt
//! die Farbe des `⬢`-Indikators im Modell-Picker.

use indexmap::IndexMap;
use std::collections::HashMap;

/// Status eines Modells für den farblichen `⬢`-Indikator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStatus {
    /// Sowohl in config.toml als auch beim Refresh gefunden → grün.
    ConfigAndFetched,
    /// In config.toml, aber beim Refresh nicht (mehr) gefunden → rot.
    ConfigStale,
    /// Nur beim Refresh gefunden (nicht in config.toml) → grau.
    FetchedOnly,
}

/// Eintrag in der Modell-Registry.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Provider-Name (z.B. `"openai"`).
    pub provider: String,
    /// Lokaler Alias aus config.toml oder serverseitiger Name (wenn kein Alias).
    /// Zweiter Teil des internen Schlüssels `provider/alias`.
    pub alias: String,
    /// Serverseitiger Modellname (z.B. `"gpt-4o-mini"`) – wird an LLM gesendet.
    pub server_model: String,
    /// Kontextfenster in Tokens (aus config.toml, für LLM-Konfiguration).
    pub context_window: Option<u64>,
    /// Vom Server reportete Demand-Wert (wird beim Refresh aktualisiert).
    pub demand: Option<u64>,
    /// War das Modell in der config.toml konfiguriert?
    pub in_config: bool,
    /// Wurde das Modell beim letzten Refresh gefunden?
    pub fetched: bool,
}

impl ModelEntry {
    /// Interner Schlüssel: `"provider/alias"` (eindeutig).
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.alias)
    }

    /// Anzeige im Modell-Picker: `"provider/alias (server_model) · 🔥42"`.
    pub fn display_full(&self) -> String {
        let base = if self.alias == self.server_model {
            format!("{}/{}", self.provider, self.alias)
        } else {
            format!("{}/{} ({})", self.provider, self.alias, self.server_model)
        };
        match self.demand {
            Some(d) if d > 0 => format!("{base} · {d}"),
            _ => base,
        }
    }

    /// Anzeige in Statusleiste/Signatur: `"provider/alias"`.
    pub fn display_key(&self) -> String {
        format!("{}/{}", self.provider, self.alias)
    }

    /// Modellname, wie er an den LLM-Server gesendet wird.
    pub fn api_model(&self) -> &str {
        &self.server_model
    }
}

/// Zentrale Verwaltung der bekannten Modelle.
pub struct ModelRegistry {
    /// Einträge, geordnet nach Einfüge-Reihenfolge (config zuerst, dann refresh).
    /// Schlüssel ist der interne Key `"provider/alias"`.
    entries: IndexMap<String, ModelEntry>,
    /// Deduplizierungs-Index: `(provider, server_model)` → interner Key.
    /// Beim Refresh wird geprüft, ob ein serverseitiger Name schon unter einem
    /// (ggf. anderen) Alias existiert.
    dedup_idx: HashMap<(String, String), String>,
    /// Hat至少ein Refresh stattgefunden? Erst dann werden Farben angezeigt.
    refreshed: bool,
}

impl ModelRegistry {
    /// Erzeugt die Registry aus den konfigurierten Modellen.
    pub fn new(config_models: &IndexMap<String, crate::config::ModelConfig>) -> Self {
        let mut entries = IndexMap::new();
        let mut dedup_idx = HashMap::new();

        for (alias, mc) in config_models {
            let id = mc.id();
            let (provider, server_model) = match id.split_once('/') {
                Some((p, m)) => (p.to_string(), m.to_string()),
                None => (String::new(), id.to_string()),
            };
            let key = format!("{}/{}", provider, alias);
            let entry = ModelEntry {
                provider,
                alias: alias.clone(),
                server_model,
                context_window: mc.context_window(),
                demand: None,
                in_config: true,
                fetched: false,
            };
            dedup_idx.insert(
                (entry.provider.clone(), entry.server_model.clone()),
                key.clone(),
            );
            entries.insert(key, entry);
        }

        ModelRegistry {
            entries,
            dedup_idx,
            refreshed: false,
        }
    }

    /// Aktualisiert den Fetch-Status aller Modelle nach einem Refresh.
    /// `fetched` ist eine Liste von `(modell_id, demand)`.
    /// Für jeden Server-seitigen Namen `(provider, model)` wird geprüft, ob
    /// ein Eintrag existiert – falls ja, wird Status + Demand aktualisiert.
    pub fn apply_refresh(&mut self, fetched: &[(String, Option<u64>)]) {
        // Alle bisherigen fetched-Flags zurücksetzen, Demand behalten (falls
        // erneut nicht geliefert).
        for entry in self.entries.values_mut() {
            entry.fetched = false;
        }

        for (full_id, demand) in fetched {
            let (provider, server_model) = match full_id.split_once('/') {
                Some((p, m)) => (p.to_string(), m.to_string()),
                None => continue,
            };

            if let Some(existing_key) = self
                .dedup_idx
                .get(&(provider.clone(), server_model.clone()))
            {
                if let Some(entry) = self.entries.get_mut(existing_key) {
                    entry.fetched = true;
                    entry.demand = *demand;
                }
            } else {
                let alias = server_model.clone();
                let key = format!("{}/{}", provider, alias);
                let entry = ModelEntry {
                    provider,
                    alias,
                    server_model,
                    context_window: None,
                    demand: *demand,
                    in_config: false,
                    fetched: true,
                };
                self.dedup_idx.insert(
                    (entry.provider.clone(), entry.server_model.clone()),
                    key.clone(),
                );
                self.entries.insert(key, entry);
            }
        }
        self.refreshed = true;
    }

    /// Farblicher Status eines Modells (für den `⬢`-Indikator).
    /// `None` wenn noch kein Refresh stattgefunden hat.
    pub fn status(&self, key: &str) -> Option<ModelStatus> {
        if !self.refreshed {
            return None;
        }
        match self.entries.get(key) {
            Some(e) if e.in_config && e.fetched => Some(ModelStatus::ConfigAndFetched),
            Some(e) if e.in_config => Some(ModelStatus::ConfigStale),
            Some(e) if e.fetched => Some(ModelStatus::FetchedOnly),
            _ => None,
        }
    }

    /// Stellt sicher, dass alle Modelle aus `config.models` in der Registry
    /// eingetragen sind. Neue Einträge werden angefügt; bestehende bleiben
    /// unverändert (Refresh-Status erhalten).
    pub fn sync_from_config(
        &mut self,
        config_models: &IndexMap<String, crate::config::ModelConfig>,
    ) {
        for (alias, mc) in config_models {
            let id = mc.id();
            let (provider, server_model) = match id.split_once('/') {
                Some((p, m)) => (p.to_string(), m.to_string()),
                None => (String::new(), id.to_string()),
            };

            // Prüfen ob schon vorhanden (über Alias oder Dedup-Key).
            if self.get_by_alias(alias).is_some() {
                continue; // Alias bereits vorhanden.
            }

            let key = format!("{}/{}", provider, alias);
            let entry = ModelEntry {
                provider,
                alias: alias.clone(),
                server_model,
                context_window: mc.context_window(),
                demand: None,
                in_config: true,
                fetched: false,
            };
            self.dedup_idx.insert(
                (entry.provider.clone(), entry.server_model.clone()),
                key.clone(),
            );
            self.entries.insert(key, entry);
        }
    }

    /// Lookup nach internem Key `"provider/alias"`.
    pub fn get(&self, key: &str) -> Option<&ModelEntry> {
        self.entries.get(key)
    }

    /// Lookup nach config-Alias (für rückwärtskompatiblen `/model <alias>`).
    /// Liefert den ersten Eintrag, dessen Alias übereinstimmt.
    pub fn get_by_alias(&self, alias: &str) -> Option<&ModelEntry> {
        self.entries.values().find(|e| e.alias == alias)
    }

    /// Findet einen Eintrag anhand von `"provider/server_model"` (dem
    /// Modell-ID-Format aus config.toml).
    pub fn find_by_model_id(&self, model_id: &str) -> Option<&ModelEntry> {
        let (provider, server_model) = model_id.split_once('/')?;
        self.entries
            .values()
            .find(|e| e.provider == provider && e.server_model == server_model)
    }

    /// Liefert Referenzen auf alle Einträge in Einfüge-Reihenfolge.
    pub fn all(&self) -> impl Iterator<Item = &ModelEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
