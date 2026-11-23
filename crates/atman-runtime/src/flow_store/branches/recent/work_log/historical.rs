//! Authority for logs predating writer checkpoints comes from paired durable tool records.

use std::{collections::HashMap, path::Path};

use crate::{
    flow_store::{publication::MessagePublication, source_capture::managed_log_reference},
    tools::tool_output::{
        OutputArtifactIdentity, OutputRangeBudget, OutputStore, reference_identity,
    },
};

use super::*;

const MAX_CALLS: usize = 4096;
const MAX_TOOL_ID_BYTES: usize = 256;
const MAX_RESULT_BYTES: usize = 512 * 1024;
pub(super) const RESULT_READ_BUDGET: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(super) struct HistoricalLogs {
    calls: HashMap<(Call, Option<FlowRunId>), Option<Witness>>,
    nodes: HashMap<Call, DirectCall>,
    entries: VecDeque<Claim>,
    pub(super) truncated: bool,
}

// WorkLogIndex has already checked flow, turn and run against the original admission.
#[derive(Clone, Eq, Hash, PartialEq)]
struct Call {
    epoch: u64,
    producer: FlowRunId,
    id: String,
}

impl Call {
    fn new(lease: &TurnLease<AtmanUuid>, producer: &FlowRunId, id: &str) -> Self {
        Self {
            epoch: lease.epoch,
            producer: producer.clone(),
            id: id.to_owned(),
        }
    }
}

struct Witness {
    commit: u64,
    name: String,
}

#[derive(Default)]
struct DirectCall {
    node: Option<Witness>,
    result: Option<FlowRecordId>,
    ambiguous: bool,
}

enum Reference {
    Inline { handle: String, path: PathBuf },
    Registered(OutputArtifactIdentity),
}

pub(super) struct Claim {
    pub(super) record: FlowRecordId,
    receipt: CommitReceipt,
    pub(super) lease: TurnLease<AtmanUuid>,
    call: Call,
    model: Option<Witness>,
    reference: Reference,
}

impl HistoricalLogs {
    pub(super) fn observe_node(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        commit: u64,
        event: &Event,
    ) {
        let Event::ToolNode {
            run_id,
            tool_use_id,
            tool_name,
            ..
        } = event
        else {
            return;
        };
        if tool_use_id.len() > MAX_TOOL_ID_BYTES || tool_name.len() > MAX_TOOL_ID_BYTES {
            return;
        }
        let call = Call::new(lease, run_id, tool_use_id);
        if let Some(direct) = self.direct(call) {
            direct.ambiguous |= direct.node.is_some();
            direct.node = Some(Witness {
                commit,
                name: tool_name.clone(),
            });
        }
    }

    fn direct(&mut self, call: Call) -> Option<&mut DirectCall> {
        if self.nodes.len() >= MAX_CALLS && !self.nodes.contains_key(&call) {
            self.truncated = true;
            return None;
        }
        Some(self.nodes.entry(call).or_default())
    }

    pub(super) fn observe_publication(
        &mut self,
        receipt: &CommitReceipt,
        fact: usize,
        directory: &Path,
        session: &str,
        publication: &MessagePublication,
    ) {
        let message = &publication.message;
        if message.turn_id != publication.lease.turn {
            return;
        }
        let call = |id| Call::new(&publication.lease, &publication.run, id);
        for (item, part) in message.parts.iter().enumerate() {
            match part {
                MessagePart::ToolUse { id, name, .. }
                    if message.role == MessageRole::Assistant
                        && id.len() <= MAX_TOOL_ID_BYTES
                        && name.len() <= MAX_TOOL_ID_BYTES =>
                {
                    let key = (call(id), publication.context_run.clone());
                    if let Some(existing) = self.calls.get_mut(&key) {
                        *existing = None;
                    } else if self.calls.len() < MAX_CALLS {
                        self.calls.insert(
                            key,
                            Some(Witness {
                                commit: receipt.sequence,
                                name: name.clone(),
                            }),
                        );
                    } else {
                        self.truncated = true;
                    }
                }
                MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if message.role == MessageRole::Tool
                    && tool_use_id.len() <= MAX_TOOL_ID_BYTES =>
                {
                    let call = call(tool_use_id);
                    let record = FlowRecordId {
                        commit: receipt.sequence,
                        fact,
                        item,
                    };
                    if let Some(direct) = self.direct(call.clone()) {
                        direct.ambiguous |= direct.result.is_some();
                        direct.result = Some(record);
                    }
                    let model = match self
                        .calls
                        .remove(&(call.clone(), publication.context_run.clone()))
                    {
                        Some(None) => continue,
                        value => value.flatten(),
                    };
                    if *is_error
                        || model
                            .as_ref()
                            .is_some_and(|value| !value.name.starts_with("bash."))
                    {
                        continue;
                    }
                    let reference = if let Some((_, bytes)) = reference_identity(content) {
                        if bytes > MAX_RESULT_BYTES {
                            self.truncated |= model.is_some();
                            continue;
                        }
                        publication
                            .tool_artifacts
                            .iter()
                            .find(|entry| {
                                entry.part as usize == item && entry.matches_message(message)
                            })
                            .map(|entry| Reference::Registered(entry.output.clone()))
                    } else if content.contains("\"log_path\"") {
                        managed_log_reference(directory, session, content)
                            .ok()
                            .flatten()
                            .map(|(handle, path)| Reference::Inline { handle, path })
                    } else {
                        None
                    };
                    let Some(reference) = reference else {
                        continue;
                    };
                    if self.entries.len() == MAX_CHECKPOINTS {
                        self.entries.pop_front();
                        self.truncated = true;
                    }
                    self.entries.push_back(Claim {
                        record,
                        receipt: receipt.clone(),
                        lease: publication.lease.clone(),
                        call,
                        model,
                        reference,
                    });
                }
                _ => {}
            }
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        (self.calls.capacity()
            * std::mem::size_of::<((Call, Option<FlowRunId>), Option<Witness>)>())
            + (self.nodes.capacity() * std::mem::size_of::<(Call, DirectCall)>())
            + self
                .calls
                .iter()
                .map(|((call, _), source)| {
                    call.id.capacity() + source.as_ref().map_or(0, |call| call.name.capacity())
                })
                .sum::<usize>()
            + self
                .nodes
                .iter()
                .map(|(call, source)| {
                    call.id.capacity() + source.node.as_ref().map_or(0, |node| node.name.capacity())
                })
                .sum::<usize>()
            + self.entries.capacity() * std::mem::size_of::<Claim>()
            + self
                .entries
                .iter()
                .map(|claim| {
                    claim.call.id.capacity()
                        + claim.receipt.digest.capacity()
                        + claim
                            .model
                            .as_ref()
                            .map_or(0, |source| source.name.capacity())
                        + match &claim.reference {
                            Reference::Inline { handle, path } => {
                                handle.capacity() + path.capacity()
                            }
                            Reference::Registered(output) => {
                                output.id.capacity() + output.digest.capacity()
                            }
                        }
                })
                .sum::<usize>()
    }

    pub(super) fn entries(&self) -> impl DoubleEndedIterator<Item = &Claim> {
        self.entries.iter()
    }

    pub(super) fn owns(&self, claim: &Claim, visible: impl Fn(u64) -> bool) -> bool {
        let model = claim.model.as_ref();
        let direct = self.nodes.get(&claim.call);
        let node = direct.and_then(|source| source.node.as_ref());
        if node.is_some()
            && direct.is_some_and(|source| source.ambiguous || source.result != Some(claim.record))
        {
            return false;
        }
        if [model, node]
            .into_iter()
            .flatten()
            .any(|source| !visible(source.commit))
            || model
                .zip(node)
                .is_some_and(|(model, node)| model.name != node.name)
        {
            return false;
        }
        model
            .or(node)
            .is_some_and(|source| source.name.starts_with("bash."))
    }
}

impl Claim {
    pub(super) fn resolve(
        &self,
        directory: &Path,
        session: &str,
        remaining: &mut usize,
        control: &crate::retained_file::CopyControl,
    ) -> io::Result<Option<(String, PathBuf)>> {
        match &self.reference {
            Reference::Inline { handle, path } => Ok(Some((handle.clone(), path.clone()))),
            Reference::Registered(output) => {
                if output.bytes > *remaining {
                    return Err(io::Error::other(
                        "historical log reference budget exhausted",
                    ));
                }
                *remaining -= output.bytes;
                let range = OutputStore::at(directory)
                    .read_artifact_range_with(
                        output,
                        0,
                        output.bytes,
                        OutputRangeBudget {
                            max_verification_bytes: MAX_RESULT_BYTES,
                            max_return_bytes: MAX_RESULT_BYTES,
                        },
                        control,
                    )
                    .map_err(io::Error::other)?;
                if range.has_more {
                    return Ok(None);
                }
                managed_log_reference(directory, session, &range.content)
            }
        }
    }

    pub(super) fn project(&self, handle: String) -> ConversationLogSource {
        ConversationLogSource {
            receipt: self.receipt.clone(),
            record: self.record,
            lease: self.lease.clone(),
            handle,
            bytes: None,
            drained: None,
        }
    }
}
