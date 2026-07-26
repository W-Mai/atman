use std::path::Path;

use crate::session::{ContextSnapshot, SessionOpenError};
use serde_json;

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
        // Only count main-agent LLM calls — subagent calls have run_id: null.
        if value["run_id"].is_null() {
            continue;
        }
        if let Some(model) = value["model"].as_str() {
            snap.model = model.to_string();
        }
        let usage = &value["usage"];
        let input = usage["input"].as_u64().unwrap_or(0);
        let cached = usage["cached_input"].as_u64().unwrap_or(0);
        let output = usage["output"].as_u64().unwrap_or(0);
        snap.tokens_in = snap.tokens_in.saturating_add(input).saturating_add(cached);
        snap.tokens_out = snap.tokens_out.saturating_add(output);
    }
    snap
}
