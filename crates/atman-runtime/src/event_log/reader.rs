use std::io::BufRead;
use std::path::Path;

use crate::context_plan::{ContextCallPurpose, ContextCallScope};
use crate::event::{Event, EventEnvelope};
use crate::session::{ContextSnapshot, ContextUsageBucket, SessionOpenError};

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

#[derive(Debug, Clone)]
pub(crate) struct ReplayRecord {
    pub envelope: EventEnvelope,
    pub persisted_ts: Option<chrono::DateTime<chrono::Utc>>,
}

pub(crate) fn scan_replay_records<R: BufRead>(mut reader: R) -> std::io::Result<Vec<ReplayRecord>> {
    let mut records = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        #[cfg(test)]
        record_parse_attempt();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        let persisted_ts = value
            .get("ts")
            .and_then(serde_json::Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|ts| ts.with_timezone(&chrono::Utc));
        let Ok(envelope) = EventEnvelope::from_json_value(value) else {
            continue;
        };
        records.push(ReplayRecord {
            envelope,
            persisted_ts,
        });
    }
    Ok(records)
}

pub(crate) fn read_replay_records(path: &Path) -> Result<Vec<ReplayRecord>, SessionOpenError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SessionOpenError::Replay {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    scan_replay_records(std::io::BufReader::new(file)).map_err(|source| SessionOpenError::Replay {
        path: path.to_path_buf(),
        source,
    })
}

pub fn read_event_envelopes(path: &Path) -> Result<Vec<EventEnvelope>, SessionOpenError> {
    Ok(read_replay_records(path)?
        .into_iter()
        .map(|record| record.envelope)
        .collect())
}

pub fn parse_json_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|line| {
            let text = line.trim();
            if text.is_empty() {
                None
            } else {
                #[cfg(test)]
                record_parse_attempt();
                serde_json::from_str::<serde_json::Value>(text).ok()
            }
        })
        .collect()
}

pub fn find_last_seq(path: &Path) -> Result<Option<u64>, SessionOpenError> {
    Ok(read_replay_records(path)?
        .last()
        .map(|record| record.envelope.seq))
}

pub(crate) fn context_snapshot_from_records(records: &[ReplayRecord]) -> ContextSnapshot {
    let mut snapshot = ContextSnapshot::default();
    for record in records {
        apply_context_record(&mut snapshot, &record.envelope.event);
    }
    snapshot
}

fn apply_context_record(snapshot: &mut ContextSnapshot, event: &Event) {
    let Event::LlmCall {
        model,
        provider,
        context_call_purpose,
        context_call_identity,
        usage,
        ttft_ms,
        tokens_per_second,
        run_id,
        ..
    } = event
    else {
        return;
    };
    if context_call_purpose.is_none() && context_call_identity.is_none() && run_id.is_none() {
        return;
    }
    let purpose = context_call_purpose.unwrap_or_default();
    let scope = context_call_identity.as_ref().map_or_else(
        || {
            if run_id.is_none() {
                ContextCallScope::Detached
            } else {
                ContextCallScope::Root
            }
        },
        |identity| identity.scope,
    );
    let total_input = usage
        .input
        .saturating_add(usage.cached_input)
        .saturating_add(usage.cache_write);
    snapshot.tokens_in = snapshot.tokens_in.saturating_add(total_input);
    snapshot.tokens_out = snapshot.tokens_out.saturating_add(usage.output);
    snapshot.cache_read = snapshot.cache_read.saturating_add(usage.cached_input);
    snapshot.cache_write = snapshot.cache_write.saturating_add(usage.cache_write);

    let bucket_idx = snapshot
        .usage_buckets
        .iter()
        .position(|bucket| {
            bucket.provider == *provider
                && bucket.model == *model
                && bucket.call_purpose == purpose
                && bucket.call_scope == scope
        })
        .unwrap_or_else(|| {
            snapshot.usage_buckets.push(ContextUsageBucket {
                provider: provider.clone(),
                model: model.clone(),
                call_purpose: purpose,
                call_scope: scope,
                ..Default::default()
            });
            snapshot.usage_buckets.len() - 1
        });
    let bucket = &mut snapshot.usage_buckets[bucket_idx];
    bucket.calls = bucket.calls.saturating_add(1);
    bucket.tokens_in = bucket.tokens_in.saturating_add(total_input);
    bucket.tokens_out = bucket.tokens_out.saturating_add(usage.output);
    bucket.cache_read = bucket.cache_read.saturating_add(usage.cached_input);
    bucket.cache_write = bucket.cache_write.saturating_add(usage.cache_write);

    if purpose == ContextCallPurpose::General && scope == ContextCallScope::Root {
        snapshot.provider.clone_from(provider);
        snapshot.model.clone_from(model);
        snapshot.last_ttft_ms = ttft_ms.unwrap_or(0);
        snapshot.last_tokens_per_sec = tokens_per_second.unwrap_or(0.0);
    }
}

pub fn replay_context_snapshot_from(path: &Path) -> ContextSnapshot {
    read_replay_records(path)
        .map(|records| context_snapshot_from_records(&records))
        .unwrap_or_default()
}

pub fn context_snapshot_from_envelopes(events: &[EventEnvelope]) -> ContextSnapshot {
    let mut snapshot = ContextSnapshot::default();
    for envelope in events {
        apply_context_record(&mut snapshot, &envelope.event);
    }
    snapshot
}
