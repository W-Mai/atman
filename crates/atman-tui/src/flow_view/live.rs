//! Active previews are a removable tail, never a source of committed conversation.

use std::{collections::HashMap, io, time::Instant};

use atman_rt::{InstanceActivity, InstanceState, TurnLease};
use atman_runtime::flow_runtime::live::{DraftPart, LiveOutputSnapshot};
use atman_runtime::{event::AtmanUuid, message::MessageRole, projection::flow::FlowRecord};
use uuid::Uuid;

use super::{FlowDocument, FlowViewModel, OutputMutation};
use crate::app::ToolDraftPreview;
use crate::app::{Disclosure, OutputItem, OutputRevision, ToolCallStatus, ToolCallView};

type Key = (Uuid, u64, usize);

#[derive(Default)]
pub(super) struct LiveDocument {
    snapshot: Option<LiveOutputSnapshot>,
    start: Option<usize>,
    keys: Vec<Key>,
    detached: HashMap<Key, (OutputItem, OutputRevision)>,
    previews: HashMap<Key, (usize, String, ToolDraftPreview)>,
    waiting_for_publication: Option<(TurnLease<AtmanUuid>, atman_rt::RunId<AtmanUuid>)>,
}

impl LiveDocument {
    pub fn detach(&mut self, document: &mut FlowDocument) {
        if let Some(start) = self.start.take() {
            self.detached = self.keys.drain(..).zip(document.take_tail(start)).collect();
        }
    }

    fn previous<'a>(&'a self, document: &'a FlowDocument, key: &Key) -> Option<&'a OutputItem> {
        self.start
            .and_then(|start| {
                self.keys
                    .iter()
                    .position(|current| current == key)
                    .map(|offset| &document[start + offset])
            })
            .or_else(|| self.detached.get(key).map(|(item, _)| item))
    }

    pub(super) fn commit_durable_publication(
        &mut self,
        document: &mut FlowDocument,
        entries: &[atman_runtime::projection::flow::FlowTranscriptEntry],
    ) -> bool {
        if let Some(waiting) = &self.waiting_for_publication {
            let committed = entries.iter().any(|entry| {
                let FlowRecord::Publication(publication) = &entry.record else {
                    return false;
                };
                publication.context_run.is_none()
                    && publication.message.role == MessageRole::Assistant
                    && publication.lease == waiting.0
                    && publication.run == waiting.1
            });
            if !committed {
                return false;
            }
            self.detach(document);
            self.detached.clear();
            self.waiting_for_publication = None;
            return true;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let committed = entries.iter().any(|entry| {
            let FlowRecord::Publication(publication) = &entry.record else {
                return false;
            };
            publication.context_run.is_none()
                && publication.message.role == MessageRole::Assistant
                && snapshot
                    .calls
                    .iter()
                    .any(|call| call.lease == publication.lease && call.run == publication.run)
        });
        if !committed {
            return false;
        }
        self.detach(document);
        self.detached.clear();
        self.snapshot = None;
        self.previews.clear();
        true
    }
}

impl FlowViewModel {
    pub(crate) fn has_live_output(&self) -> bool {
        self.live.start.is_some()
    }
    /// Apply a complete active-call snapshot from this view's instance. Older snapshots cannot restore finished previews.
    pub fn sync_live(&mut self, snapshot: LiveOutputSnapshot) -> io::Result<bool> {
        if self.flow() != Some(&snapshot.flow)
            || self.instance.structure_revision != self.document.structure_revision()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "live output does not match the bound document",
            ));
        }
        if self
            .live
            .snapshot
            .as_ref()
            .is_some_and(|current| current.revision >= snapshot.revision)
        {
            return Ok(false);
        }
        if self.live.waiting_for_publication.is_some() {
            if !snapshot.calls.is_empty() {
                self.live.snapshot = Some(snapshot);
            }
            return Ok(false);
        }
        let incoming_keys = snapshot
            .calls
            .iter()
            .flat_map(|call| {
                let mut keys = Vec::new();
                let mut index = 0;
                while index < call.parts.len() {
                    let is_tool = matches!(
                        call.parts[index],
                        DraftPart::Tool { ref name, .. }
                            if name != atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL
                    );
                    let key = (call.id, call.attempt, index);
                    keys.push(key);
                    index += 1;
                    if is_tool {
                        while index < call.parts.len()
                            && matches!(
                                call.parts[index],
                                DraftPart::Tool { ref name, .. }
                                    if name != atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL
                            )
                        {
                            index += 1;
                        }
                    }
                }
                keys.into_iter()
            })
            .collect::<Vec<_>>();
        let pending_publication = self.live.snapshot.as_ref().and_then(|current| {
            let same_lease = current
                .calls
                .first()
                .zip(snapshot.calls.first())
                .is_some_and(|(current, next)| current.lease == next.lease);
            if self.live.start.is_none() || !same_lease || self.live.keys == incoming_keys {
                return None;
            }
            current
                .calls
                .iter()
                .find(|call| call.publication_pending)
                .map(|call| (call.lease.clone(), call.run.clone()))
        });
        if let Some(waiting) = pending_publication {
            self.live.waiting_for_publication = Some(waiting);
            self.live.snapshot = Some(snapshot);
            return Ok(false);
        }
        let active_lease =
            self.snapshot()
                .and_then(|snapshot| match snapshot.status.execution.state() {
                    InstanceState::Open(InstanceActivity::Active(active)) => {
                        Some(active.lease.clone())
                    }
                    _ => None,
                });
        let current_live_lease = self
            .live
            .snapshot
            .as_ref()
            .and_then(|current| current.calls.first().map(|call| &call.lease));
        if !snapshot.calls.is_empty() && self.live.start.is_some() && active_lease.is_none() {
            // A transcript/state refresh can briefly publish no active lease
            // while the producer is still streaming. An empty projection at
            // that boundary is not a completed call; retain the live tail
            // until the execution snapshot catches up.
            return Ok(false);
        }
        if snapshot.calls.is_empty()
            && self.live.start.is_some()
            && active_lease.as_ref() == current_live_lease
        {
            // The producer drops its call immediately after the durable publication is
            // submitted. Keep the preview until the transcript catches up.
            return Ok(false);
        }
        self.live.snapshot = Some(snapshot);
        self.render_live();
        Ok(true)
    }

    pub(super) fn restore_live(&mut self) -> bool {
        let active_lease =
            self.snapshot()
                .and_then(|snapshot| match snapshot.status.execution.state() {
                    InstanceState::Open(InstanceActivity::Active(active)) => {
                        Some(active.lease.clone())
                    }
                    _ => None,
                });
        let current_live_lease = self
            .live
            .snapshot
            .as_ref()
            .and_then(|current| current.calls.first().map(|call| &call.lease));
        if self.live.start.is_some()
            && (active_lease.is_none() || active_lease.as_ref() == current_live_lease)
        {
            return false;
        }
        self.render_live()
    }

    fn render_live(&mut self) -> bool {
        // Detaching only the preview tail does not invalidate durable state.
        self.instance.structure_revision = self.document.structure_revision();
        let lease = self
            .snapshot()
            .and_then(|snapshot| match snapshot.status.execution.state() {
                InstanceState::Open(InstanceActivity::Active(active)) => Some(active.lease.clone()),
                _ => None,
            });
        let start = self.live.start.unwrap_or(self.document.len());
        let mut rows = Vec::new();
        if let Some(snapshot) = &self.live.snapshot {
            for call in snapshot
                .calls
                .iter()
                .filter(|call| Some(&call.lease) == lease.as_ref())
            {
                let mut index = 0;
                while index < call.parts.len() {
                    let key = (call.id, call.attempt, index);
                    let item = match &call.parts[index] {
                        DraftPart::Thinking(text) => OutputItem::Thinking {
                            text: text.clone(),
                            done: index + 1 < call.parts.len(),
                            disclosure: match self.live.previous(&self.document, &key) {
                                Some(OutputItem::Thinking { disclosure, .. }) => *disclosure,
                                _ => Disclosure::Summary,
                            },
                            retried: false,
                        },
                        DraftPart::Text(md) => OutputItem::AssistantMd {
                            md: md.clone(),
                            streaming: true,
                            retried: false,
                        },
                        DraftPart::Tool { name, .. }
                            if name != atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL =>
                        {
                            let row_key = key;
                            let previous_calls = match self.live.previous(&self.document, &row_key)
                            {
                                Some(OutputItem::ToolDispatch { calls }) => calls
                                    .iter()
                                    .map(|old| {
                                        (
                                            old.draft_index,
                                            old.id.clone(),
                                            old.disclosure,
                                            old.started_at,
                                        )
                                    })
                                    .collect::<Vec<_>>(),
                                _ => Vec::new(),
                            };
                            let mut calls = Vec::new();
                            while index < call.parts.len() {
                                let DraftPart::Tool {
                                    index: draft_index,
                                    call_id,
                                    name,
                                    arguments,
                                } = &call.parts[index]
                                else {
                                    break;
                                };
                                if name == atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL {
                                    break;
                                }
                                let part_key = (call.id, call.attempt, index);
                                let id = if call_id.is_empty() {
                                    format!("draft:{}:{draft_index}", call.id)
                                } else {
                                    call_id.clone()
                                };
                                let old = previous_calls
                                    .iter()
                                    .find(|(old_index, old_id, _, _)| {
                                        *old_index == Some(*draft_index) || old_id == &id
                                    })
                                    .map(|(_, _, disclosure, started_at)| {
                                        (*disclosure, *started_at)
                                    });
                                let (offset, previous_name, preview) =
                                    self.live.previews.entry(part_key).or_default();
                                if previous_name != name || *offset > arguments.len() {
                                    *offset = 0;
                                    *previous_name = name.clone();
                                    *preview = ToolDraftPreview::default();
                                }
                                preview.push(name, arguments.get(*offset..).unwrap_or_default());
                                *offset = arguments.len();
                                calls.push(ToolCallView {
                                    id,
                                    tool: name.clone(),
                                    intent: name.clone(),
                                    input: serde_json::Value::Null,
                                    status: ToolCallStatus::Running,
                                    disclosure: old
                                        .map_or(Disclosure::Summary, |(disclosure, _)| disclosure),
                                    detail: None,
                                    draft_index: Some(*draft_index),
                                    draft_preview: preview.clone(),
                                    applied_edit: None,
                                    started_at: old
                                        .map_or_else(Instant::now, |(_, started_at)| started_at),
                                    ended_at: None,
                                });
                                index += 1;
                            }
                            rows.push((row_key, OutputItem::ToolDispatch { calls }));
                            continue;
                        }
                        DraftPart::Tool { name, .. } => {
                            let part_key = key;
                            let DraftPart::Tool { arguments, .. } = &call.parts[index] else {
                                unreachable!()
                            };
                            let (offset, previous_name, preview) =
                                self.live.previews.entry(part_key).or_default();
                            if previous_name != name || *offset > arguments.len() {
                                *offset = 0;
                                *previous_name = name.clone();
                                *preview = ToolDraftPreview::default();
                            }
                            preview.push(name, arguments.get(*offset..).unwrap_or_default());
                            *offset = arguments.len();
                            OutputItem::AssistantMd {
                                md: preview.text().to_owned(),
                                streaming: true,
                                retried: false,
                            }
                        }
                    };
                    rows.push((key, item));
                    index += 1;
                }
            }
        }
        if self.live.start.is_some() && self.live.keys.iter().eq(rows.iter().map(|(key, _)| key)) {
            // Token deltas change content, not document structure or source identity.
            for (offset, (_, item)) in rows.into_iter().enumerate() {
                let index = start + offset;
                let impact = impact(&self.document[index], &item);
                self.document.mutate(index, impact, |current| {
                    *current = item;
                    true
                });
            }
        } else {
            self.live.detach(&mut self.document);
            for (key, item) in rows {
                append(&mut self.document, &mut self.live.detached, key, item);
                self.live.keys.push(key);
            }
        }
        self.live
            .previews
            .retain(|key, _| self.live.keys.contains(key));
        self.live.detached.clear();
        self.live.start = (self.document.len() > start).then_some(start);
        self.instance.structure_revision = self.document.structure_revision();
        self.live.start.is_some()
    }
}

fn append(
    document: &mut FlowDocument,
    previous: &mut HashMap<Key, (OutputItem, OutputRevision)>,
    key: Key,
    item: OutputItem,
) {
    if let Some((old, revision)) = previous.remove(&key) {
        let impact = impact(&old, &item);
        document.restore_item(item, revision);
        document.touch(document.len() - 1, impact);
    } else {
        document.push(item);
    }
}

fn impact(old: &OutputItem, new: &OutputItem) -> OutputMutation {
    if std::mem::discriminant(old) == std::mem::discriminant(new) {
        OutputMutation::SemanticPreserveSource
    } else {
        OutputMutation::Semantic
    }
}
