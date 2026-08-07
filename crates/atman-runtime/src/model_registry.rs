use std::collections::HashMap;
use std::sync::RwLock;

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
pub struct ModelEntry {
    pub model: String,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
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
pub struct ModelConfig {
    pub models: HashMap<String, ModelEntry>,
    pub aliases: HashMap<String, AliasEntry>,
}

static MODEL_CONFIG: RwLock<Option<ModelConfig>> = RwLock::new(None);

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
pub fn set_model_config(mut cfg: ModelConfig) {
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

/// Register additional model entries without clobbering existing ones.
pub fn register_model_entries(entries: Vec<(String, ModelEntry)>) {
    let mut guard = MODEL_CONFIG.write().unwrap();
    let mut cfg = guard.take().unwrap_or_default();
    for (name, entry) in entries {
        cfg.models.entry(name).or_insert(entry);
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
            return ModelInfo {
                name: resolved.clone(),
                context_budget: entry.context_budget.unwrap_or(0),
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
        let budget_cap = (self.context_budget as f64 * 0.60) as u64;
        let trigger_cap = (trigger as f64 * 0.75) as u64;
        budget_cap.min(trigger_cap)
    }

    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }
}

// ── Alias CRUD (writes config.toml) ──

fn read_config_toml() -> Option<String> {
    let path = crate::storage::config_dir().ok()?.join("config.toml");
    std::fs::read_to_string(&path).ok()
}

fn write_config_toml(text: &str) -> anyhow::Result<()> {
    let dir = crate::storage::config_dir().map_err(|e| anyhow::anyhow!("config dir: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.toml");
    let tmp = dir.join(".config.toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn reload_from_text(text: &str) {
    let Ok(raw) = toml::from_str::<toml::Value>(text) else {
        return;
    };
    let mut guard = MODEL_CONFIG.write().unwrap();
    let mut cfg = guard.take().unwrap_or_default();

    // Update aliases from config.toml — preserve all other state
    // (discovered models, config-defined models).
    cfg.aliases.clear();
    if let Some(aliases) = raw.get("alias").and_then(|a| a.as_table()) {
        for (name, entry) in aliases {
            if let Some(model) = entry.get("model").and_then(|m| m.as_str()) {
                cfg.aliases.insert(
                    name.clone(),
                    AliasEntry {
                        model: model.to_string(),
                    },
                );
            }
        }
    }

    // Update config-defined models — only update existing keys or add
    // new ones; never remove entries that aren't in config.toml.
    if let Some(models) = raw.get("models").and_then(|m| m.as_table()) {
        for (name, entry) in models {
            let provider = entry
                .get("provider")
                .and_then(|v| v.as_str())
                .map(String::from);
            let api_key = entry
                .get("api_key")
                .and_then(|v| v.as_str())
                .map(String::from);
            let base_url = entry
                .get("base_url")
                .and_then(|v| v.as_str())
                .map(String::from);
            let context_budget = entry
                .get("context_budget")
                .and_then(|v| v.as_integer())
                .map(|n| n as u64);
            let thinking = entry.get("thinking").and_then(|v| v.as_bool());
            let max_tokens = entry
                .get("max_tokens")
                .and_then(|v| v.as_integer())
                .map(|n| n as u32);
            let model = entry
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from);
            cfg.models.insert(
                name.clone(),
                ModelEntry {
                    model: model.unwrap_or_default(),
                    provider,
                    api_key,
                    base_url,
                    context_budget,
                    compact_threshold_ratio: None,
                    thinking,
                    max_tokens,
                    enabled: None,
                    discovered: false,
                },
            );
        }
    }

    *guard = Some(cfg);
}

pub fn add_alias_to_config(alias: &str, model: &str) -> anyhow::Result<()> {
    let text = read_config_toml().unwrap_or_default();
    let mut raw: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse config.toml: {e}"))?
    };
    let aliases = raw
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml is not a table"))?
        .entry("alias")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    if let Some(table) = aliases.as_table_mut() {
        let mut entry = toml::value::Table::new();
        entry.insert("model".to_string(), toml::Value::String(model.to_string()));
        table.insert(alias.to_string(), toml::Value::Table(entry));
    }
    let new_text = toml::to_string_pretty(&raw).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    write_config_toml(&new_text)?;
    reload_from_text(&new_text);
    Ok(())
}

pub fn upsert_model_config(
    name: &str,
    provider: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
    context_budget: u64,
    thinking: bool,
) -> anyhow::Result<()> {
    let text = read_config_toml().unwrap_or_default();
    let mut raw: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse config.toml: {e}"))?
    };
    let models = raw
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("config.toml is not a table"))?
        .entry("models")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    if let Some(table) = models.as_table_mut() {
        let mut entry = toml::value::Table::new();
        entry.insert(
            "provider".to_string(),
            toml::Value::String(provider.to_string()),
        );
        if let Some(key) = api_key {
            entry.insert("api_key".to_string(), toml::Value::String(key.to_string()));
        }
        if let Some(url) = base_url {
            entry.insert("base_url".to_string(), toml::Value::String(url.to_string()));
        }
        entry.insert(
            "context_budget".to_string(),
            toml::Value::Integer(context_budget as i64),
        );
        if thinking {
            entry.insert("thinking".to_string(), toml::Value::Boolean(true));
        }
        entry.insert("enabled".to_string(), toml::Value::Boolean(true));
        table.insert(name.to_string(), toml::Value::Table(entry));
    }
    let new_text = toml::to_string_pretty(&raw).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    write_config_toml(&new_text)?;
    reload_from_text(&new_text);
    Ok(())
}

pub fn remove_alias_from_config(alias: &str) -> anyhow::Result<()> {
    let text = read_config_toml().unwrap_or_default();
    let mut raw: toml::Value = toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse: {e}"))?;
    if let Some(table) = raw.get_mut("alias").and_then(|a| a.as_table_mut()) {
        table.remove(alias);
    }
    let new_text = toml::to_string_pretty(&raw).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    write_config_toml(&new_text)?;
    reload_from_text(&new_text);
    Ok(())
}

pub fn update_alias_in_config(
    old_alias: &str,
    new_alias: &str,
    new_model: &str,
) -> anyhow::Result<()> {
    let text = read_config_toml().unwrap_or_default();
    let mut raw: toml::Value = toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse: {e}"))?;
    if let Some(table) = raw.get_mut("alias").and_then(|a| a.as_table_mut()) {
        table.remove(old_alias);
        let mut entry = toml::value::Table::new();
        entry.insert(
            "model".to_string(),
            toml::Value::String(new_model.to_string()),
        );
        table.insert(new_alias.to_string(), toml::Value::Table(entry));
    }
    let new_text = toml::to_string_pretty(&raw).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    write_config_toml(&new_text)?;
    reload_from_text(&new_text);
    Ok(())
}

// ── Provider presets + first-run detection ──

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
    let models = all_model_entries();
    let has_configured = models.iter().any(|(_, e)| {
        e.api_key.as_deref().is_some_and(|k| !k.is_empty())
            && e.provider.is_some()
            && e.context_budget.unwrap_or(0) > 0
    });
    let smart_resolves = {
        let resolved = resolve_alias("smart");
        resolved != "smart" && models.iter().any(|(n, _)| *n == resolved)
    };
    !has_configured || !smart_resolves
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

        // Simulate alias add → reload_from_text
        let toml = r#"
[alias]
smart = { model = "Codex:codex/gpt-5" }
"#;
        reload_from_text(toml);

        assert!(
            model_entry("Codex:codex/gpt-5").is_some(),
            "discovered models should survive alias CRUD"
        );
        assert_eq!(resolve_alias("smart"), "Codex:codex/gpt-5");
    }
}
