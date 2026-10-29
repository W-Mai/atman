//! Active previews are a removable tail, never a source of committed conversation.

use std::{collections::HashMap, io, time::Instant};

use atman_rt::TurnLease;
use atman_runtime::flow_runtime::live::{DraftPart, LiveOutputSnapshot};
use atman_runtime::{event::AtmanUuid, message::MessageRole, projection::flow::FlowRecord};
use uuid::Uuid;

use super::{FlowDocument, FlowViewModel, OutputMutation};
use crate::app::ToolDraftPreview;
use crate::app::{Disclosure, OutputItem, OutputRevision, ToolCallStatus, ToolCallView};

type Key = (Uuid, u64, usize);
type CallIdentity = (TurnLease<AtmanUuid>, atman_rt::RunId<AtmanUuid>, Uuid);

#[derive(Default)]
pub(super) struct LiveDocument {
    snapshot: Option<LiveOutputSnapshot>,
    start: Option<usize>,
    keys: Vec<Key>,
    detached: HashMap<Key, (OutputItem, OutputRevision)>,
    previews: HashMap<Key, (usize, String, ToolDraftPreview)>,
    waiting_for_publication: Option<CallIdentity>,
    live_fold_start_id: Option<u64>,
    final_fold: Option<(usize, u64)>,
    retired: Vec<CallIdentity>,
}

impl LiveDocument {
    fn is_retired(&self, snapshot: &LiveOutputSnapshot) -> bool {
        !snapshot.calls.is_empty()
            && snapshot.calls.iter().all(|call| {
                self.retired.iter().any(|(lease, run, id)| {
                    call.lease == *lease && call.run == *run && call.id == *id
                })
            })
    }

    fn retire_calls(&mut self, calls: &[atman_runtime::flow_runtime::live::CallDraft]) {
        for call in calls {
            self.retire_call(&call.lease, &call.run, call.id);
        }
    }

    fn retire_call(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        run: &atman_rt::RunId<AtmanUuid>,
        id: Uuid,
    ) {
        if !self
            .retired
            .iter()
            .any(|(retired_lease, retired_run, retired_id)| {
                retired_lease == lease && retired_run == run && *retired_id == id
            })
        {
            self.retired.push((lease.clone(), run.clone(), id));
        }
        const RETIRED_LIMIT: usize = 16;
        if self.retired.len() > RETIRED_LIMIT {
            let excess = self.retired.len() - RETIRED_LIMIT;
            self.retired.drain(..excess);
        }
    }

    pub fn detach(&mut self, document: &mut FlowDocument) {
        if let Some(start) = self.start.take() {
            self.detached = self.keys.drain(..).zip(document.take_tail(start)).collect();
        }
    }

    pub(super) fn discard(&mut self, document: &mut FlowDocument) {
        if let Some(snapshot) = self.snapshot.take() {
            self.retire_calls(&snapshot.calls);
        }
        self.detach(document);
        self.detached.clear();
        self.waiting_for_publication = None;
        self.previews.clear();
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
        if let Some(waiting) = self.waiting_for_publication.clone() {
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
            self.retire_call(&waiting.0, &waiting.1, waiting.2);
            self.detach(document);
            self.detached.clear();
            self.waiting_for_publication = None;
            if let Some(snapshot) = self.snapshot.as_mut() {
                snapshot.calls.retain(|call| {
                    call.lease != waiting.0 || call.run != waiting.1 || call.id != waiting.2
                });
                if snapshot.calls.is_empty() {
                    self.snapshot = None;
                    self.previews.clear();
                }
            }
            return true;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let publication = entries.iter().find_map(|entry| {
            let FlowRecord::Publication(publication) = &entry.record else {
                return None;
            };
            (publication.context_run.is_none()
                && publication.message.role == MessageRole::Assistant)
                .then_some(publication)
        });
        let Some(publication) = publication else {
            return false;
        };
        let Some(call) = snapshot
            .calls
            .iter()
            .find(|call| {
                call.lease == publication.lease
                    && call.run == publication.run
                    && call.publication_pending
            })
            .or_else(|| {
                (snapshot.calls.len() == 1)
                    .then(|| snapshot.calls.first())
                    .flatten()
                    .filter(|call| call.lease == publication.lease && call.run == publication.run)
            })
        else {
            return false;
        };
        let identity = (call.lease.clone(), call.run.clone(), call.id);
        self.retire_call(&identity.0, &identity.1, identity.2);
        self.detach(document);
        self.detached.clear();
        if let Some(snapshot) = self.snapshot.as_mut() {
            snapshot.calls.retain(|candidate| {
                candidate.lease != identity.0
                    || candidate.run != identity.1
                    || candidate.id != identity.2
            });
            if snapshot.calls.is_empty() {
                self.snapshot = None;
                self.previews.clear();
            }
        }
        true
    }
}

impl FlowViewModel {
    pub(crate) fn has_live_output(&self) -> bool {
        self.live.start.is_some()
    }

    pub(super) fn clear_live_work_fold(&mut self) {
        if let Some(start_id) = self.live.live_fold_start_id.take() {
            self.work_folds.retain(|fold| fold.start_id != start_id);
        }
    }

    pub(super) fn clear_live_final_work_fold(&mut self) {
        if let Some((_, start_id)) = self.live.final_fold.take() {
            self.work_folds.retain(|fold| fold.start_id != start_id);
        }
    }

    pub(super) fn rebind_live_final_work_fold(&mut self, start: usize) {
        let Some((boundary, previous)) = self.live.final_fold.as_mut() else {
            return;
        };
        if *boundary != start {
            return;
        }
        // The first turn may contain only preview rows. Its first durable row
        // acquires a new identity while the Work transition remains the same.
        if let Some(revision) = self.document.revisions().get(start)
            && let Some(fold) = self
                .work_folds
                .iter_mut()
                .find(|fold| fold.start_id == *previous)
        {
            fold.start_id = revision.id;
            *previous = revision.id;
        }
    }

    pub(super) fn commit_live_final_work_fold(&mut self, start_id: Option<u64>) {
        if self.live.final_fold.map(|(_, id)| id) == start_id {
            self.live.final_fold = None;
        }
    }

    fn sync_live_work_fold(&mut self, start: usize, end: usize) {
        // A single live row does not form a useful group. Keep the tool row
        // visible directly; create the Work region only when it aggregates
        // multiple rows from the same turn.
        if end.saturating_sub(start) < 2 {
            self.clear_live_work_fold();
            return;
        }
        let Some(start_id) = self
            .document
            .revisions()
            .get(start)
            .map(|revision| revision.id)
        else {
            self.clear_live_work_fold();
            return;
        };
        if self.live.live_fold_start_id != Some(start_id) {
            self.clear_live_work_fold();
        }
        self.live.live_fold_start_id = self.upsert_work_fold(start..end, Some("Working"), true);
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
        if self.live.is_retired(&snapshot) {
            return Ok(false);
        }
        if self.live.waiting_for_publication.is_some() {
            if !snapshot.calls.is_empty() {
                self.live.snapshot = Some(snapshot);
            }
            return Ok(false);
        }
        if let Some(call) = snapshot.calls.first()
            && snapshot.preview_lease.as_ref() != Some(&call.lease)
        {
            if snapshot
                .calls
                .iter()
                .any(|candidate| candidate.lease == call.lease && candidate.publication_pending)
            {
                self.live.waiting_for_publication =
                    Some((call.lease.clone(), call.run.clone(), call.id));
                self.live.snapshot = Some(snapshot);
                return Ok(false);
            }
            // A non-empty snapshot without the producer's lease is a late
            // state after the turn ended. It must not reopen a retired tail.
            if self.live.start.is_some()
                && self.live.snapshot.as_ref().is_some_and(|current| {
                    current
                        .calls
                        .first()
                        .is_some_and(|current| current.lease == call.lease)
                })
            {
                self.clear_live_work_fold();
                self.clear_live_final_work_fold();
                self.live.discard(&mut self.document);
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
                .map(|call| (call.lease.clone(), call.run.clone(), call.id))
        });
        if let Some(waiting) = pending_publication {
            self.live.waiting_for_publication = Some(waiting);
            self.live.snapshot = Some(snapshot);
            return Ok(false);
        }
        if snapshot.calls.is_empty()
            && self.live.start.is_some()
            && self
                .live
                .snapshot
                .as_ref()
                .is_some_and(|current| !current.calls.is_empty())
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
        if self.live.start.is_some() {
            return false;
        }
        if self
            .live
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.calls.is_empty())
        {
            self.render_live()
        } else {
            self.clear_live_final_work_fold();
            false
        }
    }

    fn render_live(&mut self) -> bool {
        // Detaching only the preview tail does not invalidate durable state.
        self.instance.structure_revision = self.document.structure_revision();
        let lease = self
            .live
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.calls.first().map(|call| call.lease.clone()));
        let start = self.live.start.unwrap_or(self.document.len());
        let mut rows = Vec::new();
        let mut work_end = 0;
        let mut has_work_tool = false;
        let mut final_start = None;
        if let Some(snapshot) = &self.live.snapshot {
            for call in snapshot
                .calls
                .iter()
                .filter(|call| Some(&call.lease) == lease.as_ref())
            {
                let mut index = 0;
                while index < call.parts.len() {
                    let key = (call.id, call.attempt, index);
                    let final_preview = matches!(
                        &call.parts[index],
                        DraftPart::Tool { name, .. }
                            if name == atman_runtime::tools::final_answer::FINAL_ANSWER_TOOL
                    );
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
                            has_work_tool = true;
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
                    if final_preview
                        && matches!(&item, OutputItem::AssistantMd { md, .. } if !md.is_empty())
                    {
                        final_start.get_or_insert(start + rows.len());
                    }
                    rows.push((key, item));
                    if !final_preview {
                        work_end = rows.len();
                    }
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
        if let Some(end) = final_start {
            let work_start = lease
                .as_ref()
                .and_then(|lease| self.instance.work_start(lease.epoch))
                .unwrap_or(start);
            let start_id = self
                .document
                .revisions()
                .get(work_start)
                .map(|revision| revision.id);
            if self.live.live_fold_start_id == start_id {
                self.live.live_fold_start_id = None;
            } else {
                self.clear_live_work_fold();
            }
            self.rebind_live_final_work_fold(work_start);
            let first_preview = self.live.final_fold.map(|(_, id)| id) != start_id;
            let fold = self.upsert_work_fold(work_start..end, None, false);
            if first_preview {
                self.collapse_work_fold(fold);
            }
            self.live.final_fold = fold.map(|id| (work_start, id));
        } else if has_work_tool {
            self.clear_live_final_work_fold();
            self.sync_live_work_fold(start, start + work_end);
        } else {
            self.clear_live_final_work_fold();
            self.clear_live_work_fold();
        }
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
