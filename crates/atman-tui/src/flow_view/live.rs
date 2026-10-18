//! Active previews are a removable tail, never a source of committed conversation.

use std::{collections::HashMap, io, time::Instant};

use atman_rt::{InstanceActivity, InstanceState};
use atman_runtime::flow_runtime::live::{DraftPart, LiveOutputSnapshot};
use uuid::Uuid;

use super::{FlowDocument, FlowViewModel, OutputMutation};
use crate::app::ToolDraftPreview;
use crate::app::{Disclosure, NoteLevel, OutputItem, OutputRevision, ToolCallStatus, ToolCallView};

type Key = (Uuid, u64, usize);

#[derive(Default)]
pub(super) struct LiveDocument {
    snapshot: Option<LiveOutputSnapshot>,
    start: Option<usize>,
    keys: Vec<Key>,
    detached: HashMap<Key, (OutputItem, OutputRevision)>,
    previews: HashMap<Key, (usize, String, ToolDraftPreview)>,
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
}

impl FlowViewModel {
    pub(crate) fn has_live_output(&self) -> bool {
        self.live.start.is_some()
    }
    /// Apply a complete active-call snapshot from this view's instance. Older snapshots cannot restore finished previews.
    pub fn sync_live(&mut self, snapshot: LiveOutputSnapshot) -> io::Result<bool> {
        if self.flow.as_ref() != Some(&snapshot.flow)
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
        self.live.snapshot = Some(snapshot);
        self.render_live();
        Ok(true)
    }

    pub(super) fn restore_live(&mut self) -> bool {
        if self.live.start.is_some() {
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
                let header = (call.id, call.attempt, usize::MAX);
                let text = format!(
                    "Generating · {} · {}",
                    call.model,
                    call.run.0.to_string().chars().take(8).collect::<String>()
                );
                rows.push((
                    header,
                    OutputItem::SystemNote {
                        text,
                        level: NoteLevel::Info,
                    },
                ));
                for (index, part) in call.parts.iter().enumerate() {
                    let key = (call.id, call.attempt, index);
                    let previous = self.live.previous(&self.document, &key);
                    let item = match part {
                        DraftPart::Thinking(text) => OutputItem::Thinking {
                            text: text.clone(),
                            done: index + 1 < call.parts.len(),
                            disclosure: match previous {
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
                        DraftPart::Tool {
                            index,
                            call_id,
                            name,
                            arguments,
                        } => {
                            let old = match previous {
                                Some(OutputItem::ToolDispatch { calls }) => {
                                    calls.first().map(|call| (call.disclosure, call.started_at))
                                }
                                _ => None,
                            };
                            let (offset, previous_name, preview) =
                                self.live.previews.entry(key).or_default();
                            if previous_name != name || *offset > arguments.len() {
                                *offset = 0;
                                *previous_name = name.clone();
                                *preview = ToolDraftPreview::default();
                            }
                            preview.push(name, arguments.get(*offset..).unwrap_or_default());
                            *offset = arguments.len();
                            if name == atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL {
                                OutputItem::AssistantMd {
                                    md: preview.text().to_owned(),
                                    streaming: true,
                                    retried: false,
                                }
                            } else {
                                OutputItem::ToolDispatch {
                                    calls: vec![ToolCallView {
                                        id: if call_id.is_empty() {
                                            format!("draft:{}:{index}", call.id)
                                        } else {
                                            call_id.clone()
                                        },
                                        tool: name.clone(),
                                        intent: name.clone(),
                                        input: serde_json::Value::Null,
                                        status: ToolCallStatus::Running,
                                        disclosure: old
                                            .map_or(Disclosure::Summary, |(disclosure, _)| {
                                                disclosure
                                            }),
                                        detail: None,
                                        draft_index: Some(*index),
                                        draft_preview: preview.clone(),
                                        applied_edit: None,
                                        started_at: old
                                            .map_or_else(Instant::now, |(_, started_at)| {
                                                started_at
                                            }),
                                        ended_at: None,
                                    }],
                                }
                            }
                        }
                    };
                    rows.push((key, item));
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
