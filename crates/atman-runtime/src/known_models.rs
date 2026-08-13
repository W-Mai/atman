//! Known model metadata table.
//!
//! Used to fill `context_budget` and `thinking` for models discovered via
//! API (`GET /v1/models` returns IDs only, not metadata).
//!
//! Update this table when new models are released. Entries are matched by
//! exact match first, then longest-prefix match (so `gpt-4` doesn't shadow
//! `gpt-4o`).

/// (model_id_prefix, context_budget, thinking_enabled)
pub static KNOWN_MODELS: &[(&str, u64, bool)] = &[
    // OpenAI GPT-5.6 (current flagship, 2026-08)
    ("gpt-5.6-sol", 1_050_000, true),
    ("gpt-5.6-terra", 1_050_000, false),
    ("gpt-5.6-luna", 1_050_000, false),
    ("gpt-5.6", 1_050_000, true),
    // OpenAI GPT-5.5 (superseded)
    ("gpt-5.5", 1_050_000, true),
    // OpenAI GPT-5.4 (superseded)
    ("gpt-5.4", 1_000_000, false),
    ("gpt-5.4-mini", 1_000_000, false),
    ("gpt-5.4-nano", 1_000_000, false),
    // OpenAI o-series (reasoning)
    ("o4-mini", 200_000, true),
    ("o3", 200_000, true),
    ("o3-pro", 200_000, true),
    // OpenAI legacy
    ("gpt-4o", 128_000, false),
    ("gpt-4o-mini", 128_000, false),
    // Anthropic Claude (current, 2026-08)
    ("claude-opus-5", 1_000_000, true),
    ("claude-sonnet-5", 1_000_000, true),
    ("claude-fable-5", 1_000_000, false),
    ("claude-haiku-4-5", 200_000, false),
    // Anthropic legacy
    ("claude-opus-4-8", 1_000_000, true),
    ("claude-opus-4-7", 1_000_000, true),
    ("claude-sonnet-4-6", 1_000_000, true),
    // DeepSeek V4 (current, 2026-08)
    ("deepseek-v4-pro", 1_000_000, true),
    ("deepseek-v4-flash", 1_000_000, false),
    // DeepSeek legacy (deprecated Jul 24 2026, aliased to v4-flash)
    ("deepseek-chat", 1_000_000, false),
    ("deepseek-reasoner", 1_000_000, true),
    // ZhipuAI GLM
    ("glm-5.2", 1_000_000, true),
    ("glm-4-plus", 128_000, false),
    ("glm-4-flash", 128_000, false),
    // Qwen (Ollama / Alibaba)
    ("qwen2.5-72b-instruct", 131_072, false),
    ("qwen2.5-coder-32b-instruct", 131_072, false),
    // Llama (Ollama / Meta)
    ("llama3.3-70b-instruct", 131_072, false),
    ("llama3.1-8b-instruct", 131_072, false),
];

/// Look up a model ID in [`KNOWN_MODELS`].
///
/// Strips a `-YYYY-MM-DD` date suffix, tries exact match, then falls back to
/// longest-prefix match (so `gpt-4` doesn't shadow `gpt-4o`).
///
/// Returns `(context_budget, thinking_enabled)` on match, or `None` if the
/// model ID is not in the table.
pub fn lookup_known_model(model_id: &str) -> Option<(u64, bool)> {
    let stripped = strip_date_suffix(model_id);
    if let Some((_, budget, thinking)) = KNOWN_MODELS.iter().find(|(k, _, _)| *k == stripped) {
        return Some((*budget, *thinking));
    }
    let mut best: Option<(&str, u64, bool)> = None;
    for (key, budget, thinking) in KNOWN_MODELS {
        if stripped.starts_with(key) && best.is_none_or(|(k, _, _)| key.len() > k.len()) {
            best = Some((key, *budget, *thinking));
        }
    }
    best.map(|(_, b, t)| (b, t))
}

/// Strip a `-YYYY-MM-DD` date suffix from a model ID.
fn strip_date_suffix(s: &str) -> &str {
    if s.len() >= 11 {
        let suffix = &s[s.len() - 11..];
        if let Some(rest) = suffix.strip_prefix('-') {
            let parts: Vec<&str> = rest.split('-').collect();
            if parts.len() == 3 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
                return &s[..s.len() - 11];
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert_eq!(lookup_known_model("gpt-4o"), Some((128_000, false)));
        assert_eq!(lookup_known_model("claude-opus-5"), Some((1_000_000, true)));
    }

    #[test]
    fn date_suffix_stripped() {
        assert_eq!(
            lookup_known_model("gpt-4o-2024-08-06"),
            Some((128_000, false))
        );
    }

    #[test]
    fn longest_prefix_wins() {
        // gpt-4o-2024-08-06 should match gpt-4o (128K), not gpt-4 (would be 8K)
        let result = lookup_known_model("gpt-4o-2024-08-06");
        assert_eq!(result, Some((128_000, false)));
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(lookup_known_model("mystery-model"), None);
    }
}
