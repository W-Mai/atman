//! Historical process output retains file data, never a process registry or control handle.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::*;
use crate::{
    flow_store::FlowRef,
    message::MessageRole,
    projection::flow::{ConversationHistoryRecord, FlowRecord},
    tools::tool_output::ToolOutputBudget,
};

/// The original display reference and independently owned observed log prefix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetainedBashLog {
    pub source_path: PathBuf,
    pub filename: String,
    pub bytes: usize,
    pub digest: String,
}

impl RetainedSourceAssets {
    /// Blocking retention of logs from paired native publications. Failure discards this complete uncommitted asset owner.
    pub fn retain_bash_logs(
        mut self,
        flow: &FlowRef,
        source_directory: &Path,
        records: &[ConversationHistoryRecord],
        max_bytes: usize,
    ) -> io::Result<Self> {
        if self.bytes > max_bytes {
            return Err(invalid("snapshot assets exceed the copy byte budget"));
        }
        let paths = log_references(flow, source_directory, records, self.directory())?;
        for source_path in paths {
            if self
                .logs
                .iter()
                .any(|saved| saved.source_path == source_path)
            {
                continue;
            }
            let remaining = max_bytes
                .checked_sub(self.bytes)
                .ok_or_else(|| invalid("snapshot assets exceed the copy byte budget"))?;
            let filename = source_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("invalid managed process log name"))?
                .to_owned();
            let identity = crate::retained_file::copy_append_only_text(
                &source_path,
                &self.directory().join(&filename),
                remaining,
            )?;
            self.bytes += identity.bytes;
            self.logs.push(RetainedBashLog {
                source_path,
                filename,
                bytes: identity.bytes,
                digest: identity.digest,
            });
        }
        Ok(self)
    }

    pub fn logs(&self) -> &[RetainedBashLog] {
        &self.logs
    }
}

fn log_references(
    flow: &FlowRef,
    directory: &Path,
    records: &[ConversationHistoryRecord],
    retained_directory: &Path,
) -> io::Result<BTreeSet<PathBuf>> {
    let mut calls = HashMap::new();
    let mut paths = BTreeSet::new();
    let retained = OutputStore::at(retained_directory);
    for entry in records {
        if &entry.source.flow != flow {
            return Err(invalid("process log record belongs to another source Flow"));
        }
        let FlowRecord::Publication(publication) = &entry.record else {
            continue;
        };
        if &publication.lease.flow != flow || publication.lease.turn != publication.message.turn_id
        {
            return Err(invalid("process log publication has a different owner"));
        }
        for part in &publication.message.parts {
            let key = |id: &str| {
                (
                    publication.lease.turn.clone(),
                    publication.context_run.clone(),
                    id.to_owned(),
                )
            };
            match part {
                MessagePart::ToolUse { id, name, .. }
                    if publication.message.role == MessageRole::Assistant =>
                {
                    if calls.insert(key(id), name.as_str()).is_some() {
                        return Err(invalid("duplicate process log tool identity"));
                    }
                }
                MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let Some(name) = calls.remove(&key(tool_use_id)) else {
                        continue;
                    };
                    if *is_error || !name.starts_with("bash.") {
                        continue;
                    }
                    // Resolve only previously retained output IDs, never a path from arbitrary JSON.
                    let full = if let Some((id, bytes)) =
                        crate::tools::tool_output::reference_identity(content)
                    {
                        Some(
                            retained
                                .read_bytes(
                                    &id,
                                    0,
                                    bytes,
                                    ToolOutputBudget {
                                        max_bytes: bytes,
                                        max_line_bytes: bytes,
                                        max_lines: usize::MAX,
                                    },
                                )
                                .map_err(invalid)?
                                .content,
                        )
                    } else {
                        None
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(
                        full.as_deref().unwrap_or(content),
                    ) else {
                        continue;
                    };
                    let Some(path) = value.get("log_path").and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    let handle = value
                        .get("handle")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| invalid("managed process log has no handle"))?;
                    let prefix = format!("bg_{}_", flow.session_id);
                    let suffix = handle
                        .strip_prefix(&prefix)
                        .ok_or_else(|| invalid("process log belongs to another Session"))?;
                    let number = suffix.parse::<u64>().map_err(invalid)?;
                    if suffix != number.to_string() {
                        return Err(invalid("process log handle is not canonical"));
                    }
                    let expected = directory.join(format!("bg_{handle}.log"));
                    if Path::new(path) != expected || !expected.is_absolute() {
                        return Err(invalid(
                            "process log is outside its original managed directory",
                        ));
                    }
                    paths.insert(expected);
                }
                _ => {}
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests;
