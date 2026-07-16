use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub context_budget: u64,
    pub compact_threshold_ratio: f64,
    pub thinking_enabled: bool,
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
            };
        }
    }
    let (budget, ratio) = builtin_budget(&resolved);
    ModelInfo {
        name: resolved,
        context_budget: budget,
        compact_threshold_ratio: ratio,
        thinking_enabled: false,
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
        (self.context_budget as f64 * self.compact_threshold_ratio) as u64
    }

    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }
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
