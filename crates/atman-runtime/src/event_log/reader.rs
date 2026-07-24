use std::path::Path;

use crate::message::Message;
use crate::session::{ContextSnapshot, SessionOpenError};
use serde_json;

/// Parsed representation of one JSONL event line.
#[derive(Debug, Clone)]
pub struct RawEventRow {
    pub seq: u64,
    pub kind: String,
    pub value: serde_json::Value,
}

impl RawEventRow {
    pub fn field<T: serde::de::DeserializeOwned>(&self, name: &str) -> Option<T> {
        self.value
            .get(name)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }
}

pub fn read_event_rows(path: &Path) -> Result<Vec<RawEventRow>, SessionOpenError> {
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
    let mut rows = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let seq = value["seq"].as_u64().unwrap_or(0);
        let kind = value["type"].as_str().unwrap_or("").to_string();
        rows.push(RawEventRow { seq, kind, value });
    }
    Ok(rows)
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

pub fn read_jsonl_values(path: &Path) -> Result<Vec<serde_json::Value>, SessionOpenError> {
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
    Ok(parse_json_lines(&text))
}

pub fn find_last_seq(path: &Path) -> Result<Option<u64>, SessionOpenError> {
    let values = read_jsonl_values(path)?;
    Ok(values.iter().rev().find_map(|v| v["seq"].as_u64()))
}

pub fn load_last_checkpoint(path: &Path) -> Result<Option<(u64, Vec<Message>)>, SessionOpenError> {
    let values = read_jsonl_values(path)?;
    for v in values.iter().rev() {
        if v["type"].as_str() == Some("checkpoint") {
            let seq = v["seq"].as_u64().unwrap_or(0);
            let messages = v
                .get("messages")
                .and_then(|m| serde_json::from_value::<Vec<Message>>(m.clone()).ok())
                .unwrap_or_default();
            return Ok(Some((seq, messages)));
        }
    }
    Ok(None)
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
