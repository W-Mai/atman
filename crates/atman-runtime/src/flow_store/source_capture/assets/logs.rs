//! Historical process output retains file data, never a process registry or control handle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::*;
use crate::{
    event::Event,
    flow_store::trace::ExecutionScope,
    flow_store::{FlowRef, source_capture::ForkLogAsset},
    message::MessageRole,
    projection::flow::{ConversationHistoryRecord, FlowRecord},
    retained_file::{CopyControl, LogExtent, log_copy_bytes},
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
        let outputs = OutputStore::at(self.directory());
        let sources = conversation_logs(flow, source_directory, records, &outputs)?;
        self.retain_log_paths_with_control(sources.into_iter(), max_bytes, control)
    }

    /// Model baselines and checkpoints can reference logs without a visible publication.
    #[cfg(test)]
    pub(in crate::flow_store::source_capture) fn retain_context_logs(
        self,
        flow: &FlowRef,
        source_directory: &Path,
        messages: &[Message],
        max_bytes: usize,
    ) -> io::Result<Self> {
        let outputs = OutputStore::at(self.directory());
        let paths = context_logs(flow, source_directory, messages, &outputs, &[])?;
        self.retain_log_paths_with_control(
            paths.into_iter().map(|path| (path, None)),
            max_bytes,
            &CopyControl::default(),
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
            let digest;
            let extent = if let Some(checkpoint) = checkpoint {
                if checkpoint.bytes > remaining {
                    return Err(invalid("snapshot assets exceed the copy byte budget"));
                }
                digest = checkpoint.digest.to_hex();
                LogExtent::Checkpoint {
                    bytes: checkpoint.bytes,
                    digest: digest.as_str(),
                }
            } else {
                LogExtent::Observed {
                    max_bytes: remaining,
                }
            };
            let identity =
                crate::retained_file::copy_log(&source_path, &destination, extent, control)?;
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

/// The immediate source owns its native log files and all previously published copies.
pub(in crate::flow_store::source_capture) struct ForkLogSelection<'a> {
    pub flow: &'a FlowRef,
    pub directory: &'a Path,
    pub native_records: &'a [ConversationHistoryRecord],
    pub inherited: &'a [ForkLogAsset],
}

impl ForkLogSelection<'_> {
    fn resolve<'a>(
        &self,
        contexts: impl IntoIterator<Item = &'a [Message]>,
        outputs: &OutputStore,
        control: &CopyControl,
    ) -> io::Result<Vec<(PathBuf, Option<CommittedLog>)>> {
        // Existing manifests hash the native references first, then each context
        // window's newly discovered references in encounter order.
        let mut logs: Vec<_> =
            conversation_logs(self.flow, self.directory, self.native_records, outputs)?
                .into_iter()
                .collect();
        let mut seen: HashSet<_> = logs.iter().map(|(path, _)| path.clone()).collect();
        for log in self.inherited {
            control.check()?;
            let path = self.directory.join(format!("bg_{}.log", log.handle));
            if !seen.insert(path.clone()) {
                return Err(invalid("inherited process log collides with a native log"));
            }
            logs.push((
                path,
                Some(CommittedLog {
                    bytes: log.bytes,
                    digest: blake3::Hash::from_hex(&log.digest).map_err(invalid)?,
                }),
            ));
        }
        for context in contexts {
            control.check()?;
            for path in context_logs(self.flow, self.directory, context, outputs, self.inherited)? {
                if seen.insert(path.clone()) {
                    logs.push((path, None));
                }
            }
        }
        Ok(logs)
    }

    pub fn measure<'a>(
        &self,
        contexts: impl IntoIterator<Item = &'a [Message]>,
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<usize> {
        let outputs = OutputStore::at(self.directory);
        let mut remaining = max_bytes;
        for (path, checkpoint) in self.resolve(contexts, &outputs, control)? {
            let bytes = log_copy_bytes(
                &path,
                checkpoint.map(|checkpoint| checkpoint.bytes),
                remaining,
                control,
            )?;
            remaining -= bytes;
        }
        Ok(max_bytes - remaining)
    }

    pub fn retain<'a>(
        &self,
        assets: RetainedSourceAssets,
        contexts: impl IntoIterator<Item = &'a [Message]>,
        max_bytes: usize,
        control: &CopyControl,
    ) -> io::Result<RetainedSourceAssets> {
        let outputs = OutputStore::at(assets.directory());
        let logs = self.resolve(contexts, &outputs, control)?;
        assets.retain_log_paths_with_control(logs.into_iter(), max_bytes, control)
    }
}

fn conversation_logs(
    flow: &FlowRef,
    directory: &Path,
    records: &[ConversationHistoryRecord],
    outputs: &OutputStore,
) -> io::Result<BTreeMap<PathBuf, Option<CommittedLog>>> {
    let mut sources = log_references(flow, directory, records, outputs)?
        .into_iter()
        .map(|path| (path, None))
        .collect::<BTreeMap<_, _>>();
    for (path, checkpoint) in committed_logs(flow, directory, records)? {
        sources.insert(path, Some(checkpoint));
    }
    Ok(sources)
}

fn context_logs(
    flow: &FlowRef,
    directory: &Path,
    messages: &[Message],
    outputs: &OutputStore,
    inherited: &[ForkLogAsset],
) -> io::Result<BTreeSet<PathBuf>> {
    let mut calls = HashMap::new();
    let mut paths = BTreeSet::new();
    for message in messages {
        for part in &message.parts {
            let key = |id: &str| (message.turn_id.clone(), id.to_owned());
            match part {
                MessagePart::ToolUse { id, name, .. } if message.role == MessageRole::Assistant => {
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
                        && let Some(path) = log_path(flow, directory, outputs, content, inherited)?
                    {
                        paths.insert(path);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(paths)
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
    outputs: &OutputStore,
) -> io::Result<BTreeSet<PathBuf>> {
    let mut calls = HashMap::new();
    let mut tool_nodes = HashMap::new();
    let mut paths = BTreeSet::new();
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
                    if let Some(path) = log_path(flow, directory, outputs, content, &[])? {
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
    inherited: &[ForkLogAsset],
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
    let content = full.as_deref().unwrap_or(content);
    if !inherited.is_empty()
        && let Ok(reference) = serde_json::from_str::<LogReference>(content)
        && let Some(handle) = reference.handle
        && inherited.iter().any(|log| log.handle == handle)
    {
        // An older registered output may still carry its original path. The verified
        // publication authorizes only the immediate source's copy of this handle.
        return Ok(Some(directory.join(format!("bg_{handle}.log"))));
    }
    Ok(
        managed_log_reference(directory, &flow.session_id.to_string(), content)?
            .map(|(_, path)| path),
    )
}

#[derive(Deserialize)]
struct LogReference {
    log_path: String,
    handle: Option<String>,
}

/// Validate the two locator fields without retaining other parts of the tool result.
pub(in crate::flow_store) fn managed_log_reference(
    directory: &Path,
    session: &str,
    content: &str,
) -> io::Result<Option<(String, PathBuf)>> {
    let Ok(value) = serde_json::from_str::<LogReference>(content) else {
        return Ok(None);
    };
    let handle = value
        .handle
        .ok_or_else(|| invalid("managed process log has no handle"))?;
    let filename = BgHandle::managed_log_name(&handle, session)
        .ok_or_else(|| invalid("process log handle does not belong to the source Session"))?;
    let expected = directory.join(filename);
    if Path::new(&value.log_path) != expected || !expected.is_absolute() {
        return Err(invalid(
            "process log is outside its original managed directory",
        ));
    }
    Ok(Some((handle, expected)))
}

#[cfg(test)]
mod tests;
