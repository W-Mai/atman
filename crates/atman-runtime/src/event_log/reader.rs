use std::path::Path;

use crate::context_plan::{ContextCallPurpose, ContextCallScope};
use crate::session::{ContextSnapshot, ContextUsageBucket, SessionOpenError};
use serde_json;

#[cfg(test)]
thread_local! {
    static PARSE_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_parse_attempts() {
    PARSE_ATTEMPTS.with(|attempts| attempts.set(0));
}

#[cfg(test)]
pub(crate) fn parse_attempts() -> u64 {
    PARSE_ATTEMPTS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_parse_attempt() {
    PARSE_ATTEMPTS.with(|attempts| attempts.set(attempts.get().saturating_add(1)));
}

pub fn read_event_envelopes(
    path: &Path,
) -> Result<Vec<crate::event::EventEnvelope>, SessionOpenError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        #[cfg(test)]
        record_parse_attempt();
        if let Ok(env) = serde_json::from_str::<crate::event::EventEnvelope>(line) {
            out.push(env);
        }
    }
    Ok(out)
}

pub fn parse_json_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| {
            let t = line.trim();
            if t.is_empty() {
                None
            } else {
                #[cfg(test)]
                record_parse_attempt();
                serde_json::from_str::<serde_json::Value>(t).ok()
            }
        })
        .collect()
}

pub fn find_last_seq(path: &Path) -> Result<Option<u64>, SessionOpenError> {
    let envelopes = read_event_envelopes(path)?;
    Ok(envelopes.last().map(|env| env.seq))
}

pub fn replay_context_snapshot_from(path: &Path) -> ContextSnapshot {
    let mut snap = ContextSnapshot::default();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return snap,
    };
    for value in parse_json_lines(&text) {
        if value["type"].as_str() != Some("llm_call") {
            continue;
        }
        let explicit_purpose = value
            .get("context_call_purpose")
            .and_then(|purpose| serde_json::from_value(purpose.clone()).ok());
        let explicit_scope = value
            .get("context_call_identity")
            .and_then(|identity| identity.get("scope"))
            .and_then(|scope| serde_json::from_value(scope.clone()).ok());
        // Older events did not identify helper calls. Preserve their historical
        // run_id convention instead of misclassifying them as root usage.
        if explicit_purpose.is_none() && explicit_scope.is_none() && value["run_id"].is_null() {
            continue;
        }
        let purpose = explicit_purpose.unwrap_or_default();
        let scope = explicit_scope.unwrap_or_else(|| {
            if value["run_id"].is_null() {
                ContextCallScope::Detached
            } else {
                ContextCallScope::Root
            }
        });
        let provider = value["provider"].as_str().unwrap_or("");
        let model = value["model"].as_str().unwrap_or("");
        let usage = &value["usage"];
        let input = usage["input"].as_u64().unwrap_or(0);
        let cached = usage["cached_input"].as_u64().unwrap_or(0);
        let output = usage["output"].as_u64().unwrap_or(0);
        let cache_write = usage["cache_write"].as_u64().unwrap_or(0);
        let total_input = input.saturating_add(cached).saturating_add(cache_write);
        snap.tokens_in = snap.tokens_in.saturating_add(total_input);
        snap.tokens_out = snap.tokens_out.saturating_add(output);
        snap.cache_read = snap.cache_read.saturating_add(cached);
        snap.cache_write = snap.cache_write.saturating_add(cache_write);

        let bucket_idx = snap
            .usage_buckets
            .iter()
            .position(|bucket| {
                bucket.provider == provider
                    && bucket.model == model
                    && bucket.call_purpose == purpose
                    && bucket.call_scope == scope
            })
            .unwrap_or_else(|| {
                snap.usage_buckets.push(ContextUsageBucket {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    call_purpose: purpose,
                    call_scope: scope,
                    ..Default::default()
                });
                snap.usage_buckets.len() - 1
            });
        let bucket = &mut snap.usage_buckets[bucket_idx];
        bucket.calls = bucket.calls.saturating_add(1);
        bucket.tokens_in = bucket.tokens_in.saturating_add(total_input);
        bucket.tokens_out = bucket.tokens_out.saturating_add(output);
        bucket.cache_read = bucket.cache_read.saturating_add(cached);
        bucket.cache_write = bucket.cache_write.saturating_add(cache_write);

        if purpose == ContextCallPurpose::General && scope == ContextCallScope::Root {
            snap.provider = provider.to_string();
            snap.model = model.to_string();
            snap.last_ttft_ms = value["ttft_ms"].as_u64().unwrap_or(0);
            snap.last_tokens_per_sec = value["tokens_per_second"].as_f64().unwrap_or(0.0);
        }
    }
    snap
}
