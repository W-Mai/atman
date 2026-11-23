//! Historical process output retains file data, never a process registry or control handle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::*;
use crate::{
    event::Event,
    flow_store::FlowRef,
    flow_store::trace::ExecutionScope,
    message::MessageRole,
    projection::flow::{ConversationHistoryRecord, FlowRecord},
    retained_file::CopyControl,
    tools::{bash_bg::BgHandle, tool_output::ToolOutputBudget},
};

struct CommittedLog {
    bytes: usize,
    digest: blake3::Hash,
}

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
        self,
        flow: &FlowRef,
        source_directory: &Path,
        records: &[ConversationHistoryRecord],
        max_bytes: usize,
    ) -> io::Result<Self> {
        self.retain_bash_logs_with_control(
            flow,
            source_directory,
            records,
            max_bytes,
            &CopyControl::default(),
        )
    }

    pub(crate) fn retain_bash_logs_with_control(
        self,
        flow: &FlowRef,
        source_directory: &Path,
        records: &[ConversationHistoryRecord],
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<Self> {
        let paths = log_references(flow, source_directory, records, self.directory())?;
        let mut sources = paths
            .into_iter()
            .map(|path| (path, None))
            .collect::<BTreeMap<_, _>>();
        for (path, checkpoint) in committed_logs(flow, source_directory, records)? {
            sources.insert(path, Some(checkpoint));
        }
        self.retain_log_paths_with_control(sources.into_iter(), max_bytes, control)
    }

    /// Model baselines and checkpoints can reference logs without a visible publication.
    #[allow(dead_code)]
    pub(in crate::flow_store::source_capture) fn retain_context_logs(
        self,
        flow: &FlowRef,
        source_directory: &Path,
        messages: &[Message],
        max_bytes: usize,
    ) -> io::Result<Self> {
        self.retain_context_logs_with_control(
            flow,
            source_directory,
            messages,
            max_bytes,
            &CopyControl::default(),
        )
    }

    pub(in crate::flow_store::source_capture) fn retain_context_logs_with_control(
        self,
        flow: &FlowRef,
        source_directory: &Path,
        messages: &[Message],
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<Self> {
        control.check()?;
        let mut calls = HashMap::new();
        let mut paths = BTreeSet::new();
        let outputs = OutputStore::at(self.directory());
        for message in messages {
            for part in &message.parts {
                let key = |id: &str| (message.turn_id.clone(), id.to_owned());
                match part {
                    MessagePart::ToolUse { id, name, .. }
                        if message.role == MessageRole::Assistant =>
                    {
                        if calls.insert(key(id), name.as_str()).is_some() {
                            return Err(invalid("duplicate model-context tool identity"));
                        }
                    }
                    MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        if calls
                            .remove(&key(tool_use_id))
                            .is_some_and(|name| name.starts_with("bash."))
                            && !is_error
                            && let Some(path) = log_path(flow, source_directory, &outputs, content)?
                        {
                            paths.insert(path);
                        }
                    }
                    _ => {}
                }
            }
        }
        self.retain_log_paths_with_control(
            paths.into_iter().map(|path| (path, None)),
            max_bytes,
            control,
        )
    }

    fn retain_log_paths_with_control(
        mut self,
        paths: impl Iterator<Item = (PathBuf, Option<CommittedLog>)>,
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<Self> {
        control.check()?;
        if self.bytes > max_bytes {
            return Err(invalid("snapshot assets exceed the copy byte budget"));
        }
        for (source_path, checkpoint) in paths {
            control.check()?;
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
            let destination = self.directory().join(&filename);
            let identity = if let Some(checkpoint) = checkpoint {
                if checkpoint.bytes > remaining {
                    return Err(invalid("snapshot assets exceed the copy byte budget"));
                }
                crate::retained_file::copy_log_checkpoint(
                    &source_path,
                    &destination,
                    checkpoint.bytes,
                    checkpoint.digest.to_hex().as_str(),
                    control,
                )?
            } else {
                crate::retained_file::copy_append_only_text_with(
                    &source_path,
                    &destination,
                    remaining,
                    control,
                )?
            };
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

fn committed_logs(
    flow: &FlowRef,
    directory: &Path,
    records: &[ConversationHistoryRecord],
) -> io::Result<BTreeMap<PathBuf, CommittedLog>> {
    let key = |lease: &atman_rt::TurnLease<crate::event::AtmanUuid>| {
        (lease.epoch, lease.turn.clone(), lease.run.clone())
    };
    let leases = records
        .iter()
        .filter_map(|entry| match &entry.record {
            FlowRecord::Input(input) => Some(&input.lease),
            FlowRecord::Publication(publication) => Some(&publication.lease),
            FlowRecord::Completion(completion) => Some(&completion.lease),
            _ => None,
        })
        .filter(|lease| &lease.flow == flow)
        .map(key)
        .collect::<HashSet<_>>();
    let session = flow.session_id.to_string();
    let mut checkpoints = BTreeMap::new();
    for entry in records {
        let FlowRecord::Execution(record) = &entry.record else {
            continue;
        };
        let (
            ExecutionScope::Invocation(lease),
            Event::BashLogCheckpoint {
                handle,
                bytes,
                digest,
                ..
            },
        ) = (&record.scope, &record.event)
        else {
            continue;
        };
        if &lease.flow != flow || !leases.contains(&key(lease)) {
            continue;
        }
        let filename = BgHandle::managed_log_name(handle, &session)
            .ok_or_else(|| invalid("process log checkpoint has an invalid handle"))?;
        if !directory.is_absolute() {
            return Err(invalid(
                "process log is outside its original managed directory",
            ));
        }
        let bytes = usize::try_from(*bytes).map_err(invalid)?;
        checkpoints.insert(
            directory.join(filename),
            CommittedLog {
                bytes,
                digest: blake3::Hash::from_bytes(*digest),
            },
        );
    }
    Ok(checkpoints)
}

fn log_references(
    flow: &FlowRef,
    directory: &Path,
    records: &[ConversationHistoryRecord],
    retained_directory: &Path,
) -> io::Result<BTreeSet<PathBuf>> {
    let mut calls = HashMap::new();
    let mut tool_nodes = HashMap::new();
    let mut paths = BTreeSet::new();
    let retained = OutputStore::at(retained_directory);
    for entry in records {
        if &entry.source.flow != flow {
            return Err(invalid("process log record belongs to another source Flow"));
        }
        if let FlowRecord::Execution(record) = &entry.record
            && let Event::ToolNode {
                run_id,
                tool_use_id,
                tool_name,
                ..
            } = &record.event
        {
            let ExecutionScope::Invocation(lease) = &record.scope else {
                return Err(invalid("process log tool node has no invocation"));
            };
            if &lease.flow != flow {
                return Err(invalid("process log tool node belongs to another Flow"));
            }
            if tool_nodes
                .insert(
                    (lease.turn.clone(), run_id.clone(), tool_use_id.clone()),
                    tool_name.as_str(),
                )
                .is_some()
            {
                return Err(invalid("duplicate process log tool node identity"));
            }
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
                    let message_name = calls.remove(&key(tool_use_id));
                    let node_name = tool_nodes.remove(&(
                        publication.lease.turn.clone(),
                        publication.run.clone(),
                        tool_use_id.clone(),
                    ));
                    if message_name
                        .zip(node_name)
                        .is_some_and(|(message, node)| message != node)
                    {
                        return Err(invalid("process log tool sources disagree"));
                    }
                    let Some(name) = message_name.or(node_name) else {
                        continue;
                    };
                    if *is_error || !name.starts_with("bash.") {
                        continue;
                    }
                    if let Some(path) = log_path(flow, directory, &retained, content)? {
                        paths.insert(path);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(paths)
}

fn log_path(
    flow: &FlowRef,
    directory: &Path,
    retained: &OutputStore,
    content: &str,
) -> io::Result<Option<PathBuf>> {
    // Resolve only previously retained output IDs, never a path from arbitrary JSON.
    let full = if let Some((id, bytes)) = crate::tools::tool_output::reference_identity(content) {
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(full.as_deref().unwrap_or(content))
    else {
        return Ok(None);
    };
    let Some(path) = value.get("log_path").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let handle = value
        .get("handle")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("managed process log has no handle"))?;
    let filename = BgHandle::managed_log_name(handle, &flow.session_id.to_string())
        .ok_or_else(|| invalid("process log handle does not belong to the source Session"))?;
    let expected = directory.join(filename);
    if Path::new(path) != expected || !expected.is_absolute() {
        return Err(invalid(
            "process log is outside its original managed directory",
        ));
    }
    Ok(Some(expected))
}

#[cfg(test)]
mod tests;
