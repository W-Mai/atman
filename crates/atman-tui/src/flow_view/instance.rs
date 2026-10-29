//! Incremental document binding for persistent instances.

use std::{
    collections::HashMap,
    io,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Instant,
};

use atman_rt::TurnLease;
use atman_runtime::{
    event::AtmanUuid,
    event_log::journal::CommitReceipt,
    event_log::legacy::{LegacyFlowKey, LegacySource},
    flow_runtime::{FlowView, FlowViewSnapshot},
    flow_store::{
        FlowRef, completion::InvocationResult, trace::ExecutionScope, view::FlowContentSummary,
    },
    message::{Message, MessagePart, MessageRole},
    projection::flow::{
        ArchiveTranscript, FlowRecord, FlowRecordId, FlowTranscript, LegacyFlowView,
    },
    tools::tool_output::OutputStore,
};

use super::{FlowDocument, OutputMutation};
use crate::{
    app::{NoteLevel, OutputItem, ToolCallStatus},
    history::ToolDisplayMeta,
};

type FlowRunId = atman_rt::RunId<AtmanUuid>;

#[derive(PartialEq, Eq)]
struct LegacyBinding {
    session: uuid::Uuid,
    owner: LegacyFlowKey,
    source: LegacySource,
}

#[derive(PartialEq, Eq)]
enum SourceBinding {
    Instance(FlowRef),
    Legacy(LegacyBinding),
}

#[derive(Default)]
struct TurnView {
    work_start: usize,
    panel: Option<usize>,
    graph_revision: Option<u64>,
    completion: Option<FlowRecordId>,
}

#[derive(Default)]
pub(super) struct InstanceDocument {
    archive: bool,
    work_input: bool,
    cursor: Option<CommitReceipt>,
    pub(super) structure_revision: u64,
    entries: usize,
    turns: HashMap<u64, TurnView>,
    calls: HashMap<(u64, FlowRunId, String), (usize, ToolDisplayMeta)>,
    records: HashMap<u64, FlowRecordId>,
}

/// A document and its local disclosure state, independent of whether it is displayed in the main view or a window.
///
/// Native instance updates are derived from a shared `FlowTranscript`. Synchronization appends new records and changes only affected items; it neither consumes input nor controls execution. Existing session renderers use the same document storage and interactions.
#[derive(Default)]
pub struct FlowViewModel {
    pub(super) document: FlowDocument,
    pub(crate) work_folds: Vec<super::fold::WorkFoldState>,
    pub(super) legacy_fold_bindings: HashMap<(atman_rt::TurnId<AtmanUuid>, usize), u64>,
    binding: Option<SourceBinding>,
    pub(super) instance: InstanceDocument,
    pub(super) live: super::live::LiveDocument,
    snapshot: Option<Arc<FlowViewSnapshot>>,
    updated_workflows: Vec<usize>,
    copy: CopyText,
}

#[derive(Default)]
struct CopyText {
    message: Option<String>,
    tool: Option<String>,
}

impl CopyText {
    fn observe(&mut self, message: &Message) {
        if message.role == MessageRole::Assistant {
            self.message = Some(
                message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
            );
        }
        if let Some(content) = message.parts.iter().rev().find_map(|part| match part {
            MessagePart::ToolResult { content, .. } => Some(content),
            _ => None,
        }) {
            self.tool = Some(content.clone());
        }
    }
}

impl Deref for FlowViewModel {
    type Target = FlowDocument;
    fn deref(&self) -> &Self::Target {
        &self.document
    }
}

impl DerefMut for FlowViewModel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.document
    }
}

impl From<Vec<OutputItem>> for FlowViewModel {
    fn from(items: Vec<OutputItem>) -> Self {
        Self {
            document: items.into(),
            ..Self::default()
        }
    }
}

impl FlowViewModel {
    pub(crate) fn copy_message(&self) -> Option<String> {
        if self.binding.is_some() {
            return self.copy.message.clone().filter(|text| !text.is_empty());
        }
        self.iter().rev().find_map(|item| match item {
            OutputItem::AssistantMd { md, .. } => Some(md.clone()),
            _ => None,
        })
    }

    pub(crate) fn copy_tool(&self) -> Option<String> {
        if self.binding.is_some() {
            return self.copy.tool.clone();
        }
        self.iter().rev().find_map(|item| match item {
            OutputItem::Bash { output, .. } if !output.trim().is_empty() => Some(output.clone()),
            _ => None,
        })
    }

    pub fn is_archive(&self) -> bool {
        self.instance.archive
    }

    /// Bind an immutable original-log snapshot without execution state or a persistent cursor. Changed sources and external document replacements require a new view model; repeated reads preserve local disclosure.
    pub fn sync_legacy(
        &mut self,
        source: &LegacyFlowView,
        output_store: Option<&OutputStore>,
    ) -> io::Result<bool> {
        let binding = SourceBinding::Legacy(LegacyBinding {
            session: source.session_id(),
            owner: source.owner().clone(),
            source: source.source().clone(),
        });
        if self
            .binding
            .as_ref()
            .is_some_and(|current| current != &binding)
            || (self.binding.is_none() && !self.document.is_empty())
            || (self.binding.is_some()
                && self.instance.structure_revision != self.document.structure_revision())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "legacy view binding does not match the source history",
            ));
        }
        self.updated_workflows.clear();
        if self.binding.is_some() {
            return Ok(false);
        }
        self.replace_archive(source.transcript(), output_store)?;
        self.restore_work_markers(0, true);
        self.binding = Some(binding);
        self.instance.archive = true;
        self.instance.structure_revision = self.document.structure_revision();
        Ok(true)
    }

    fn replace_archive(
        &mut self,
        archive: &ArchiveTranscript,
        output_store: Option<&OutputStore>,
    ) -> io::Result<()> {
        let output_store = archive
            .outputs
            .has_retained_outputs()
            .then_some(&archive.outputs)
            .or(output_store);
        let mut items = if let Some(store) = output_store {
            crate::history::flatten_transcript_with_output_store(&archive.entries, store)
        } else {
            crate::history::flatten_transcript(&archive.entries)
        };
        for record in &archive.unparsed {
            items.push(OutputItem::SystemNote {
                text: format!(
                    "Retained source record · line {} · {:?}\n{}",
                    record.source.line,
                    record.source.issues,
                    String::from_utf8_lossy(&record.bytes()?).escape_debug()
                ),
                level: NoteLevel::Warn,
            });
        }
        self.document.replace(items);
        self.copy = CopyText::default();
        for entry in &archive.entries {
            if let atman_runtime::projection::message_window::TranscriptEntry::Message {
                message,
                ..
            } = entry
            {
                self.copy.observe(message);
            }
        }
        self.updated_workflows
            .extend(
                self.document
                    .iter()
                    .enumerate()
                    .filter_map(|(index, item)| {
                        matches!(item, OutputItem::WorkflowPanel { .. }).then_some(index)
                    }),
            );
        Ok(())
    }

    pub(crate) fn updated_workflows(&self) -> &[usize] {
        &self.updated_workflows
    }
    pub fn has_active_animation(&self) -> bool {
        self.document.has_active_animation()
            || self
                .work_folds
                .iter()
                .any(|fold| fold.is_animating(Instant::now()))
    }

    pub fn flow(&self) -> Option<&FlowRef> {
        match &self.binding {
            Some(SourceBinding::Instance(flow)) => Some(flow),
            _ => None,
        }
    }

    pub fn cursor(&self) -> Option<&CommitReceipt> {
        self.instance.cursor.as_ref()
    }

    pub(crate) fn has_work_input(&self) -> bool {
        self.instance.work_input
    }

    /// Durable state belonging to the displayed document. Transcript-only updates and external structural edits invalidate an older state snapshot.
    pub fn snapshot(&self) -> Option<&Arc<FlowViewSnapshot>> {
        self.snapshot.as_ref().filter(|snapshot| {
            Some(&snapshot.status.cursor) == self.cursor()
                && self.instance.structure_revision == self.document.structure_revision()
        })
    }

    /// Publish conversation and state together. A state-only update refreshes controls without rebuilding document layout; separate views share the immutable snapshot and retain their own disclosure state.
    pub fn sync_view(
        &mut self,
        view: &FlowView,
        output_store: Option<&OutputStore>,
    ) -> io::Result<bool> {
        let had_live_output = self.has_live_output();
        let previous_cursor = self
            .snapshot()
            .map(|snapshot| snapshot.status.cursor.clone());
        let changed = self.snapshot().is_none_or(|snapshot| {
            snapshot.status.cursor != view.snapshot().status.cursor
                || snapshot.pending_offset != view.snapshot().pending_offset
        });
        let document_changed = self.sync(view.transcript(), output_store)?;
        self.snapshot = Some(view.snapshot().clone());
        let execution_finished = matches!(
            view.snapshot().status.execution.state(),
            atman_rt::InstanceState::Open(
                atman_rt::InstanceActivity::Idle | atman_rt::InstanceActivity::Interrupted { .. },
            ) | atman_rt::InstanceState::Closing { .. }
                | atman_rt::InstanceState::Closed
        );
        if had_live_output
            && execution_finished
            && previous_cursor.as_ref() != Some(&view.snapshot().status.cursor)
        {
            self.clear_live_work_fold();
            self.clear_live_final_work_fold();
            self.live.discard(&mut self.document);
        }
        let live_changed = self.restore_live();
        Ok(changed || document_changed || live_changed)
    }

    pub fn source_record(&self, index: usize) -> Option<FlowRecordId> {
        self.document
            .revisions()
            .get(index)
            .and_then(|revision| self.instance.records.get(&revision.id))
            .copied()
    }

    /// Synchronize one complete committed source boundary. A different flow or a replaced history requires an explicit new view binding.
    pub fn sync(
        &mut self,
        source: &FlowTranscript,
        output_store: Option<&OutputStore>,
    ) -> io::Result<bool> {
        if matches!(self.binding, Some(SourceBinding::Legacy(_)))
            || self.flow().is_some_and(|flow| flow != source.flow())
            || self
                .instance
                .cursor
                .as_ref()
                .is_some_and(|cursor| !source.contains_cursor(cursor))
            || self.instance.entries > source.entries().len()
            || (self.binding.is_none() && !self.document.is_empty())
            || (self.binding.is_some()
                && self.instance.structure_revision != self.document.structure_revision())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "flow view binding does not match the source history",
            ));
        }
        self.updated_workflows.clear();
        if source.cursor() == self.instance.cursor.as_ref() {
            self.binding = Some(SourceBinding::Instance(source.flow().clone()));
            return Ok(false);
        }
        let revision = self.document.revision_clock();
        if matches!(source.content(), Some(FlowContentSummary::History(_))) {
            if self.instance.cursor.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "an archived view cannot acquire additional records",
                ));
            }
            let archive = source.archived_transcript()?;
            self.replace_archive(&archive, output_store)?;
            return Ok(self.finish_sync(source, 0, revision));
        }
        self.live.commit_durable_publication(
            &mut self.document,
            &source.entries()[self.instance.entries..],
        );
        self.live.detach(&mut self.document);
        let document_start = self.document.len();
        for entry in &source.entries()[self.instance.entries..] {
            for message in entry.record.shared_messages() {
                self.copy.observe(message);
            }
            match &entry.record {
                FlowRecord::Continuation { source, messages } => {
                    self.instance.push(&mut self.document, entry.id, OutputItem::SystemNote {
                        text: format!("Continued from session {} · inherited context · original history retained", source.session_id),
                        level: NoteLevel::Info,
                    });
                    for item in
                        crate::history::flatten_messages_with_output_store(messages, output_store)
                    {
                        self.instance.push(&mut self.document, entry.id, item);
                    }
                }
                FlowRecord::Input(input) => {
                    let first = !self.instance.turns.contains_key(&input.lease.epoch);
                    for message in entry.record.shared_messages() {
                        self.instance.message(
                            &mut self.document,
                            &input.lease,
                            &input.lease.run,
                            entry.id,
                            message,
                            output_store,
                        );
                    }
                    if first {
                        self.instance
                            .turns
                            .entry(input.lease.epoch)
                            .or_default()
                            .work_start = self.document.len();
                    }
                }
                FlowRecord::Publication(publication) if publication.context_run.is_none() => {
                    self.instance
                        .ensure_turn(&mut self.document, &publication.lease, entry.id);
                    self.instance.message(
                        &mut self.document,
                        &publication.lease,
                        &publication.run,
                        entry.id,
                        &publication.message,
                        output_store,
                    );
                }
                FlowRecord::Execution(event) => {
                    if let ExecutionScope::Invocation(lease) = &event.scope {
                        self.instance
                            .ensure_turn(&mut self.document, lease, entry.id);
                    }
                }
                FlowRecord::Completion(completion) => {
                    self.instance
                        .ensure_turn(&mut self.document, &completion.lease, entry.id);
                    self.instance
                        .finish_calls(&mut self.document, completion.lease.epoch);
                    if let InvocationResult::Failed(error) = &completion.result {
                        self.instance.push(
                            &mut self.document,
                            entry.id,
                            OutputItem::SystemNote {
                                text: format!("Turn failed: {error}"),
                                level: NoteLevel::Error,
                            },
                        );
                    }
                    self.instance
                        .push(&mut self.document, entry.id, OutputItem::Divider);
                }
                _ => {}
            }
        }
        for turn in source.turns() {
            let Some(view) = self.instance.turns.get_mut(&turn.lease().epoch) else {
                continue;
            };
            let completion = turn.completion().map(|end| end.record);
            if view.graph_revision == Some(turn.workflow().revision())
                && view.completion == completion
            {
                continue;
            }
            if let Some(index) = view.panel {
                self.updated_workflows.push(index);
                self.document
                    .mutate(index, OutputMutation::Semantic, |item| {
                        let OutputItem::WorkflowPanel {
                            graph,
                            ended_at,
                            started_at,
                            cancelled,
                            ..
                        } = item
                        else {
                            return false;
                        };
                        *graph = turn.workflow().clone();
                        // These instants only control animation; graph durations use recorded UTC times.
                        *ended_at = completion.map(|_| *started_at);
                        *cancelled = turn.completion().is_some_and(|end| {
                            matches!(
                                end.outcome,
                                atman_rt::TurnOutcome::Interrupted(
                                    atman_rt::Interruption::Cancelled
                                )
                            )
                        });
                        true
                    });
            }
            view.graph_revision = Some(turn.workflow().revision());
            view.completion = completion;
        }
        Ok(self.finish_sync(source, document_start, revision))
    }

    fn finish_sync(
        &mut self,
        source: &FlowTranscript,
        document_start: usize,
        revision: u64,
    ) -> bool {
        self.restore_work_markers(document_start, self.instance.cursor.is_none());
        self.binding = Some(SourceBinding::Instance(source.flow().clone()));
        self.instance.entries = source.entries().len();
        self.instance.work_input = source.has_work_input();
        self.instance.archive = matches!(source.content(), Some(FlowContentSummary::History(_)));
        self.instance.cursor = source.cursor().cloned();
        self.instance.structure_revision = self.document.structure_revision();
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| Some(&snapshot.status.cursor) != self.cursor())
        {
            self.snapshot = None;
        }
        revision != self.document.revision_clock()
    }

    fn restore_work_markers(&mut self, document_start: usize, restored: bool) {
        let mut fallback_start = document_start;
        let markers = self.document[document_start..]
            .iter()
            .enumerate()
            .filter_map(|(offset, item)| {
                let index = document_start + offset;
                match item {
                    OutputItem::UserTurn { .. } | OutputItem::Divider => {
                        fallback_start = index + 1;
                        None
                    }
                    OutputItem::WorkFoldMarker {
                        start_index,
                        summary,
                    } => {
                        let start = start_index.unwrap_or(fallback_start);
                        // A fold marker precedes its final answer in every document source.
                        fallback_start = index + 2;
                        Some((start, index, summary.clone()))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        for (start, end, summary) in markers {
            self.rebind_live_final_work_fold(start);
            let fold = self.upsert_work_fold(start..end, summary.as_deref(), restored);
            self.commit_live_final_work_fold(fold);
        }
    }
}

impl InstanceDocument {
    pub(super) fn work_start(&self, epoch: u64) -> Option<usize> {
        self.turns.get(&epoch).map(|turn| turn.work_start)
    }

    fn push(&mut self, document: &mut FlowDocument, record: FlowRecordId, item: OutputItem) {
        document.push(item);
        self.records
            .insert(document.revisions().last().unwrap().id, record);
    }

    fn ensure_turn(
        &mut self,
        document: &mut FlowDocument,
        lease: &TurnLease<AtmanUuid>,
        record: FlowRecordId,
    ) {
        let view = self.turns.entry(lease.epoch).or_insert_with(|| TurnView {
            work_start: document.len(),
            ..TurnView::default()
        });
        if view.panel.is_none() {
            view.panel = Some(document.len());
            let turn_index = self.turns.len();
            self.push(
                document,
                record,
                OutputItem::WorkflowPanel {
                    turn_index,
                    graph: atman_runtime::projection::workflow::WorkflowProjection::new(
                        lease.turn.clone(),
                    ),
                    expanded_nodes: Default::default(),
                    panel_expanded: false,
                    started_at: Instant::now(),
                    ended_at: None,
                    cancelled: false,
                },
            );
        }
    }

    fn message(
        &mut self,
        document: &mut FlowDocument,
        lease: &TurnLease<AtmanUuid>,
        run: &FlowRunId,
        record: FlowRecordId,
        message: &Message,
        output_store: Option<&OutputStore>,
    ) {
        if message.role == MessageRole::Tool {
            for part in &message.parts {
                let MessagePart::ToolResult { tool_use_id, .. } = part else {
                    continue;
                };
                let Some((index, meta)) =
                    self.calls
                        .get(&(lease.epoch, run.clone(), tool_use_id.clone()))
                else {
                    continue;
                };
                let Some(item) = document.get(*index) else {
                    continue;
                };
                let mut items = vec![item.clone()];
                let single = Message {
                    role: message.role,
                    origin: message.origin,
                    turn_id: message.turn_id.clone(),
                    parts: vec![part.clone()],
                };
                crate::history::flatten_message_with_output_store(
                    &single,
                    &mut items,
                    &HashMap::from([(tool_use_id.clone(), meta.clone())]),
                    output_store,
                    None,
                );
                let updated = items.remove(0);
                document.mutate(*index, OutputMutation::Semantic, |item| {
                    *item = updated;
                    true
                });
            }
            return;
        }
        let mut items = Vec::new();
        crate::history::flatten_message_with_output_store(
            message,
            &mut items,
            &HashMap::new(),
            output_store,
            None,
        );
        let mut final_answer = false;
        for mut item in items {
            if let OutputItem::WorkFoldMarker { start_index, .. } = &mut item {
                *start_index = self.turns.get(&lease.epoch).map(|turn| turn.work_start);
                final_answer = true;
            }
            if let OutputItem::ToolDispatch { calls } = &item {
                for call in calls {
                    let intent = atman_runtime::message::ToolCallIntent::new(&call.intent);
                    self.calls.insert(
                        (lease.epoch, run.clone(), call.id.clone()),
                        (
                            document.len(),
                            ToolDisplayMeta::from_tool_use(
                                &call.tool,
                                &call.input,
                                intent.as_ref(),
                            ),
                        ),
                    );
                }
            }
            self.push(document, record, item);
        }
        if final_answer && let Some(turn) = self.turns.get_mut(&lease.epoch) {
            turn.work_start = document.len();
        }
    }

    fn finish_calls(&self, document: &mut FlowDocument, epoch: u64) {
        for ((owner, _, id), (index, _)) in &self.calls {
            if *owner != epoch {
                continue;
            }
            document.mutate(*index, OutputMutation::Semantic, |item| {
                let OutputItem::ToolDispatch { calls } = item else {
                    return false;
                };
                let Some(call) = calls
                    .iter_mut()
                    .find(|call| call.id == *id && call.status == ToolCallStatus::Running)
                else {
                    return false;
                };
                call.status = ToolCallStatus::Error;
                call.ended_at = Some(call.started_at);
                true
            });
        }
    }
}
