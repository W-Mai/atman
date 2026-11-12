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
    event_log::legacy::{LegacyFlowKey, LegacySource},
    flow_runtime::{FlowView, FlowViewSnapshot},
    flow_store::branches::HistoryHead,
    flow_store::position::FlowPosition,
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
type TurnKey = (FlowRef, u64);

fn turn_key(lease: &TurnLease<AtmanUuid>) -> TurnKey {
    (lease.flow.clone(), lease.epoch)
}

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

#[derive(Clone, Copy)]
struct ToolSources<'a> {
    output: Option<&'a OutputStore>,
    materialized: Option<&'a crate::history::MaterializedTools>,
}

#[derive(Default)]
pub(super) struct InstanceDocument {
    archive: bool,
    work_input: bool,
    cursor: Option<FlowPosition>,
    pub(super) head: Option<HistoryHead>,
    pub(super) structure_revision: u64,
    entries: usize,
    turns: HashMap<TurnKey, TurnView>,
    calls: HashMap<(TurnKey, FlowRunId, String), (usize, ToolDisplayMeta)>,
    pub(super) records: HashMap<u64, FlowRecordId>,
    user_turns:
        HashMap<FlowRecordId, atman_runtime::flow_store::source_capture::ConversationCutoff>,
    handoff: Option<super::live::PublicationHandoff>,
}

/// A document and its local disclosure state, independent of whether it is displayed in the main view or a window.
///
/// Native instance updates are derived from a shared `FlowTranscript`. Synchronization appends new records and changes only affected items; it neither consumes input nor controls execution. Existing session renderers use the same document storage and interactions.
#[derive(Default)]
pub struct FlowViewModel {
    pub(super) document: FlowDocument,
    pub(crate) work_folds: Vec<super::fold::WorkFoldState>,
    pub(super) work_disclosures: HashMap<u64, bool>,
    pub(super) history_presentations: HashMap<
        atman_runtime::flow_store::source_capture::ConversationBranchId,
        super::presentation::BranchPresentation,
    >,
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
    pub(crate) fn install_history(
        &mut self,
        previous: &HistoryHead,
        mut document: Self,
    ) -> io::Result<bool> {
        if self.instance.head.as_ref() != Some(previous)
            || self.flow().is_none()
            || self.flow() != document.flow()
            || document.snapshot().is_none()
            || document
                .instance
                .head
                .as_ref()
                .is_none_or(|head| head == previous)
        {
            crate::render_work::discard(document);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared history does not replace this document source",
            ));
        }
        self.retain_branch_presentation(&mut document);
        crate::render_work::discard(std::mem::replace(self, document));
        Ok(true)
    }

    pub(crate) fn has_copy_message(&self) -> bool {
        if self.binding.is_some() {
            return self
                .copy
                .message
                .as_ref()
                .is_some_and(|text| !text.is_empty());
        }
        self.iter()
            .any(|item| matches!(item, OutputItem::AssistantMd { md, .. } if !md.is_empty()))
    }

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
        self.document.has_active_animation() || self.work_fold_animates()
    }

    pub fn flow(&self) -> Option<&FlowRef> {
        match &self.binding {
            Some(SourceBinding::Instance(flow)) => Some(flow),
            _ => None,
        }
    }

    pub fn cursor(&self) -> Option<&FlowPosition> {
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
        self.sync_view_with_sources(view, output_store, None)
    }

    pub(crate) fn sync_materialized(
        &mut self,
        view: &FlowView,
        sources: &crate::history::MaterializedTools,
    ) -> io::Result<bool> {
        if !sources.covers(view, self.instance.entries) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prepared tool artifacts do not cover this document boundary",
            ));
        }
        self.sync_view_with_sources(view, Some(&sources.outputs), Some(sources))
    }

    fn sync_view_with_sources(
        &mut self,
        view: &FlowView,
        output_store: Option<&OutputStore>,
        materialized: Option<&crate::history::MaterializedTools>,
    ) -> io::Result<bool> {
        let had_live_output = self.has_live_output();
        let previous_cursor = self
            .snapshot()
            .map(|snapshot| snapshot.status.cursor.clone());
        let changed = self.snapshot().is_none_or(|snapshot| {
            snapshot.status.cursor != view.snapshot().status.cursor
                || snapshot.pending_offset != view.snapshot().pending_offset
        });
        let document_changed =
            self.sync_with_sources(view.transcript(), output_store, materialized)?;
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

    /// Capture the installed native branch and cursor without deriving a different history head.
    pub fn source_selection(
        &self,
    ) -> Option<atman_runtime::flow_store::source_capture::SourceSelection> {
        if self.is_archive() {
            return None;
        }
        Some(atman_runtime::flow_store::source_capture::SourceSelection {
            flow: self.flow()?.clone(),
            branch: self.instance.head.as_ref()?.branch,
            cursor: self.cursor()?.require_durable().ok()?.clone(),
        })
    }

    /// Authored turn action bound to the installed document's original input and durable receipt.
    pub fn user_turn_boundary(
        &self,
        index: usize,
    ) -> Option<&atman_runtime::flow_store::source_capture::ConversationCutoff> {
        if !matches!(
            self.get(index)?,
            OutputItem::UserTurn { .. } | OutputItem::Interjection { .. }
        ) {
            return None;
        }
        self.instance.user_turns.get(&self.source_record(index)?)
    }

    /// Synchronize one complete committed source boundary. A different flow or a replaced history requires an explicit new view binding.
    pub fn sync(
        &mut self,
        source: &FlowTranscript,
        output_store: Option<&OutputStore>,
    ) -> io::Result<bool> {
        self.sync_with_sources(source, output_store, None)
    }

    fn sync_with_sources(
        &mut self,
        source: &FlowTranscript,
        output_store: Option<&OutputStore>,
        materialized: Option<&crate::history::MaterializedTools>,
    ) -> io::Result<bool> {
        let tool_sources = ToolSources {
            output: output_store,
            materialized,
        };
        if matches!(self.binding, Some(SourceBinding::Legacy(_)))
            || self.flow().is_some_and(|flow| flow != source.flow())
            || (self.instance.cursor.is_some()
                && self.instance.head.as_ref() != source.history_head())
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
        let handoff = self.live.commit_durable_publication(
            &mut self.document,
            &source.entries()[self.instance.entries..],
        );
        crate::render_work::discard(std::mem::replace(&mut self.instance.handoff, handoff));
        self.live.detach(&mut self.document);
        let document_start = self.document.len();
        for entry in &source.entries()[self.instance.entries..] {
            for message in entry.record.shared_messages() {
                self.copy.observe(message);
            }
            match &entry.record {
                FlowRecord::Continuation { source, messages } => {
                    self.instance.push(
                        &mut self.document,
                        entry.id,
                        OutputItem::SystemNote {
                            text: format!(
                                "Continued from session {} · inherited context",
                                source.session_id
                            ),
                            level: NoteLevel::Info,
                        },
                    );
                    for item in crate::history::flatten_messages_with_sources(
                        messages,
                        output_store,
                        materialized,
                    ) {
                        self.instance.push(&mut self.document, entry.id, item);
                    }
                }
                FlowRecord::Input(input) => {
                    if let Some(boundary) = source.user_turn_boundary(entry.id) {
                        self.instance.user_turns.insert(entry.id, boundary);
                    }
                    let first = !self.instance.turns.contains_key(&turn_key(&input.lease));
                    for message in entry.record.shared_messages() {
                        self.instance.message(
                            &mut self.document,
                            &input.lease,
                            &input.lease.run,
                            entry.id,
                            message,
                            tool_sources,
                        )?;
                    }
                    if first {
                        self.instance
                            .turns
                            .entry(turn_key(&input.lease))
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
                        tool_sources,
                    )?;
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
                        .finish_calls(&mut self.document, &turn_key(&completion.lease));
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
            let Some(view) = self.instance.turns.get_mut(&turn_key(turn.lease())) else {
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
        crate::render_work::discard(self.instance.handoff.take());
        self.restore_work_markers(document_start, self.instance.cursor.is_none());
        self.binding = Some(SourceBinding::Instance(source.flow().clone()));
        self.instance.entries = source.entries().len();
        self.instance.work_input = source.has_work_input();
        self.instance.archive = matches!(source.content(), Some(FlowContentSummary::History(_)));
        self.instance.cursor = source.cursor().cloned();
        self.instance.head = source.history_head().cloned();
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
        let markers = self
            .document
            .iter()
            .skip(document_start)
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
    pub(super) fn work_start(&self, lease: &TurnLease<AtmanUuid>) -> Option<usize> {
        self.turns.get(&turn_key(lease)).map(|turn| turn.work_start)
    }

    fn push(&mut self, document: &mut FlowDocument, record: FlowRecordId, mut item: OutputItem) {
        if let Some((revision, mutation)) = self
            .handoff
            .as_mut()
            .filter(|handoff| handoff.record == record)
            .and_then(|handoff| handoff.take_revision(&mut item))
        {
            // The durable payload acquires the existing row, never draft authority.
            document.restore_item(item, revision);
            document.touch(document.len() - 1, mutation);
        } else {
            document.push(item);
        }
        self.records
            .insert(document.revisions().last().unwrap().id, record);
    }

    fn ensure_turn(
        &mut self,
        document: &mut FlowDocument,
        lease: &TurnLease<AtmanUuid>,
        record: FlowRecordId,
    ) {
        let view = self
            .turns
            .entry(turn_key(lease))
            .or_insert_with(|| TurnView {
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
                    presentation: Default::default(),
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
        sources: ToolSources<'_>,
    ) -> io::Result<()> {
        if message.role == MessageRole::Tool {
            for part in &message.parts {
                let MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = part
                else {
                    continue;
                };
                let Some((index, meta)) =
                    self.calls
                        .get(&(turn_key(lease), run.clone(), tool_use_id.clone()))
                else {
                    continue;
                };
                if document.get(*index).is_none() {
                    continue;
                }
                let mut detail = if let Some(prepared) = sources.materialized {
                    prepared
                        .detail(record, tool_use_id)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "tool detail artifact is missing",
                            )
                        })?
                        .clone()
                } else {
                    crate::history::restore_tool_item_with_sources(
                        Some(meta),
                        content,
                        *is_error,
                        sources.output,
                        None,
                    )
                    .map(Arc::new)
                };
                document.mutate(*index, OutputMutation::Semantic, |item| {
                    crate::history::finish_restored_call_shared(
                        std::slice::from_mut(item),
                        tool_use_id,
                        *is_error,
                        &mut detail,
                    )
                });
            }
            return Ok(());
        }
        let mut items = Vec::new();
        crate::history::flatten_message_with_sources(
            message,
            &mut items,
            &HashMap::new(),
            sources.output,
            None,
            sources.materialized,
        );
        let mut final_answer = false;
        for mut item in items {
            if let OutputItem::WorkFoldMarker { start_index, .. } = &mut item {
                *start_index = self.turns.get(&turn_key(lease)).map(|turn| turn.work_start);
                final_answer = true;
            }
            if let OutputItem::ToolDispatch { calls } = &item {
                for call in calls {
                    let intent = atman_runtime::message::ToolCallIntent::new(&call.intent);
                    self.calls.insert(
                        (turn_key(lease), run.clone(), call.id.clone()),
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
        if final_answer && let Some(turn) = self.turns.get_mut(&turn_key(lease)) {
            turn.work_start = document.len();
        }
        Ok(())
    }

    fn finish_calls(&self, document: &mut FlowDocument, key: &TurnKey) {
        for ((owner, _, id), (index, _)) in &self.calls {
            if owner != key {
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

#[cfg(test)]
mod source_ownership_tests {
    use super::*;

    #[test]
    fn work_panels_and_call_completion_keep_original_flow_namespaces() {
        let first = TurnLease {
            flow: FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: atman_rt::FlowInstanceId::now(),
            },
            turn: atman_rt::TurnId::now(),
            run: FlowRunId::now(),
            epoch: 1,
        };
        let mut second = first.clone();
        second.flow.instance_id = atman_rt::FlowInstanceId::now();
        let mut owner = InstanceDocument::default();
        let mut document = FlowDocument::default();
        for (position, lease) in [&first, &second].into_iter().enumerate() {
            let record = FlowRecordId {
                commit: position as u64 + 1,
                fact: 0,
                item: 0,
            };
            owner.ensure_turn(&mut document, lease, record);
            let message = Message {
                parts: vec![MessagePart::ToolUse {
                    id: "same-call".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path":"a.rs"}),
                    intent: None,
                }],
                ..Message::assistant_text(lease.turn.clone(), "")
            };
            owner
                .message(
                    &mut document,
                    lease,
                    &lease.run,
                    record,
                    &message,
                    ToolSources {
                        output: None,
                        materialized: None,
                    },
                )
                .unwrap();
        }
        assert_eq!(owner.turns.len(), 2);
        assert_ne!(
            owner.turns[&turn_key(&first)].panel,
            owner.turns[&turn_key(&second)].panel
        );
        assert_eq!(owner.calls.len(), 2);
        owner.finish_calls(&mut document, &turn_key(&first));
        for (lease, expected) in [
            (&first, ToolCallStatus::Error),
            (&second, ToolCallStatus::Running),
        ] {
            let (index, _) =
                &owner.calls[&(turn_key(lease), lease.run.clone(), "same-call".into())];
            let Some(OutputItem::ToolDispatch { calls }) = document.get(*index) else {
                panic!("tool")
            };
            assert_eq!(calls[0].status, expected);
        }
    }
}
