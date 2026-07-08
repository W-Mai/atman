#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub context_budget: u64,
    pub compact_threshold_ratio: f64,
}

pub fn model_info(name: &str) -> ModelInfo {
    let bare = match name.split_once('/') {
        Some((_, rest)) => rest,
        None => name,
    };
    let (budget, ratio) = match bare {
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
    };
    ModelInfo {
        name: name.to_string(),
        context_budget: budget,
        compact_threshold_ratio: ratio,
    }
}

impl ModelInfo {
    pub fn compact_threshold_tokens(&self) -> u64 {
        (self.context_budget as f64 * self.compact_threshold_ratio) as u64
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
}
