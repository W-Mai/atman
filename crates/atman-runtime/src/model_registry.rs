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

pub fn set_model_config(cfg: ModelConfig) {
    *MODEL_CONFIG.write().unwrap() = Some(cfg);
}

pub fn resolve_alias(name: &str) -> String {
    if let Ok(Some(cfg)) = MODEL_CONFIG.read().as_deref() {
        if let Some(entry) = cfg.aliases.get(name) {
            return entry.model.clone();
        }
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
            let (budget, ratio) = builtin_budget(&resolved);
            return ModelInfo {
                name: resolved.clone(),
                context_budget: entry.context_budget.unwrap_or(budget),
                compact_threshold_ratio: entry.compact_threshold_ratio.unwrap_or(ratio),
                thinking_enabled: entry.thinking.unwrap_or(false),
                max_output_tokens: entry.max_tokens,
            };
        }
    }
    let (budget, ratio) = builtin_budget(&resolved);
    ModelInfo {
        name: resolved,
        context_budget: budget,
        compact_threshold_ratio: ratio,
        thinking_enabled: false,
        max_output_tokens: None,
    }
}

fn builtin_budget(name: &str) -> (u64, f64) {
    let bare = match name.split_once('/') {
        Some((_, rest)) => rest,
        None => name,
    };
    match bare {
        n if n.starts_with("claude-opus") => (200_000, 0.8),
        n if n.starts_with("claude-sonnet") => (200_000, 0.8),
        n if n.starts_with("claude-haiku") => (200_000, 0.8),
        n if n.starts_with("claude-") => (200_000, 0.8),
        n if n.starts_with("gpt-5") => (128_000, 0.8),
        n if n.starts_with("gpt-4o-mini") => (128_000, 0.8),
        n if n.starts_with("gpt-4o") => (128_000, 0.8),
        n if n.starts_with("gpt-4-turbo") => (128_000, 0.8),
        n if n.starts_with("gpt-4") => (32_000, 0.8),
        n if n.starts_with("gpt-3.5") => (16_000, 0.8),
        n if n.starts_with("o1") => (128_000, 0.8),
        n if n.starts_with("o3") => (128_000, 0.8),
        n if n.starts_with("glm-5") => (128_000, 0.8),
        n if n.starts_with("glm-4.5") => (128_000, 0.8),
        n if n.starts_with("glm-4") => (128_000, 0.8),
        n if n.starts_with("glm-") => (128_000, 0.8),
        n if n.starts_with("deepseek-v4") => (1_000_000, 0.8),
        n if n.starts_with("deepseek-v3") => (128_000, 0.8),
        n if n.starts_with("deepseek-r1") => (128_000, 0.8),
        n if n.starts_with("deepseek") => (64_000, 0.8),
        n if n.starts_with("qwen3") => (128_000, 0.8),
        n if n.starts_with("qwen-max") => (128_000, 0.8),
        n if n.starts_with("qwen") => (32_000, 0.8),
        n if n.starts_with("llama") => (8_000, 0.8),
        _ => (32_000, 0.8),
    }
}

impl ModelInfo {
    pub fn compact_threshold_tokens(&self) -> u64 {
        let reserved = self.max_output_tokens.unwrap_or(0) as u64;
        let available = self.context_budget.saturating_sub(reserved);
        (available as f64 * self.compact_threshold_ratio) as u64
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
    std::fs::write(&path, text)?;
    Ok(())
}

fn reload_from_text(text: &str) {
    if let Ok(raw) = toml::from_str::<toml::Value>(text) {
        let mut cfg = ModelConfig::default();
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
                    },
                );
            }
        }
        set_model_config(cfg);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_opus_returns_200k() {
        assert_eq!(model_info("claude-opus-4.7").context_budget, 200_000);
    }

    #[test]
    fn gpt_4o_returns_128k() {
        assert_eq!(model_info("gpt-4o-mini").context_budget, 128_000);
        assert_eq!(model_info("gpt-4o-2024-08-06").context_budget, 128_000);
    }

    #[test]
    fn unknown_model_falls_back_to_32k() {
        assert_eq!(model_info("mystery-model").context_budget, 32_000);
        assert_eq!(model_info("").context_budget, 32_000);
    }

    #[test]
    fn threshold_is_eighty_percent() {
        let info = model_info("claude-opus-4.7");
        assert_eq!(info.compact_threshold_tokens(), 160_000);
    }

    #[test]
    fn alias_resolves_to_real_model() {
        let mut cfg = ModelConfig::default();
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
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "my-local-model".into(),
            ModelEntry {
                model: "my-local-model".into(),
                context_budget: Some(8192),
                compact_threshold_ratio: Some(0.9),
                thinking: None,
                provider: None,
                api_key: None,
                base_url: None,
                max_tokens: None,
            },
        );
        set_model_config(cfg);
        let info = model_info("my-local-model");
        assert_eq!(info.context_budget, 8192);
        assert_eq!(info.compact_threshold_ratio, 0.9);
    }

    #[test]
    fn compact_threshold_reserves_configured_output_tokens() {
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "large-output".into(),
            ModelEntry {
                model: "large-output".into(),
                context_budget: Some(1_000_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                provider: None,
                api_key: None,
                base_url: None,
                max_tokens: Some(400_000),
            },
        );
        set_model_config(cfg);
        let info = model_info("large-output");
        assert_eq!(info.compact_threshold_tokens(), 480_000);
    }

    #[test]
    fn alias_chains_through_custom_model() {
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
                provider: None,
                api_key: None,
                base_url: None,
                max_tokens: None,
            },
        );
        set_model_config(cfg);
        let info = model_info("default");
        assert_eq!(info.name, "my-model");
        assert_eq!(info.context_budget, 65_536);
    }
}
