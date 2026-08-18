use std::collections::HashMap;
use std::sync::RwLock;

use crate::auth_store::AuthStore;

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub context_budget: u64,
    pub compact_threshold_ratio: f64,
    pub thinking_enabled: bool,
    pub max_output_tokens: Option<u32>,
}

pub const DEFAULT_CONFIG_PROVIDER_TYPE: &str = "openai-compat";

pub fn config_provider_types() -> Vec<&'static str> {
    let mut types = Vec::new();
    for preset in PROVIDER_PRESETS {
        if preset.provider_type == "codex" {
            continue;
        }
        if !types.contains(&preset.provider_type) {
            types.push(preset.provider_type);
        }
    }
    if types.is_empty() {
        types.push(DEFAULT_CONFIG_PROVIDER_TYPE);
    }
    types
}

#[derive(Debug, Clone, Default)]
pub struct ProviderEntry {
    pub name: String,
    pub kind: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelEntry {
    pub model: String,
    pub provider: Option<String>,
    pub context_budget: Option<u64>,
    pub compact_threshold_ratio: Option<f64>,
    pub thinking: Option<bool>,
    pub max_tokens: Option<u32>,
    pub enabled: Option<bool>,
    #[allow(dead_code)]
    pub discovered: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AliasEntry {
    pub model: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub providers: HashMap<String, ProviderEntry>,
    pub models: HashMap<String, ModelEntry>,
    pub aliases: HashMap<String, AliasEntry>,
}

/// Backwards-compatible alias — ProviderConfig is the canonical name.
pub type ModelConfig = ProviderConfig;

static MODEL_CONFIG: RwLock<Option<ProviderConfig>> = RwLock::new(None);

/// Serializes tests that mutate the global model registry.
///
/// This stays available in integration tests so they can avoid racing the
/// shared `MODEL_CONFIG` state.
pub static MODEL_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Model IDs discovered from OAuth providers (e.g. Codex).
static DISCOVERED_MODELS: RwLock<Vec<String>> = RwLock::new(Vec::new());

pub fn set_discovered_models(models: Vec<String>) {
    *DISCOVERED_MODELS.write().unwrap() = models;
}

pub fn discovered_models() -> Vec<String> {
    DISCOVERED_MODELS.read().unwrap().clone()
}

/// Set the base model configuration (from config.toml).
/// Preserves previously registered discovered models.
pub fn set_provider_config(mut cfg: ProviderConfig) {
    let mut guard = MODEL_CONFIG.write().unwrap();
    if let Some(old) = guard.take() {
        // Preserve discovered models from old config.
        for (name, entry) in old.models {
            if entry.discovered {
                cfg.models.entry(name).or_insert(entry);
            }
        }
        // Preserve aliases from old config that aren't in the new one.
        // config.toml aliases are authoritative; only preserve
        // non-config-sourced aliases (currently none exist).
    }
    *guard = Some(cfg);
}

/// Backwards-compatible alias for [set_provider_config].
pub fn set_model_config(cfg: ModelConfig) {
    set_provider_config(cfg);
}

/// Register additional model entries without clobbering existing ones.
pub fn register_model_entries(entries: Vec<(String, ModelEntry)>) {
    let mut guard = MODEL_CONFIG.write().unwrap();
    let mut cfg = guard.take().unwrap_or_default();
    for (name, entry) in entries {
        cfg.models.entry(name).or_insert(entry);
    }
    *guard = Some(cfg);
}

/// Register additional provider entries without clobbering existing ones.
pub fn register_provider_entries(entries: Vec<(String, ProviderEntry)>) {
    let mut guard = MODEL_CONFIG.write().unwrap();
    let mut cfg = guard.take().unwrap_or_default();
    for (name, entry) in entries {
        cfg.providers.entry(name).or_insert(entry);
    }
    *guard = Some(cfg);
}

/// Build ModelEntry values from discovered models and register them.
/// Models are keyed as `<provider_name>:<slug>` (e.g. `Codex:codex/gpt-5.5`).
pub fn register_discovered(
    _provider_id: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModel],
) {
    let entries: Vec<(String, ModelEntry)> = models
        .iter()
        .map(|m| {
            let name = format!("{provider_name}:{}", m.slug);
            let entry = ModelEntry {
                model: name.clone(),
                provider: Some(provider_name.to_string()),
                context_budget: m.context_budget,
                thinking: Some(m.thinking),
                enabled: None,
                discovered: true,
                ..Default::default()
            };
            (name, entry)
        })
        .collect();
    let slugs: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
    register_model_entries(entries);
    set_discovered_models(slugs);
}

#[derive(Debug, Clone)]
pub struct ModelRow {
    pub slug: String,
    pub provider_name: String,
    pub context_budget: u64,
    pub max_output_tokens: Option<u32>,
    pub thinking: bool,
}

#[derive(Debug, Clone)]
pub struct ProviderGroup {
    pub provider_name: String,
    pub models: Vec<ModelRow>,
}

/// Return all models grouped by provider, with complete metadata.
/// Single canonical source for UI — no manual union of all_model_entries +
/// discovered_models + aliases.
pub fn all_provider_groups() -> Vec<ProviderGroup> {
    let entries = all_model_entries();
    let mut groups: std::collections::BTreeMap<String, Vec<ModelRow>> =
        std::collections::BTreeMap::new();
    for (name, entry) in entries {
        let info = model_info(&name);
        let provider = entry.provider.unwrap_or_else(|| "unknown".to_string());
        let row = ModelRow {
            slug: name,
            provider_name: provider.clone(),
            context_budget: info.context_budget,
            max_output_tokens: info.max_output_tokens,
            thinking: info.thinking_enabled(),
        };
        groups.entry(provider).or_default().push(row);
    }
    groups
        .into_iter()
        .map(|(provider_name, models)| ProviderGroup {
            provider_name,
            models,
        })
        .collect()
}

pub fn resolve_alias(name: &str) -> String {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        let mut current = name.to_string();
        let mut seen = std::collections::HashSet::new();
        while let Some(entry) = cfg.aliases.get(&current) {
            if !seen.insert(current.clone()) {
                break;
            }
            current = entry.model.clone();
        }
        return current;
    }
    name.to_string()
}

pub fn model_entry(name: &str) -> Option<ModelEntry> {
    let resolved = resolve_alias(name);
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        return cfg.models.get(&resolved).cloned();
    }
    None
}

pub fn all_model_entries() -> Vec<(String, ModelEntry)> {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        return cfg
            .models
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    Vec::new()
}

pub fn is_provider_enabled(name: &str) -> bool {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        if let Some(entry) = cfg.providers.get(name) {
            return entry.enabled.unwrap_or(true);
        }
    }
    true
}

pub fn all_provider_entries() -> Vec<(String, ProviderEntry)> {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        return cfg
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    Vec::new()
}

pub fn all_aliases() -> Vec<(String, String)> {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        return cfg
            .aliases
            .iter()
            .map(|(k, v)| (k.clone(), v.model.clone()))
            .collect();
    }
    Vec::new()
}

pub fn model_info(name: &str) -> ModelInfo {
    let resolved = resolve_alias(name);
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        if let Some(entry) = cfg.models.get(&resolved) {
            let enabled = entry.enabled.unwrap_or(true);
            return ModelInfo {
                name: resolved.clone(),
                context_budget: if enabled {
                    entry.context_budget.unwrap_or(0)
                } else {
                    0
                },
                compact_threshold_ratio: entry.compact_threshold_ratio.unwrap_or(0.8),
                thinking_enabled: entry.thinking.unwrap_or(false),
                max_output_tokens: entry.max_tokens,
            };
        }
    }
    ModelInfo {
        name: resolved,
        context_budget: 0,
        compact_threshold_ratio: 0.8,
        thinking_enabled: false,
        max_output_tokens: None,
    }
}

impl ModelInfo {
    pub fn compact_threshold_tokens(&self) -> u64 {
        let reserved = self.max_output_tokens.unwrap_or(0) as u64;
        let available = self.context_budget.saturating_sub(reserved);
        (available as f64 * self.compact_threshold_ratio) as u64
    }

    pub fn compaction_trigger_threshold(&self) -> u64 {
        if self.context_budget == 0 {
            return u64::MAX;
        }
        let budget = self.context_budget;

        let configured_output = self.max_output_tokens.unwrap_or(32_000) as u64;
        let output_cap = (budget as f64 * 0.20) as u64;
        let output_reserve = configured_output.min(output_cap).max(8_000);

        let safety = (budget as f64 * 0.05) as u64;
        let safety_margin = safety.max(4_000);

        let trigger = budget
            .saturating_sub(output_reserve)
            .saturating_sub(safety_margin);
        let floor = (budget as f64 * 0.50) as u64;
        let ceiling = (budget as f64 * 0.95) as u64;
        trigger.clamp(floor, ceiling)
    }

    pub fn compaction_target_after(&self) -> u64 {
        let trigger = self.compaction_trigger_threshold();
        let budget_cap = (self.context_budget as f64 * 0.25) as u64;
        let trigger_cap = (trigger as f64 * 0.75) as u64;
        budget_cap.min(trigger_cap)
    }

    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }
}

// Known models table (supplements API discovery)

pub use crate::known_models::{KNOWN_MODELS, lookup_known_model};
/// Register preset models for a provider whose base_url matches a PROVIDER_PRESETS entry.
/// Models are marked `discovered = true` so they survive `set_model_config` reloads
/// but are never written to config.toml. User-defined models in config.toml take priority.
pub fn register_preset_models_for(provider_name: &str, base_url: &str) {
    for preset in PROVIDER_PRESETS {
        if preset.base_url == base_url && !preset.models.is_empty() {
            let entries: Vec<(String, ModelEntry)> = preset
                .models
                .iter()
                .map(|m| {
                    let name = m.id.to_string();
                    let entry = ModelEntry {
                        model: m.id.to_string(),
                        provider: Some(provider_name.to_string()),
                        context_budget: Some(m.context_budget),
                        thinking: Some(m.thinking),
                        enabled: None,
                        discovered: true,
                        ..Default::default()
                    };
                    (name, entry)
                })
                .collect();
            register_model_entries(entries);
            return;
        }
    }
}

/// Register preset models for all config providers that match a PROVIDER_PRESETS entry.
/// Called at bootstrap after `register_providers_from_config`.
pub fn register_all_preset_models() {
    let providers = all_provider_entries();
    for (name, entry) in &providers {
        if let Some(base_url) = &entry.base_url {
            register_preset_models_for(name, base_url);
        }
    }
}

// Config migration (v1 to v2)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMigrationOutcome {
    NotNeeded,
    Migrated { backup: std::path::PathBuf },
}

pub fn migrate_config_if_needed(
    text: &str,
) -> Result<Option<String>, crate::config_hub::ConfigError> {
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    let legacy = has_legacy_model_fields(&doc);
    match doc.get("config_version") {
        None if !legacy => return Ok(None),
        None => {}
        Some(version) => match version.as_integer() {
            Some(1) if legacy => {}
            Some(1) | Some(2) => return Ok(None),
            Some(value) => {
                return Err(crate::config_hub::ConfigError::Invalid(format!(
                    "unsupported config_version {value}"
                )));
            }
            None => {
                return Err(crate::config_hub::ConfigError::Invalid(
                    "config_version must be an integer".into(),
                ));
            }
        },
    }

    let provider_names: std::collections::HashSet<String> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();
    let models = doc
        .get("models")
        .and_then(|item| item.as_table())
        .ok_or_else(|| crate::config_hub::ConfigError::Invalid("models is not a table".into()))?;
    let mut groups: std::collections::BTreeMap<(String, String, String), Vec<String>> =
        std::collections::BTreeMap::new();

    for (name, entry) in models.iter() {
        let api_key = entry
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let base_url = entry
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ptype = entry
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("openai-compat")
            .to_string();

        if !api_key.is_empty()
            || !base_url.is_empty()
            || (matches!(
                ptype.as_str(),
                "openai" | "openai-compat" | "anthropic" | "codex"
            ) && !provider_names.contains(&ptype))
        {
            groups
                .entry((ptype, api_key, base_url))
                .or_default()
                .push(name.to_string());
        }
    }

    if groups.is_empty() {
        return Ok(None);
    }

    let mut used_names: std::collections::HashSet<String> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();

    for ((ptype, api_key, base_url), model_names) in &groups {
        let provider_name = pick_provider_name(ptype, base_url, &mut used_names);
        used_names.insert(provider_name.clone());

        // Create [providers.{name}] section
        let mut table = toml_edit::Table::new();
        table.insert("kind", toml_edit::value(ptype.clone()));
        if !api_key.is_empty() {
            table.insert("api_key", toml_edit::value(api_key.clone()));
        }
        if !base_url.is_empty() {
            table.insert("base_url", toml_edit::value(base_url.clone()));
        }
        table.insert("enabled", toml_edit::value(true));

        // Ensure [providers] table exists
        if doc.get("providers").is_none() {
            doc.insert("providers", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        if let Some(providers) = doc.get_mut("providers").and_then(|p| p.as_table_mut()) {
            providers.insert(&provider_name, toml_edit::Item::Table(table));
        }

        // Update each model's provider field and remove api_key/base_url
        for model_name in model_names {
            if let Some(model) = doc
                .get_mut("models")
                .and_then(|m| m.as_table_mut())
                .and_then(|t| t.get_mut(model_name.as_str()))
                .and_then(|e| e.as_table_mut())
            {
                model.insert("provider", toml_edit::value(&provider_name));
                model.remove("api_key");
                model.remove("base_url");
            }
        }
    }

    doc.insert("config_version", toml_edit::value(2i64));
    let migrated = doc.to_string();
    parse_config(&migrated).ok_or_else(|| {
        crate::config_hub::ConfigError::Invalid("validate migrated config.toml".into())
    })?;
    Ok(Some(migrated))
}

fn has_legacy_model_fields(doc: &toml_edit::DocumentMut) -> bool {
    let provider_names: std::collections::HashSet<&str> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name).collect())
        .unwrap_or_default();
    doc.get("models")
        .and_then(|item| item.as_table())
        .is_some_and(|models| {
            models.iter().any(|(_, entry)| {
                entry.get("api_key").is_some()
                    || entry.get("base_url").is_some()
                    || entry
                        .get("provider")
                        .and_then(|value| value.as_str())
                        .is_some_and(|provider| {
                            matches!(provider, "openai" | "openai-compat" | "anthropic" | "codex")
                                && !provider_names.contains(provider)
                        })
            })
        })
}

fn pick_provider_name(
    ptype: &str,
    base_url: &str,
    used: &mut std::collections::HashSet<String>,
) -> String {
    // Try to match base_url against PROVIDER_PRESETS
    for preset in PROVIDER_PRESETS {
        if !base_url.is_empty() && preset.base_url == base_url {
            let name = preset.name.to_lowercase();
            if !used.contains(&name) {
                return name;
            }
        }
    }
    // Fall back to provider type, with suffix for duplicates
    let base = ptype.to_string();
    if !used.contains(&base) {
        return base;
    }
    for i in 2.. {
        let candidate = format!("{base}-{i}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

// Alias CRUD (writes config.toml)

pub fn read_config_toml_pub() -> Option<String> {
    crate::config_hub::ConfigHub::global()
        .ok()?
        .read_config_toml()
        .ok()
}

pub(crate) fn reload_from_text(text: &str) -> anyhow::Result<()> {
    if !text.trim().is_empty() {
        toml::from_str::<toml::Value>(text)
            .map_err(|error| anyhow::anyhow!("parse config.toml: {error}"))?;
    }
    set_provider_config(parse_config(text).unwrap_or_default());
    Ok(())
}

/// Unified config parser — parses `[providers.X]`, `[models.X]`, and `[alias.X]`
/// sections from a TOML string into a [ProviderConfig].
///
/// Replaces the per-crate `parse_model_config` functions in CLI and daemon.
pub fn parse_config(text: &str) -> Option<ProviderConfig> {
    #[derive(serde::Deserialize, Default)]
    struct RawProvider {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        enabled: Option<bool>,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawModel {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        context_budget: Option<u64>,
        #[serde(default)]
        compact_threshold_ratio: Option<f64>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        discovered: bool,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawAlias {
        model: String,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawFile {
        #[serde(default)]
        providers: std::collections::HashMap<String, RawProvider>,
        #[serde(default)]
        models: std::collections::HashMap<String, RawModel>,
        #[serde(default)]
        alias: std::collections::HashMap<String, RawAlias>,
    }

    let raw: RawFile = toml::from_str(text).ok()?;
    let mut cfg = ProviderConfig::default();

    for (key, p) in raw.providers {
        cfg.providers.insert(
            key.clone(),
            ProviderEntry {
                name: p.name.unwrap_or(key),
                kind: p.kind.unwrap_or_default(),
                api_key: p.api_key,
                api_key_env: p.api_key_env,
                base_url: p.base_url,
                max_tokens: p.max_tokens,
                enabled: p.enabled,
            },
        );
    }

    for (name, m) in raw.models {
        cfg.models.insert(
            name,
            ModelEntry {
                model: m.model.unwrap_or_default(),
                provider: m.provider,
                context_budget: m.context_budget,
                compact_threshold_ratio: m.compact_threshold_ratio,
                thinking: m.thinking,
                max_tokens: m.max_tokens,
                enabled: m.enabled,
                discovered: m.discovered,
            },
        );
    }

    for (name, a) in raw.alias {
        cfg.aliases.insert(name, AliasEntry { model: a.model });
    }

    if cfg.providers.is_empty() && cfg.models.is_empty() && cfg.aliases.is_empty() {
        return None;
    }
    Some(cfg)
}

pub fn add_alias_to_config(alias: &str, model: &str) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .add_alias(alias, model)
        .map_err(Into::into)
}

pub fn upsert_provider_config(
    name: &str,
    kind: &str,
    api_key: Option<&str>,
    api_key_env: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
    enabled: bool,
) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .upsert_provider(crate::config_hub::ProviderConfigUpdate {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            enabled,
        })
        .map_err(Into::into)
}

#[derive(Debug, Clone)]
pub struct ModelConfigUpdate<'a> {
    pub old_name: Option<&'a str>,
    pub name: &'a str,
    pub model: &'a str,
    pub provider: Option<&'a str>,
    pub context_budget: u64,
    pub thinking: bool,
    pub max_tokens: Option<u32>,
    pub enabled: bool,
}

pub(crate) fn apply_model_config_update(
    doc: &mut toml_edit::DocumentMut,
    update: ModelConfigUpdate<'_>,
) -> anyhow::Result<()> {
    if doc.get("models").is_none() {
        doc.insert("models", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    {
        let models = doc
            .get_mut("models")
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| anyhow::anyhow!("models is not a table"))?;
        if let Some(old_name) = update.old_name.filter(|old| *old != update.name) {
            models.remove(old_name);
        }
        let entry = models
            .entry(update.name)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("model entry is not a table"))?;
        entry.insert("model", toml_edit::value(update.model));
        if let Some(provider) = update.provider {
            entry.insert("provider", toml_edit::value(provider));
        } else {
            entry.remove("provider");
        }
        entry.insert(
            "context_budget",
            toml_edit::value(update.context_budget as i64),
        );
        entry.insert("thinking", toml_edit::value(update.thinking));
        if let Some(max_tokens) = update.max_tokens {
            entry.insert("max_tokens", toml_edit::value(max_tokens as i64));
        } else {
            entry.remove("max_tokens");
        }
        entry.insert("enabled", toml_edit::value(update.enabled));
    }

    if let Some(old_name) = update.old_name.filter(|old| *old != update.name)
        && let Some(aliases) = doc.get_mut("alias").and_then(|item| item.as_table_mut())
    {
        for (_, alias) in aliases.iter_mut() {
            if let Some(table) = alias.as_table_mut() {
                if table.get("model").and_then(|item| item.as_str()) == Some(old_name) {
                    table.insert("model", toml_edit::value(update.name));
                }
            } else if let Some(inline) = alias.as_inline_table_mut()
                && inline.get("model").and_then(|value| value.as_str()) == Some(old_name)
            {
                inline.insert("model", toml_edit::Value::from(update.name));
            }
        }
    }
    Ok(())
}

pub fn upsert_model_config(update: ModelConfigUpdate<'_>) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .upsert_model(update)
        .map_err(Into::into)
}

pub fn remove_alias_from_config(alias: &str) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .remove_alias(alias)
        .map_err(Into::into)
}

pub fn update_alias_in_config(
    old_alias: &str,
    new_alias: &str,
    new_model: &str,
) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .update_alias(Some(old_alias), new_alias, new_model)
        .map_err(Into::into)
}

// Provider presets + first-run detection

pub struct ProviderPreset {
    pub name: &'static str,
    pub description: &'static str,
    pub base_url: &'static str,
    pub provider_type: &'static str,
    pub models: &'static [ProviderPresetModel],
    pub key_url: Option<&'static str>,
    pub needs_api_key: bool,
}

pub struct ProviderPresetModel {
    pub id: &'static str,
    pub description: &'static str,
    pub context_budget: u64,
    pub thinking: bool,
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "DeepSeek",
        description: "Recommended — cheap, smart, supports thinking",
        base_url: "https://api.deepseek.com",
        provider_type: "openai-compat",
        models: &[
            ProviderPresetModel {
                id: "deepseek-v4-flash",
                description: "Fast & capable",
                context_budget: 1000000,
                thinking: false,
            },
            ProviderPresetModel {
                id: "deepseek-v4-pro",
                description: "Thinking mode",
                context_budget: 1000000,
                thinking: true,
            },
        ],
        key_url: Some("https://platform.deepseek.com"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "OpenAI",
        description: "GPT-4o / GPT-4o-mini",
        base_url: "https://api.openai.com/v1",
        provider_type: "openai",
        models: &[
            ProviderPresetModel {
                id: "gpt-4o",
                description: "Most capable",
                context_budget: 128000,
                thinking: false,
            },
            ProviderPresetModel {
                id: "gpt-4o-mini",
                description: "Fast & cheap",
                context_budget: 128000,
                thinking: false,
            },
        ],
        key_url: Some("https://platform.openai.com/api-keys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "Anthropic",
        description: "Claude models",
        base_url: "https://api.anthropic.com",
        provider_type: "anthropic",
        models: &[ProviderPresetModel {
            id: "claude-sonnet-4-20250514",
            description: "Claude Sonnet 4",
            context_budget: 200000,
            thinking: true,
        }],
        key_url: Some("https://console.anthropic.com/settings/keys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "ZhipuAI",
        description: "GLM models",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        provider_type: "openai-compat",
        models: &[ProviderPresetModel {
            id: "glm-5.2",
            description: "GLM 5.2",
            context_budget: 1000000,
            thinking: true,
        }],
        key_url: Some("https://open.bigmodel.cn/usercenter/apikeys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "Ollama",
        description: "Local models, no API key needed",
        base_url: "http://localhost:11434/v1",
        provider_type: "openai-compat",
        models: &[],
        key_url: None,
        needs_api_key: false,
    },
    ProviderPreset {
        name: "Codex",
        description: "ChatGPT Plus/Pro OAuth",
        base_url: "https://chatgpt.com/backend-api/codex",
        provider_type: "codex",
        models: &[],
        key_url: None,
        needs_api_key: false,
    },
];

pub fn is_first_run() -> bool {
    let providers = all_provider_entries();
    let models = all_model_entries();
    let config_configured = providers.iter().any(|(_, e)| {
        e.api_key.as_deref().is_some_and(|k| !k.is_empty())
            || e.api_key_env
                .as_deref()
                .is_some_and(|env| std::env::var(env).is_ok_and(|v| !v.trim().is_empty()))
    });
    let env_configured =
        std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok();
    let auth_configured = AuthStore::load()
        .is_ok_and(|store| store.providers.iter().any(|provider| provider.enabled));
    let smart_resolves = {
        let resolved = resolve_alias("smart");
        resolved != "smart" && models.iter().any(|(n, _)| *n == resolved)
    };
    !(config_configured || env_configured || auth_configured) || !smart_resolves
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Tests that mutate MODEL_CONFIG must hold this lock to avoid races
    /// when cargo test runs them in parallel.
    static TEST_CFG_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn unregistered_model_returns_zero_budget() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        *MODEL_CONFIG.write().unwrap() = None;
        assert_eq!(model_info("mystery-model").context_budget, 0);
        assert_eq!(model_info("").context_budget, 0);
    }

    #[test]
    fn threshold_is_eighty_percent() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        assert_eq!(info.compact_threshold_tokens(), 160_000);
    }

    #[test]
    fn compaction_trigger_is_near_budget_top() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        let trigger = info.compaction_trigger_threshold();
        assert!(
            trigger > 150_000 && trigger <= 190_000,
            "trigger should be near the top of the budget, got {trigger}"
        );
    }

    #[test]
    fn compaction_target_is_lower_than_trigger() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        let trigger = info.compaction_trigger_threshold();
        let target = info.compaction_target_after();
        assert!(
            target < trigger,
            "target {target} should be less than trigger {trigger}"
        );
        assert_eq!(target, 50_000, "compaction target should be 25% of context");
    }

    #[test]
    fn alias_resolves_to_real_model() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                ..Default::default()
            },
        );
        cfg.aliases.insert(
            "smart".into(),
            AliasEntry {
                model: "claude-opus-4.7".into(),
            },
        );
        set_model_config(cfg);
        let info = model_info("smart");
        assert_eq!(info.context_budget, 200_000);
        assert_eq!(info.name, "claude-opus-4.7");
    }

    #[test]
    fn custom_model_overrides_budget() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "my-local-model".into(),
            ModelEntry {
                model: "my-local-model".into(),
                context_budget: Some(8192),
                compact_threshold_ratio: Some(0.9),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("my-local-model");
        assert_eq!(info.context_budget, 8192);
        assert_eq!(info.compact_threshold_ratio, 0.9);
    }

    #[test]
    fn compact_threshold_reserves_configured_output_tokens() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "large-output".into(),
            ModelEntry {
                model: "large-output".into(),
                context_budget: Some(1_000_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                max_tokens: Some(400_000),
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("large-output");
        assert_eq!(info.compact_threshold_tokens(), 480_000);
        let trigger = info.compaction_trigger_threshold();
        assert!(
            trigger > 700_000,
            "trigger with capped output reserve should be > 700K, got {trigger}"
        );
    }

    #[test]
    fn alias_chains_through_custom_model() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut cfg = ModelConfig::default();
        cfg.aliases.insert(
            "default".into(),
            AliasEntry {
                model: "my-model".into(),
            },
        );
        cfg.models.insert(
            "my-model".into(),
            ModelEntry {
                model: "my-model".into(),
                context_budget: Some(65_536),
                compact_threshold_ratio: None,
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("default");
        assert_eq!(info.name, "my-model");
        assert_eq!(info.context_budget, 65_536);
    }

    #[test]
    fn discovered_models_survive_set_model_config() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        // Register discovered models first.
        register_discovered(
            "pid-abc",
            "Codex",
            &[crate::provider::DiscoveredModel {
                slug: "codex/gpt-5".to_string(),
                context_budget: Some(128_000),
                thinking: true,
            }],
        );
        assert!(model_entry("Codex:codex/gpt-5").is_some());

        // Simulate config reload from config.toml.
        let mut cfg = ModelConfig::default();
        cfg.aliases.insert(
            "cheap".into(),
            AliasEntry {
                model: "claude-opus-4.7".into(),
            },
        );
        set_model_config(cfg);

        // Discovered models should still be there.
        assert!(
            model_entry("Codex:codex/gpt-5").is_some(),
            "discovered models should survive set_model_config"
        );
        // Alias from config should work.
        assert_eq!(resolve_alias("cheap"), "claude-opus-4.7");
    }

    #[test]
    fn reload_replaces_config_models_and_preserves_discovered_models() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        let mut initial = ProviderConfig::default();
        initial.models.insert(
            "old-config".into(),
            ModelEntry {
                model: "provider/old".into(),
                discovered: false,
                ..Default::default()
            },
        );
        initial.models.insert(
            "dynamic".into(),
            ModelEntry {
                model: "provider/dynamic".into(),
                discovered: true,
                ..Default::default()
            },
        );
        set_provider_config(initial);

        reload_from_text(
            r#"
[models.new-config]
model = "provider/new"
"#,
        )
        .unwrap();

        assert!(model_entry("old-config").is_none());
        assert!(model_entry("new-config").is_some());
        assert!(model_entry("dynamic").is_some());
    }

    #[test]
    fn discovered_models_survive_reload_from_text_alias_crud() {
        let _lock = TEST_CFG_LOCK.lock().unwrap();
        register_discovered(
            "pid-abc",
            "Codex",
            &[crate::provider::DiscoveredModel {
                slug: "codex/gpt-5".to_string(),
                context_budget: Some(128_000),
                thinking: true,
            }],
        );

        let toml = r#"
[alias]
smart = { model = "Codex:codex/gpt-5" }
"#;
        reload_from_text(toml).unwrap();

        assert!(
            model_entry("Codex:codex/gpt-5").is_some(),
            "discovered models should survive alias CRUD"
        );
        assert_eq!(resolve_alias("smart"), "Codex:codex/gpt-5");
    }

    #[test]
    fn model_config_update_replaces_name_and_preserves_other_sections() {
        let mut doc = r#"
# keep this comment
[providers.openai]
kind = "openai"

[alias]
smart = { model = "old-name" }
cheap = { model = "other-name" }

[alias.deep]
model = "old-name"

[models.old-name]
model = "old-id"
provider = "openai"
enabled = true
"#
        .parse::<toml_edit::DocumentMut>()
        .unwrap();

        apply_model_config_update(
            &mut doc,
            ModelConfigUpdate {
                old_name: Some("old-name"),
                name: "new-name",
                model: "new-id",
                provider: Some("openai"),
                context_budget: 128_000,
                thinking: true,
                max_tokens: Some(4096),
                enabled: false,
            },
        )
        .unwrap();

        let out = doc.to_string();
        assert!(out.contains("# keep this comment"));
        assert!(out.contains("[providers.openai]"));
        assert!(out.contains("[alias]"));
        assert!(out.contains("smart = { model = \"new-name\" }"));
        assert!(out.contains("cheap = { model = \"other-name\" }"));
        assert!(out.contains("[alias.deep]"));
        assert!(out.contains("model = \"new-name\""));
        assert!(out.contains("[models.new-name]"));
        assert!(!out.contains("[models.old-name]"));
        assert!(out.contains("model = \"new-id\""));
        assert!(out.contains("context_budget = 128000"));
        assert!(out.contains("thinking = true"));
        assert!(out.contains("max_tokens = 4096"));
        assert!(out.contains("enabled = false"));
    }
}
