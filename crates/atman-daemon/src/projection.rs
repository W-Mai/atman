use std::collections::HashMap;

use atman_proto::{
    ApprovalActorProjection, ApprovalEscalationHopProjection, ApprovalExecutionBoundary,
    ApprovalGroupOwnerProjection, ApprovalGroupProjection, ApprovalPolicyProjection,
    ApprovalProvenanceProjection, ApprovalRequestProjection, ApprovalScopeProjection,
    ApprovalState, ApprovalTarget, CompactionOperationId, CompactionOutcome, CompactionProjection,
    ContextProjection, ContextUsageBucketProjection, EventCursor, FlowRunId, ImageDetail,
    InteractionProjection, InterjectionProjection, InterjectionSource, LlmCallPurpose,
    LlmCallScope, LlmUsageProjection, McpServerProjection, McpServerStateProjection,
    McpToolProjection, McpTransportProjection, MessageOrigin, MessagePart, MessageProjection,
    MessageRole, NameSource, NoticeLevel, PlanProjection, PlanStepProjection, ProjectionChange,
    ProjectionDelta, ResourceId, ResourceKind, ResourceProjection, ResourceState, Revision,
    RunLifecycle, RunProjection, SessionId, SessionLifecycle, SessionMetadataProjection,
    SessionProjection, TodoProjection, TodoState, TranscriptItem, TrustEscalation, TrustMode,
    TrustPolicyAction, TrustProjection, TrustRiskOverrides, TrustTheme, TrustTierOverrides, TurnId,
    UsageProjection, WorkflowFanoutMode, WorkflowNodeKind, WorkflowNodeProjection,
    WorkflowNodeState, WorkflowProjection, WorkflowStatementKind,
};
use atman_runtime::event::{Event, EventEnvelope, FlowStatus};
use atman_runtime::message::ImageData;
use atman_runtime::projection::message_window::FlowOwnership;
use atman_runtime::projection::workflow::WorkflowProjection as RuntimeWorkflowProjection;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionProjector {
    projection: SessionProjection,
    active_turns: Vec<atman_runtime::event::TurnId>,
    run_turns: HashMap<atman_runtime::event::FlowRunId, atman_runtime::event::TurnId>,
    workflows: Vec<(atman_runtime::event::TurnId, RuntimeWorkflowProjection)>,
    event_usage: UsageProjection,
    watch_usage: UsageProjection,
    last_runtime_seq: u64,
    ownership: FlowOwnership,
}

pub(crate) struct HistoricalProjection {
    pub cursor: EventCursor,
    pub projection: SessionProjection,
}

pub(crate) struct RestoredProjection {
    pub projector: SessionProjector,
    pub event_cursor: EventCursor,
}

impl SessionProjector {
    pub(crate) fn new(
        session_id: SessionId,
        meta: Option<atman_runtime::session_meta::SessionMeta>,
    ) -> Self {
        Self {
            projection: SessionProjection {
                revision: Revision::default(),
                metadata: metadata_projection(session_id, meta),
                lifecycle: SessionLifecycle::Idle,
                runs: Vec::new(),
                transcript: Vec::new(),
                workflows: Vec::new(),
                compactions: Vec::new(),
                goal: None,
                todos: Vec::new(),
                plans: Vec::new(),
                context: ContextProjection::default(),
                trust: TrustProjection::default(),
                interactions: InteractionProjection::default(),
                resources: Vec::new(),
                usage: UsageProjection::default(),
            },
            active_turns: Vec::new(),
            run_turns: HashMap::new(),
            workflows: Vec::new(),
            event_usage: UsageProjection::default(),
            watch_usage: UsageProjection::default(),
            last_runtime_seq: 0,
            ownership: FlowOwnership::default(),
        }
    }

    pub(crate) fn from_events(
        session_id: SessionId,
        meta: Option<atman_runtime::session_meta::SessionMeta>,
        events: &[EventEnvelope],
    ) -> Self {
        let mut projector = Self::new(session_id, meta);
        for event in events {
            projector.apply_envelope(event);
        }
        projector
    }

    pub(crate) fn projection(&self) -> &SessionProjection {
        &self.projection
    }

    pub(crate) fn last_runtime_seq(&self) -> u64 {
        self.last_runtime_seq
    }

    pub(crate) fn snapshot(&self) -> SessionProjection {
        self.projection.clone()
    }

    pub(crate) fn rebase_after_rebuild(&mut self, previous_revision: Revision) {
        self.projection.revision = Revision(previous_revision.0.saturating_add(1));
    }

    pub(crate) fn set_lifecycle(&mut self, lifecycle: SessionLifecycle) -> Option<ProjectionDelta> {
        if self.projection.lifecycle == lifecycle {
            return None;
        }
        self.projection.lifecycle = lifecycle;
        self.commit(vec![ProjectionChange::LifecycleSet { lifecycle }])
    }

    pub(crate) fn set_terminal_size(
        &mut self,
        resource_id: &ResourceId,
        rows: u16,
        cols: u16,
    ) -> Option<ProjectionDelta> {
        let resource = self.projection.resources.iter_mut().find(|resource| {
            &resource.id == resource_id && resource.kind == ResourceKind::Terminal
        })?;
        let rows = rows.to_string();
        let cols = cols.to_string();
        if resource.details.get("rows") == Some(&rows)
            && resource.details.get("cols") == Some(&cols)
        {
            return None;
        }
        resource.details.insert("rows".into(), rows);
        resource.details.insert("cols".into(), cols);
        let resource = resource.clone();
        self.commit(vec![ProjectionChange::ResourceUpsert { resource }])
    }

    pub(crate) fn reconcile_disconnected(&mut self) -> Option<ProjectionDelta> {
        let mut changes = Vec::new();
        let (lost_runs, orphaned_resources) = self.disconnected_candidates();
        self.apply_disconnected_facts(
            &lost_runs,
            &orphaned_resources,
            chrono::Utc::now(),
            "daemon disconnected before a terminal event",
            None,
            &mut changes,
        );
        self.apply_disconnected_transients(&mut changes);
        self.finish_disconnected_reconciliation(&mut changes);
        self.commit(changes)
    }

    fn apply_disconnected_transients(&mut self, changes: &mut Vec<ProjectionChange>) {
        if !self.projection.compactions.is_empty() {
            for compaction in self.projection.compactions.drain(..) {
                self.projection.transcript.push(TranscriptItem::Compaction {
                    seq: compaction.started_seq,
                    ts: compaction.started_at,
                    operation_id: Some(compaction.id),
                    context_id: compaction.context_id,
                    run_id: compaction.run_id,
                    outcome: CompactionOutcome::Abandoned,
                    range_start: compaction.range_start,
                    range_end: compaction.range_end,
                    before_tokens: compaction.before_tokens,
                    after_tokens: compaction.before_tokens,
                    summary: "compaction interrupted before completion".into(),
                });
            }
            self.projection.transcript.sort_by_key(TranscriptItem::seq);
            changes.push(ProjectionChange::TranscriptReplace {
                items: self.projection.transcript.clone(),
            });
            changes.push(ProjectionChange::CompactionsReplace {
                compactions: Vec::new(),
            });
        }

        let previous_interactions = self.projection.interactions.clone();
        self.projection.interactions.prompts.clear();
        self.projection.interactions.forms.clear();
        self.projection.interactions.compact_reviews.clear();
        for interjection in &mut self.projection.interactions.interjections {
            if interjection.state == atman_proto::InterjectionState::Pending {
                interjection.state = atman_proto::InterjectionState::Cancelled;
            }
        }
        for approval in &mut self.projection.interactions.approvals {
            if matches!(
                approval.state,
                ApprovalState::Evaluating | ApprovalState::Pending
            ) {
                approval.state = ApprovalState::Cancelled;
            }
        }
        if self.projection.interactions != previous_interactions {
            changes.push(ProjectionChange::InteractionsSet {
                interactions: self.projection.interactions.clone(),
            });
        }
    }

    fn finish_disconnected_reconciliation(&mut self, changes: &mut Vec<ProjectionChange>) {
        if self.all_runs_terminal() && self.projection.lifecycle != SessionLifecycle::Idle {
            self.projection.lifecycle = SessionLifecycle::Idle;
            changes.push(ProjectionChange::LifecycleSet {
                lifecycle: SessionLifecycle::Idle,
            });
        }
    }

    pub(crate) fn generation_reconciliation_event(&self, daemon_generation: &str) -> Option<Event> {
        let (lost_runs, orphaned_resources) = self.disconnected_candidates();
        let has_pending_interactions = !self.projection.interactions.prompts.is_empty()
            || !self.projection.interactions.forms.is_empty()
            || !self.projection.interactions.compact_reviews.is_empty()
            || self
                .projection
                .interactions
                .interjections
                .iter()
                .any(|item| item.state == atman_proto::InterjectionState::Pending)
            || self.projection.interactions.approvals.iter().any(|item| {
                matches!(
                    item.state,
                    ApprovalState::Evaluating | ApprovalState::Pending
                )
            });
        (!lost_runs.is_empty()
            || !orphaned_resources.is_empty()
            || !self.projection.compactions.is_empty()
            || has_pending_interactions)
            .then(|| Event::GenerationReconciled {
                daemon_generation: daemon_generation.to_owned(),
                reason: "daemon restarted before a terminal event".into(),
                lost_runs,
                orphaned_resources,
            })
    }

    fn disconnected_candidates(&self) -> (Vec<atman_runtime::event::FlowRunId>, Vec<String>) {
        let lost_runs = self
            .projection
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.state,
                    RunLifecycle::Queued
                        | RunLifecycle::Starting
                        | RunLifecycle::Running
                        | RunLifecycle::WaitingInput
                        | RunLifecycle::Cancelling
                )
            })
            .map(|run| atman_runtime::event::FlowRunId(run.id.0))
            .collect();
        let orphaned_resources = self
            .projection
            .resources
            .iter()
            .filter(|resource| {
                matches!(
                    resource.state,
                    ResourceState::Starting | ResourceState::Running | ResourceState::Terminating
                )
            })
            .map(|resource| resource.id.0.clone())
            .collect();
        (lost_runs, orphaned_resources)
    }

    fn apply_disconnected_facts(
        &mut self,
        lost_runs: &[atman_runtime::event::FlowRunId],
        orphaned_resources: &[String],
        at: chrono::DateTime<chrono::Utc>,
        reason: &str,
        daemon_generation: Option<&str>,
        changes: &mut Vec<ProjectionChange>,
    ) {
        for run in &mut self.projection.runs {
            if lost_runs.iter().any(|run_id| run_id.0 == run.id.0)
                && !matches!(
                    run.state,
                    RunLifecycle::Cancelled
                        | RunLifecycle::Succeeded
                        | RunLifecycle::Failed
                        | RunLifecycle::Lost
                )
            {
                run.state = RunLifecycle::Lost;
                run.finished_at = Some(at);
                run.error = Some(reason.into());
                changes.push(ProjectionChange::RunUpsert { run: run.clone() });
            }
        }
        for resource in &mut self.projection.resources {
            if orphaned_resources.iter().any(|id| id == &resource.id.0)
                && !matches!(
                    resource.state,
                    ResourceState::Exited
                        | ResourceState::Failed
                        | ResourceState::Released
                        | ResourceState::Lost
                        | ResourceState::Orphaned
                )
            {
                resource.state = ResourceState::Orphaned;
                resource.finished_at = Some(at);
                resource
                    .details
                    .insert("recovery_reason".into(), reason.into());
                if let Some(daemon_generation) = daemon_generation {
                    resource
                        .details
                        .insert("reconciled_by_generation".into(), daemon_generation.into());
                }
                changes.push(ProjectionChange::ResourceUpsert {
                    resource: resource.clone(),
                });
            }
        }
    }

    pub(crate) fn register_run(
        &mut self,
        run_id: FlowRunId,
        turn_id: atman_runtime::event::TurnId,
        flow_name: String,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<ProjectionDelta> {
        if self.projection.runs.iter().any(|run| run.id == run_id) {
            return None;
        }
        self.run_turns
            .insert(atman_runtime::event::FlowRunId(run_id.0), turn_id.clone());
        let run = RunProjection {
            id: run_id,
            turn_id: Some(TurnId(turn_id.0)),
            flow_name,
            model: None,
            provider: None,
            parent_run_id: None,
            parent_node_id: None,
            state: RunLifecycle::Starting,
            started_at,
            finished_at: None,
            error: None,
        };
        self.upsert_run(run.clone());
        if self.projection.lifecycle != SessionLifecycle::Active {
            self.projection.lifecycle = SessionLifecycle::Active;
            return self.commit(vec![
                ProjectionChange::RunUpsert { run },
                ProjectionChange::LifecycleSet {
                    lifecycle: SessionLifecycle::Active,
                },
            ]);
        }
        self.commit(vec![ProjectionChange::RunUpsert { run }])
    }

    pub(crate) fn append_compaction_text(
        &mut self,
        operation_id: &atman_runtime::event::CompactionOperationId,
        text: &str,
    ) -> Option<ProjectionDelta> {
        if text.is_empty() {
            return None;
        }
        let compaction = self
            .projection
            .compactions
            .iter_mut()
            .find(|compaction| compaction.id.0 == operation_id.0)?;
        compaction.summary.push_str(text);
        self.commit(vec![ProjectionChange::CompactionsReplace {
            compactions: self.projection.compactions.clone(),
        }])
    }

    pub(crate) fn apply_envelope(&mut self, envelope: &EventEnvelope) -> Option<ProjectionDelta> {
        if envelope.seq <= self.last_runtime_seq {
            return None;
        }
        self.last_runtime_seq = envelope.seq;
        self.ownership.observe(&envelope.event);
        let mut changes = Vec::new();

        if let Some((message, run_id)) = envelope.event.context_message() {
            self.append_transcript(
                TranscriptItem::Message {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    run_id: run_id.map(|id| FlowRunId(id.0)),
                    context_id: envelope
                        .context_id
                        .as_ref()
                        .map(|id| atman_proto::ContextId(id.0)),
                    checkpoint_index: None,
                    message: message_projection(message, envelope.seq, None),
                },
                &mut changes,
            );
        }

        match &envelope.event {
            Event::TurnStart { turn_id } => {
                if !self.active_turns.contains(turn_id) {
                    self.active_turns.push(turn_id.clone());
                }
            }
            Event::TurnEnd { turn_id } => {
                self.active_turns.retain(|active| active != turn_id);
            }
            Event::FlowStart {
                run_id,
                turn_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ..
            } => {
                let turn_id = turn_id
                    .clone()
                    .or_else(|| self.run_turns.get(run_id).cloned())
                    .or_else(|| {
                        parent_run_id
                            .as_ref()
                            .and_then(|parent| self.run_turns.get(parent))
                            .cloned()
                    })
                    .or_else(|| self.unique_active_turn());
                if let Some(turn_id) = &turn_id {
                    self.run_turns.insert(run_id.clone(), turn_id.clone());
                }
                let run = RunProjection {
                    id: FlowRunId(run_id.0),
                    turn_id: turn_id.map(|id| TurnId(id.0)),
                    flow_name: flow_name.clone(),
                    model: None,
                    provider: None,
                    parent_run_id: parent_run_id.as_ref().map(|id| FlowRunId(id.0)),
                    parent_node_id: parent_node_id.clone(),
                    state: RunLifecycle::Running,
                    started_at: envelope.ts,
                    finished_at: None,
                    error: None,
                };
                self.upsert_run(run.clone());
                changes.push(ProjectionChange::RunUpsert { run });
                if self.projection.lifecycle != SessionLifecycle::Active {
                    self.projection.lifecycle = SessionLifecycle::Active;
                    changes.push(ProjectionChange::LifecycleSet {
                        lifecycle: SessionLifecycle::Active,
                    });
                }
            }
            Event::FlowEnd { run_id, status, .. } => {
                let state = match status {
                    FlowStatus::Ok => RunLifecycle::Succeeded,
                    FlowStatus::Errored { .. } => RunLifecycle::Failed,
                    FlowStatus::Cancelled => RunLifecycle::Cancelled,
                };
                let error = match status {
                    FlowStatus::Errored { message } => Some(message.clone()),
                    _ => None,
                };
                let run = if let Some(run) = self
                    .projection
                    .runs
                    .iter_mut()
                    .find(|run| run.id.0 == run_id.0)
                {
                    run.state = state;
                    run.finished_at = Some(envelope.ts);
                    run.error = error;
                    run.clone()
                } else {
                    let run = RunProjection {
                        id: FlowRunId(run_id.0),
                        turn_id: self.run_turns.get(run_id).map(|id| TurnId(id.0)),
                        flow_name: String::new(),
                        model: None,
                        provider: None,
                        parent_run_id: None,
                        parent_node_id: None,
                        state,
                        started_at: envelope.ts,
                        finished_at: Some(envelope.ts),
                        error,
                    };
                    self.upsert_run(run.clone());
                    run
                };
                changes.push(ProjectionChange::RunUpsert { run });
                if self.all_runs_terminal() && self.projection.lifecycle != SessionLifecycle::Idle {
                    self.projection.lifecycle = SessionLifecycle::Idle;
                    changes.push(ProjectionChange::LifecycleSet {
                        lifecycle: SessionLifecycle::Idle,
                    });
                }
            }
            Event::RunCancelRequested { run_id } => {
                if let Some(run) = self
                    .projection
                    .runs
                    .iter_mut()
                    .find(|run| run.id.0 == run_id.0)
                    && !matches!(
                        run.state,
                        RunLifecycle::Cancelled
                            | RunLifecycle::Succeeded
                            | RunLifecycle::Failed
                            | RunLifecycle::Lost
                    )
                {
                    run.state = RunLifecycle::Cancelling;
                    changes.push(ProjectionChange::RunUpsert { run: run.clone() });
                }
            }
            Event::GenerationReconciled {
                daemon_generation,
                reason,
                lost_runs,
                orphaned_resources,
            } => {
                self.apply_disconnected_facts(
                    lost_runs,
                    orphaned_resources,
                    envelope.ts,
                    reason,
                    Some(daemon_generation),
                    &mut changes,
                );
                self.apply_disconnected_transients(&mut changes);
                self.finish_disconnected_reconciliation(&mut changes);
            }
            Event::DiffPreview {
                flow_run_id,
                tool_use_id,
                title,
                old_content,
                new_content,
                unified_diff,
                ..
            } => self.append_transcript(
                TranscriptItem::Diff {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                    tool_use_id: tool_use_id.clone(),
                    title: title.clone(),
                    old_content: old_content.clone(),
                    new_content: new_content.clone(),
                    unified_diff: unified_diff.clone(),
                },
                &mut changes,
            ),
            Event::FileEditApplied {
                turn_id,
                flow_run_id,
                tool_use_id,
                tool_name,
                path,
                metrics,
            } => self.append_transcript(
                TranscriptItem::FileEdit {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    turn_id: turn_id.as_ref().map(|id| TurnId(id.0)),
                    run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                    tool_use_id: tool_use_id.clone(),
                    tool_name: tool_name.clone(),
                    path: path.clone(),
                    added_lines: metrics.insertions as u64,
                    removed_lines: metrics.deletions as u64,
                    hunks: metrics.hunks as u64,
                },
                &mut changes,
            ),
            Event::CompactionStarted {
                operation_id,
                flow_run_id,
                range_start,
                range_end,
                compacted_count,
                before_tokens,
            } => {
                let compaction = CompactionProjection {
                    id: CompactionOperationId(operation_id.0),
                    started_seq: envelope.seq,
                    context_id: envelope
                        .context_id
                        .as_ref()
                        .map(|id| atman_proto::ContextId(id.0)),
                    run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                    range_start: *range_start,
                    range_end: *range_end,
                    before_tokens: *before_tokens,
                    compacted_count: *compacted_count as u64,
                    summary: String::new(),
                    started_at: envelope.ts,
                };
                self.projection
                    .compactions
                    .retain(|existing| existing.id != compaction.id);
                self.projection.compactions.push(compaction);
                self.projection
                    .compactions
                    .sort_by_key(|compaction| compaction.started_seq);
                changes.push(ProjectionChange::CompactionsReplace {
                    compactions: self.projection.compactions.clone(),
                });
            }
            Event::CompactionSummary {
                operation_id,
                flow_run_id,
                range_start,
                range_end,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => {
                if let Some(operation_id) = operation_id {
                    let before = self.projection.compactions.len();
                    self.projection
                        .compactions
                        .retain(|compaction| compaction.id.0 != operation_id.0);
                    if self.projection.compactions.len() != before {
                        changes.push(ProjectionChange::CompactionsReplace {
                            compactions: self.projection.compactions.clone(),
                        });
                    }
                }
                self.append_transcript(
                    TranscriptItem::Compaction {
                        seq: envelope.seq,
                        ts: envelope.ts,
                        operation_id: operation_id.as_ref().map(|id| CompactionOperationId(id.0)),
                        context_id: envelope
                            .context_id
                            .as_ref()
                            .map(|id| atman_proto::ContextId(id.0)),
                        run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                        outcome: CompactionOutcome::Finished,
                        range_start: *range_start,
                        range_end: *range_end,
                        before_tokens: *before_tokens,
                        after_tokens: *after_tokens,
                        summary: summary.clone(),
                    },
                    &mut changes,
                );
            }
            Event::CompactionFailed {
                operation_id,
                flow_run_id,
                range_start,
                range_end,
                compacted_count: _,
                before_tokens,
                reason,
            } => {
                self.projection
                    .compactions
                    .retain(|compaction| compaction.id.0 != operation_id.0);
                changes.push(ProjectionChange::CompactionsReplace {
                    compactions: self.projection.compactions.clone(),
                });
                self.append_transcript(
                    TranscriptItem::Compaction {
                        seq: envelope.seq,
                        ts: envelope.ts,
                        operation_id: Some(CompactionOperationId(operation_id.0)),
                        context_id: envelope
                            .context_id
                            .as_ref()
                            .map(|id| atman_proto::ContextId(id.0)),
                        run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                        outcome: CompactionOutcome::Failed,
                        range_start: *range_start,
                        range_end: *range_end,
                        before_tokens: *before_tokens,
                        after_tokens: *before_tokens,
                        summary: reason.clone(),
                    },
                    &mut changes,
                );
            }
            Event::ContextCompact {
                flow_run_id,
                compacted_range_start,
                compacted_range_end,
                replacement_msg_seq,
                ..
            } => {
                if replacement_msg_seq.is_some_and(|replacement_seq| {
                    self.compact_transcript_messages(
                        envelope.context_id.as_ref(),
                        flow_run_id.as_ref(),
                        *compacted_range_start as usize,
                        *compacted_range_end as usize,
                        replacement_seq,
                    )
                }) {
                    changes.push(ProjectionChange::TranscriptReplace {
                        items: self.projection.transcript.clone(),
                    });
                }
            }
            Event::Checkpoint {
                flow_run_id,
                messages,
                ..
            } => {
                let run_id = flow_run_id.as_ref().map(|id| FlowRunId(id.0));
                let replacement = messages
                    .iter()
                    .enumerate()
                    .map(|(index, message)| TranscriptItem::Message {
                        seq: envelope.seq,
                        ts: envelope.ts,
                        run_id: run_id.clone(),
                        context_id: envelope
                            .context_id
                            .as_ref()
                            .map(|id| atman_proto::ContextId(id.0)),
                        checkpoint_index: Some(index),
                        message: message_projection(message, envelope.seq, Some(index)),
                    })
                    .collect();
                if self.replace_transcript_messages(
                    envelope.context_id.as_ref(),
                    flow_run_id.as_ref(),
                    replacement,
                ) {
                    changes.push(ProjectionChange::TranscriptReplace {
                        items: self.projection.transcript.clone(),
                    });
                }
            }
            Event::AttachmentDegraded {
                flow_run_id, patch, ..
            } => {
                let slots = transcript_message_slots(
                    &self.projection.transcript,
                    &self.ownership,
                    envelope.context_id.as_ref(),
                    flow_run_id.as_ref(),
                );
                let mut changed = false;
                for (output_index, seq) in slots {
                    let TranscriptItem::Message { message, .. } =
                        &mut self.projection.transcript[output_index]
                    else {
                        continue;
                    };
                    for (part_index, part) in message.parts.iter_mut().enumerate() {
                        if let MessagePart::Image { id, .. } = part
                            && patch.target.matches(
                                id.map(|id| atman_runtime::message::MessagePartId(id.0)),
                                seq,
                                part_index,
                            )
                        {
                            *part = MessagePart::Text { text: patch.text() };
                            changed = true;
                        }
                    }
                }
                if changed {
                    changes.push(ProjectionChange::TranscriptReplace {
                        items: self.projection.transcript.clone(),
                    });
                }
            }
            Event::MermaidDiagram { source } => self.append_transcript(
                TranscriptItem::Mermaid {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    source: source.clone(),
                },
                &mut changes,
            ),
            Event::WatchWarn { message, .. } => self.append_transcript(
                TranscriptItem::Notice {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    level: NoticeLevel::Warning,
                    text: message.clone(),
                },
                &mut changes,
            ),
            Event::WorkspaceLifecycle {
                run_id,
                workspace_id,
                path,
                state,
                cleanup_error,
                reconciliation_reason,
            } => {
                let id = ResourceId(format!("workspace:{workspace_id}"));
                let started_at = self
                    .projection
                    .resources
                    .iter()
                    .find(|resource| resource.id == id)
                    .and_then(|resource| resource.started_at)
                    .or(Some(envelope.ts));
                let resource = ResourceProjection {
                    id,
                    kind: ResourceKind::Workspace,
                    state: workspace_state(state),
                    owner_run_id: FlowRunId(run_id.0),
                    tool_use_id: None,
                    label: path.clone(),
                    started_at,
                    finished_at: resource_is_terminal(state).then_some(envelope.ts),
                    details: cleanup_error
                        .iter()
                        .map(|error| ("cleanup_error".into(), error.clone()))
                        .chain(
                            reconciliation_reason
                                .iter()
                                .map(|reason| ("reconciliation_reason".into(), reason.clone())),
                        )
                        .collect(),
                };
                self.upsert_resource(resource.clone());
                changes.push(ProjectionChange::ResourceUpsert { resource });
            }
            Event::TaskLifecycle {
                task_id,
                kind,
                run_id: Some(run_id),
                source_handle,
                label,
                command,
                workspace_id,
                status,
                termination,
            } if !matches!(kind, atman_runtime::TaskKind::Flow) => {
                let id = task_resource_id(task_id);
                let previous = self
                    .projection
                    .resources
                    .iter()
                    .find(|resource| resource.id == id);
                let started_at = previous
                    .and_then(|resource| resource.started_at)
                    .or(Some(envelope.ts));
                let state = task_resource_state(*status);
                let mut details = previous
                    .map(|resource| resource.details.clone())
                    .unwrap_or_default();
                details.insert("source_handle".into(), source_handle.clone());
                if let Some(command) = command {
                    details.insert("command".into(), command.clone());
                }
                if let Some(workspace_id) = workspace_id {
                    details.insert("workspace_id".into(), workspace_id.clone());
                }
                if let Some(termination) = termination {
                    details.insert(
                        "termination".into(),
                        match termination {
                            atman_runtime::task_registry::TaskTermination::Killed => "killed",
                            atman_runtime::task_registry::TaskTermination::Suicide => "suicide",
                        }
                        .into(),
                    );
                }
                let resource = ResourceProjection {
                    id,
                    kind: match kind {
                        atman_runtime::TaskKind::Bash => ResourceKind::BackgroundProcess,
                        atman_runtime::TaskKind::Terminal => ResourceKind::Terminal,
                        atman_runtime::TaskKind::Flow => unreachable!(),
                    },
                    state,
                    owner_run_id: FlowRunId(run_id.0),
                    tool_use_id: None,
                    label: label.clone(),
                    started_at,
                    finished_at: task_resource_is_terminal(*status).then_some(envelope.ts),
                    details,
                };
                self.upsert_resource(resource.clone());
                changes.push(ProjectionChange::ResourceUpsert { resource });
            }
            Event::TaskLifecycle { .. } => {}
            Event::TerminalFinalState { handle, screen, .. } => {
                if let Some(resource) = self.projection.resources.iter_mut().find(|resource| {
                    resource.kind == ResourceKind::Terminal
                        && resource.details.get("source_handle") == Some(handle)
                }) {
                    resource
                        .details
                        .insert("rows".into(), screen.rows.to_string());
                    resource
                        .details
                        .insert("cols".into(), screen.cols.to_string());
                    changes.push(ProjectionChange::ResourceUpsert {
                        resource: resource.clone(),
                    });
                }
            }
            Event::TaskReaped { task_id } => {
                let resource_id = task_resource_id(task_id);
                let previous_len = self.projection.resources.len();
                self.projection
                    .resources
                    .retain(|resource| resource.id != resource_id);
                if self.projection.resources.len() != previous_len {
                    changes.push(ProjectionChange::ResourceRemove { resource_id });
                }
            }
            Event::PendingPrompt {
                prompt_id,
                kind,
                payload,
            } => {
                self.projection
                    .interactions
                    .prompts
                    .retain(|item| item.id.0 != *prompt_id);
                self.projection
                    .interactions
                    .prompts
                    .push(atman_proto::PendingPromptProjection {
                        id: atman_proto::PromptId(*prompt_id),
                        kind: kind.clone(),
                        payload: payload.clone(),
                    });
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::PromptResolved { prompt_id, .. } => {
                let before = self.projection.interactions.prompts.len();
                self.projection
                    .interactions
                    .prompts
                    .retain(|item| item.id.0 != *prompt_id);
                if before != self.projection.interactions.prompts.len() {
                    changes.push(ProjectionChange::InteractionsSet {
                        interactions: self.projection.interactions.clone(),
                    });
                }
            }
            Event::FormRequested { form } => {
                let form = pending_form_projection(form);
                self.projection
                    .interactions
                    .forms
                    .retain(|item| item.id != form.id);
                self.projection.interactions.forms.push(form);
                self.projection
                    .interactions
                    .forms
                    .sort_by_key(|item| item.emitted_at);
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::FormResolved { form_id, .. } => {
                let before = self.projection.interactions.forms.len();
                self.projection
                    .interactions
                    .forms
                    .retain(|item| item.id != *form_id);
                if before != self.projection.interactions.forms.len() {
                    changes.push(ProjectionChange::InteractionsSet {
                        interactions: self.projection.interactions.clone(),
                    });
                }
            }
            Event::CompactReviewRequested { review } => {
                let review = compact_review_projection(review);
                self.projection
                    .interactions
                    .compact_reviews
                    .retain(|item| item.id != review.id);
                self.projection.interactions.compact_reviews.push(review);
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::CompactReviewResolved { review_id, .. } => {
                let before = self.projection.interactions.compact_reviews.len();
                self.projection
                    .interactions
                    .compact_reviews
                    .retain(|item| item.id != *review_id);
                if before != self.projection.interactions.compact_reviews.len() {
                    changes.push(ProjectionChange::InteractionsSet {
                        interactions: self.projection.interactions.clone(),
                    });
                }
            }
            Event::UserInject { injection, .. } => {
                let interjection = interjection_projection(injection);
                self.projection
                    .interactions
                    .interjections
                    .retain(|item| item.id != interjection.id);
                self.projection
                    .interactions
                    .interjections
                    .push(interjection);
                self.projection
                    .interactions
                    .interjections
                    .sort_by_key(|item| item.created_at);
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::PermissionRequestCreated { payload }
            | Event::PermissionRequestTargeted { payload }
            | Event::PermissionRequestDeferred { payload } => {
                self.upsert_approval(payload, ApprovalState::Pending, &mut changes)
            }
            Event::PermissionRequestApproved { payload }
            | Event::UnrestrictedExecution { payload } => {
                self.upsert_approval(payload, ApprovalState::Approved, &mut changes)
            }
            Event::PermissionRequestDenied { payload } => {
                self.upsert_approval(payload, ApprovalState::Denied, &mut changes)
            }
            Event::PermissionRequestCancelled { payload } => {
                self.upsert_approval(payload, ApprovalState::Cancelled, &mut changes)
            }
            Event::PermissionGroupCreated { payload }
            | Event::PermissionGroupUpdated { payload }
            | Event::PermissionGroupResolved { payload } => {
                let resolved = matches!(&envelope.event, Event::PermissionGroupResolved { .. });
                let group = approval_group_projection(payload, resolved);
                self.projection
                    .interactions
                    .approval_groups
                    .retain(|item| item.id != group.id);
                self.projection.interactions.approval_groups.push(group);
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::LlmCall {
                model,
                provider,
                managed_context,
                context_call_purpose,
                context_call_identity,
                run_id,
                usage,
                ..
            } => {
                let previous_usage = self.projection.usage.clone();
                let previous_context = self.projection.context.clone();
                let purpose = context_call_purpose.unwrap_or_default();
                let scope =
                    context_call_identity
                        .as_ref()
                        .map(|identity| identity.scope)
                        .unwrap_or_else(|| match run_id {
                            None => atman_runtime::context_plan::ContextCallScope::Detached,
                            Some(run_id)
                                if self.projection.runs.iter().any(|run| {
                                    run.id.0 == run_id.0 && run.parent_run_id.is_some()
                                }) =>
                            {
                                atman_runtime::context_plan::ContextCallScope::Child
                            }
                            Some(_) => atman_runtime::context_plan::ContextCallScope::Root,
                        });
                self.event_usage.input_tokens = self
                    .event_usage
                    .input_tokens
                    .saturating_add(usage.prompt_input());
                self.event_usage.output_tokens =
                    self.event_usage.output_tokens.saturating_add(usage.output);
                self.event_usage.cache_read_tokens = self
                    .event_usage
                    .cache_read_tokens
                    .saturating_add(usage.cached_input);
                self.event_usage.cache_write_tokens = self
                    .event_usage
                    .cache_write_tokens
                    .saturating_add(usage.cache_write);
                self.event_usage.llm_calls = self.event_usage.llm_calls.saturating_add(1);
                self.refresh_usage();
                if purpose == atman_runtime::context_plan::ContextCallPurpose::General {
                    if let Some(run_id) = run_id
                        && let Some(run) = self
                            .projection
                            .runs
                            .iter_mut()
                            .find(|run| run.id.0 == run_id.0)
                        && (run.model.as_ref() != Some(model)
                            || run.provider.as_ref() != Some(provider))
                    {
                        run.model = Some(model.clone());
                        run.provider = Some(provider.clone());
                        changes.push(ProjectionChange::RunUpsert { run: run.clone() });
                    }
                    if scope == atman_runtime::context_plan::ContextCallScope::Root
                        && managed_context.unwrap_or(true)
                        && envelope.context_id.is_none()
                    {
                        self.projection.context.model = model.clone();
                        self.projection.context.provider = provider.clone();
                    }
                }
                self.projection.context.cache_read_tokens = self.projection.usage.cache_read_tokens;
                self.projection.context.cache_write_tokens =
                    self.projection.usage.cache_write_tokens;
                if self.projection.usage != previous_usage {
                    changes.push(ProjectionChange::UsageSet {
                        usage: self.projection.usage.clone(),
                    });
                }
                if self.projection.context != previous_context {
                    changes.push(ProjectionChange::ContextSet {
                        context: self.projection.context.clone(),
                    });
                }
            }
            _ => {}
        }

        if self.apply_workflow_event(envelope) {
            self.projection.workflows = self
                .workflows
                .iter()
                .map(|(_, workflow)| workflow_projection(workflow))
                .collect();
            changes.push(ProjectionChange::WorkflowsReplace {
                workflows: self.projection.workflows.clone(),
            });
        }

        self.commit(changes)
    }

    pub(crate) fn set_metadata(
        &mut self,
        meta: Option<atman_runtime::session_meta::SessionMeta>,
    ) -> Option<ProjectionDelta> {
        let metadata = metadata_projection(self.projection.metadata.id.clone(), meta);
        if metadata == self.projection.metadata {
            return None;
        }
        self.projection.metadata = metadata.clone();
        self.commit(vec![ProjectionChange::MetadataSet { metadata }])
    }

    pub(crate) fn set_goal(&mut self, goal: Option<String>) -> Option<ProjectionDelta> {
        if self.projection.goal == goal {
            return None;
        }
        self.projection.goal = goal.clone();
        self.commit(vec![ProjectionChange::GoalSet { goal }])
    }

    pub(crate) fn set_todos(
        &mut self,
        todos: Vec<atman_runtime::memory::todo::Todo>,
    ) -> Option<ProjectionDelta> {
        let todos = todos.into_iter().map(todo_projection).collect::<Vec<_>>();
        if self.projection.todos == todos {
            return None;
        }
        self.projection.todos = todos.clone();
        self.commit(vec![ProjectionChange::TodosReplace { todos }])
    }

    pub(crate) fn set_plans(
        &mut self,
        plans: Vec<atman_runtime::memory::plan::Plan>,
    ) -> Option<ProjectionDelta> {
        let plans = plans.into_iter().map(plan_projection).collect::<Vec<_>>();
        if self.projection.plans == plans {
            return None;
        }
        self.projection.plans = plans.clone();
        self.commit(vec![ProjectionChange::PlansReplace { plans }])
    }

    pub(crate) fn set_context(
        &mut self,
        context: atman_runtime::ContextSnapshot,
    ) -> Option<ProjectionDelta> {
        let next = context_projection(&context);
        self.watch_usage = UsageProjection {
            input_tokens: context.tokens_in,
            output_tokens: context.tokens_out,
            cache_read_tokens: context.cache_read,
            cache_write_tokens: context.cache_write,
            cost_usd: context.cost_usd,
            llm_calls: context
                .usage_buckets
                .iter()
                .map(|bucket| bucket.calls)
                .sum(),
        };
        let usage = merged_usage(&self.event_usage, &self.watch_usage);
        if self.projection.context == next && self.projection.usage == usage {
            return None;
        }
        self.projection.context = next.clone();
        self.projection.usage = usage.clone();
        self.commit(vec![
            ProjectionChange::ContextSet { context: next },
            ProjectionChange::UsageSet { usage },
        ])
    }

    pub(crate) fn set_trust(
        &mut self,
        trust: atman_runtime::trust::TrustConfig,
    ) -> Option<ProjectionDelta> {
        let trust = trust_projection(&trust);
        if self.projection.trust == trust {
            return None;
        }
        self.projection.trust = trust.clone();
        self.commit(vec![ProjectionChange::TrustSet { trust }])
    }

    fn refresh_usage(&mut self) {
        self.projection.usage = merged_usage(&self.event_usage, &self.watch_usage);
    }

    fn commit(&mut self, changes: Vec<ProjectionChange>) -> Option<ProjectionDelta> {
        if changes.is_empty() {
            return None;
        }
        let base_revision = self.projection.revision;
        self.projection.revision.0 = self.projection.revision.0.saturating_add(1);
        Some(ProjectionDelta {
            base_revision,
            revision: self.projection.revision,
            changes,
        })
    }

    fn append_transcript(&mut self, item: TranscriptItem, changes: &mut Vec<ProjectionChange>) {
        self.projection.transcript.push(item.clone());
        changes.push(ProjectionChange::TranscriptAppend { items: vec![item] });
    }

    fn upsert_run(&mut self, run: RunProjection) {
        self.projection
            .runs
            .retain(|existing| existing.id != run.id);
        self.projection.runs.push(run);
        self.projection.runs.sort_by_key(|run| run.started_at);
    }

    fn upsert_resource(&mut self, resource: ResourceProjection) {
        self.projection
            .resources
            .retain(|existing| existing.id != resource.id);
        self.projection.resources.push(resource);
    }

    fn all_runs_terminal(&self) -> bool {
        self.projection.runs.iter().all(|run| {
            matches!(
                run.state,
                RunLifecycle::Cancelled
                    | RunLifecycle::Succeeded
                    | RunLifecycle::Failed
                    | RunLifecycle::Lost
            )
        })
    }

    fn compact_transcript_messages(
        &mut self,
        context_id: Option<&atman_runtime::event::ContextId>,
        run_id: Option<&atman_runtime::event::FlowRunId>,
        range_start: usize,
        range_end: usize,
        replacement_seq: u64,
    ) -> bool {
        let slots = transcript_message_slots(
            &self.projection.transcript,
            &self.ownership,
            context_id,
            run_id,
        );
        if range_start > range_end || range_end >= slots.len() {
            return false;
        }
        let Some(replacement_position) = slots.iter().position(|(_, seq)| *seq == replacement_seq)
        else {
            return false;
        };
        let insertion_output_index = slots[range_start].0;
        let replacement_output_index = slots[replacement_position].0;
        let replacement = self.projection.transcript[replacement_output_index].clone();
        let removed = slots[range_start..=range_end]
            .iter()
            .map(|(output_index, _)| *output_index)
            .chain(std::iter::once(replacement_output_index))
            .collect::<std::collections::HashSet<_>>();
        let mut compacted = Vec::with_capacity(
            self.projection
                .transcript
                .len()
                .saturating_sub(removed.len())
                .saturating_add(1),
        );
        for (output_index, item) in self.projection.transcript.drain(..).enumerate() {
            if output_index == insertion_output_index {
                compacted.push(replacement.clone());
            }
            if !removed.contains(&output_index) {
                compacted.push(item);
            }
        }
        self.projection.transcript = compacted;
        true
    }

    fn replace_transcript_messages(
        &mut self,
        context_id: Option<&atman_runtime::event::ContextId>,
        run_id: Option<&atman_runtime::event::FlowRunId>,
        replacement: Vec<TranscriptItem>,
    ) -> bool {
        let slots = transcript_message_slots(
            &self.projection.transcript,
            &self.ownership,
            context_id,
            run_id,
        );
        let existing = slots
            .iter()
            .map(|(output_index, _)| &self.projection.transcript[*output_index])
            .collect::<Vec<_>>();
        if existing.iter().copied().eq(replacement.iter()) {
            return false;
        }
        let removed = slots
            .iter()
            .map(|(output_index, _)| *output_index)
            .collect::<std::collections::HashSet<_>>();
        let insertion_output_index = slots
            .first()
            .map(|(output_index, _)| *output_index)
            .unwrap_or(self.projection.transcript.len());
        let mut replacement = Some(replacement);
        let mut transcript = Vec::with_capacity(
            self.projection
                .transcript
                .len()
                .saturating_sub(removed.len())
                .saturating_add(replacement.as_ref().map_or(0, Vec::len)),
        );
        for (output_index, item) in self.projection.transcript.drain(..).enumerate() {
            if output_index == insertion_output_index {
                transcript.append(replacement.as_mut().expect("replacement inserted once"));
            }
            if !removed.contains(&output_index) {
                transcript.push(item);
            }
        }
        if let Some(mut replacement) = replacement {
            transcript.append(&mut replacement);
        }
        self.projection.transcript = transcript;
        true
    }

    fn upsert_approval(
        &mut self,
        payload: &atman_runtime::permission_audit::PermissionRequestAudit,
        state: ApprovalState,
        changes: &mut Vec<ProjectionChange>,
    ) {
        let Some(approval) = approval_request_projection(payload, state) else {
            return;
        };
        self.projection
            .interactions
            .approvals
            .retain(|item| item.id != approval.id);
        self.projection.interactions.approvals.push(approval);
        changes.push(ProjectionChange::InteractionsSet {
            interactions: self.projection.interactions.clone(),
        });
    }

    fn apply_workflow_event(&mut self, envelope: &EventEnvelope) -> bool {
        if matches!(
            &envelope.event,
            Event::PermissionGroupCreated { .. }
                | Event::PermissionGroupUpdated { .. }
                | Event::PermissionGroupResolved { .. }
        ) {
            let mut changed = false;
            for (_, workflow) in &mut self.workflows {
                changed |= workflow
                    .apply_event_at(&envelope.event, envelope.ts)
                    .changed();
            }
            return changed;
        }
        let Some(run_id) = event_run_id(&envelope.event) else {
            return false;
        };
        let turn_id = self
            .run_turns
            .get(run_id)
            .cloned()
            .or_else(|| self.unique_active_turn())
            .unwrap_or_else(orphan_turn_id);
        self.run_turns
            .entry(run_id.clone())
            .or_insert_with(|| turn_id.clone());
        let workflow = if let Some((_, workflow)) = self
            .workflows
            .iter_mut()
            .find(|(existing, _)| existing == &turn_id)
        {
            workflow
        } else {
            self.workflows
                .push((turn_id.clone(), RuntimeWorkflowProjection::new(turn_id)));
            &mut self.workflows.last_mut().expect("workflow inserted").1
        };
        workflow
            .apply_event_at(&envelope.event, envelope.ts)
            .changed()
    }

    fn unique_active_turn(&self) -> Option<atman_runtime::event::TurnId> {
        if self.active_turns.len() != 1 {
            return None;
        }
        self.active_turns.first().cloned()
    }
}

pub(crate) fn redacted_projection(
    projection: &SessionProjection,
    redactor: Option<&atman_runtime::redact::Redactor>,
) -> anyhow::Result<SessionProjection> {
    let Some(redactor) = redactor else {
        return Ok(projection.clone());
    };
    let mut value = serde_json::to_value(projection)?;
    redactor.redact_json(&mut value);
    Ok(serde_json::from_value(value)?)
}

pub(crate) fn redacted_updates(
    updates: &atman_proto::GetSessionUpdatesResponse,
    redactor: Option<&atman_runtime::redact::Redactor>,
) -> anyhow::Result<atman_proto::GetSessionUpdatesResponse> {
    let Some(redactor) = redactor else {
        return Ok(updates.clone());
    };
    let mut updates = updates.clone();
    for event in &mut updates.events {
        redact_terminal_bytes(event, redactor);
    }
    let mut value = serde_json::to_value(updates)?;
    redactor.redact_json(&mut value);
    Ok(serde_json::from_value(value)?)
}

pub(crate) fn redacted_projection_event(
    event: &atman_proto::ProjectionEventEnvelope,
    redactor: Option<&atman_runtime::redact::Redactor>,
) -> anyhow::Result<atman_proto::ProjectionEventEnvelope> {
    let Some(redactor) = redactor else {
        return Ok(event.clone());
    };
    let mut event = event.clone();
    redact_terminal_bytes(&mut event, redactor);
    let mut value = serde_json::to_value(event)?;
    redactor.redact_json(&mut value);
    Ok(serde_json::from_value(value)?)
}

fn redact_terminal_bytes(
    event: &mut atman_proto::ProjectionEventEnvelope,
    redactor: &atman_runtime::redact::Redactor,
) {
    let atman_proto::ServerEvent::Signal {
        signal: atman_proto::SessionSignal::TerminalBytes { bytes, .. },
    } = &mut event.event
    else {
        return;
    };
    let mut redacted = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii() {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii() {
                cursor += 1;
            }
            let text = std::str::from_utf8(&bytes[start..cursor])
                .expect("an ASCII byte range is valid UTF-8");
            redacted.extend_from_slice(redactor.redact(text).0.as_bytes());
        } else {
            redacted.push(bytes[cursor]);
            cursor += 1;
        }
    }
    *bytes = redacted;
}

pub(crate) async fn load_historical_projection(
    session_id: SessionId,
    session_dir: &std::path::Path,
    fallback_trust: atman_runtime::trust::TrustConfig,
) -> anyhow::Result<HistoricalProjection> {
    let replay_dir = session_dir.to_path_buf();
    let replay_session_id = session_id.clone();
    let (meta, mut projector, mut event_cursor, context, goal, trust) =
        tokio::task::spawn_blocking(move || {
            let events_path = replay_dir.join("events.jsonl");
            anyhow::ensure!(
                events_path.is_file(),
                "session not found: {replay_session_id}"
            );
            let meta = atman_runtime::session_meta::SessionMeta::load(&replay_dir);
            let (projector, event_cursor, context) =
                match crate::projection_snapshot::load(&replay_session_id, &replay_dir)? {
                    Some(loaded) => (loaded.projector, loaded.event_cursor, None),
                    None => {
                        let events =
                            atman_runtime::event_log::reader::read_event_envelopes(&events_path)?;
                        let context =
                            atman_runtime::event_log::reader::context_snapshot_from_envelopes(
                                &events,
                            )?;
                        let projector = SessionProjector::from_events(
                            replay_session_id.clone(),
                            meta.clone(),
                            &events,
                        );
                        let event_cursor = EventCursor(projector.projection().revision.0);
                        (projector, event_cursor, Some(context))
                    }
                };
            let goal = atman_runtime::memory::goal::GoalStore::at(&replay_dir).get()?;
            let trust =
                atman_runtime::session::load_session_trust(&replay_dir)?.unwrap_or(fallback_trust);
            Ok::<_, anyhow::Error>((meta, projector, event_cursor, context, goal, trust))
        })
        .await
        .map_err(|error| anyhow::anyhow!("historical session replay task failed: {error}"))??;

    let todo_store = atman_runtime::memory::todo::TodoStore::at(session_dir);
    let plan_store = atman_runtime::memory::plan::PlanStore::at(session_dir);
    let (todos, plans) = tokio::join!(todo_store.list(), plan_store.list());
    let previous_revision = projector.projection().revision.0;
    projector.set_metadata(meta);
    projector.set_goal((!goal.is_empty()).then_some(goal));
    projector.set_todos(todos?);
    projector.set_plans(plans?);
    projector.set_trust(trust);
    if let Some(context) = context {
        projector.set_context(context);
    }
    projector.reconcile_disconnected();
    event_cursor.0 = event_cursor.0.saturating_add(
        projector
            .projection()
            .revision
            .0
            .saturating_sub(previous_revision),
    );
    Ok(HistoricalProjection {
        cursor: event_cursor,
        projection: projector.snapshot(),
    })
}

fn transcript_message_slots(
    transcript: &[TranscriptItem],
    ownership: &FlowOwnership,
    context_id: Option<&atman_runtime::event::ContextId>,
    run_id: Option<&atman_runtime::event::FlowRunId>,
) -> Vec<(usize, u64)> {
    let context_run = ownership.context_run(run_id);
    transcript
        .iter()
        .enumerate()
        .filter_map(|(output_index, item)| match item {
            TranscriptItem::Message {
                seq,
                run_id: item_run_id,
                context_id: item_context_id,
                checkpoint_index,
                ..
            } if item_context_id.as_ref().map(|id| id.0) == context_id.map(|id| id.0)
                && (context_id.is_some()
                    || ownership.context_run(
                        item_run_id
                            .as_ref()
                            .map(|id| atman_runtime::event::FlowRunId(id.0))
                            .as_ref(),
                    ) == context_run) =>
            {
                Some((
                    output_index,
                    checkpoint_index.map_or(*seq, |index| u64::MAX.saturating_sub(index as u64)),
                ))
            }
            _ => None,
        })
        .collect()
}

fn metadata_projection(
    session_id: SessionId,
    meta: Option<atman_runtime::session_meta::SessionMeta>,
) -> SessionMetadataProjection {
    let meta = meta.unwrap_or_default();
    SessionMetadataProjection {
        id: session_id,
        title: meta.title.unwrap_or_else(|| "Untitled session".into()),
        name_source: match meta.name_source {
            atman_runtime::session_meta::NameSource::Auto => NameSource::Auto,
            atman_runtime::session_meta::NameSource::User => NameSource::User,
        },
        project_root: meta.project_root.map(|path| path.display().to_string()),
        created_at: meta.created_at,
        updated_at: None,
    }
}

pub(crate) fn trust_projection(trust: &atman_runtime::trust::TrustConfig) -> TrustProjection {
    let action = |action: Option<atman_runtime::trust::PolicyAction>| {
        action.map(|action| match action {
            atman_runtime::trust::PolicyAction::Auto => TrustPolicyAction::Auto,
            atman_runtime::trust::PolicyAction::Ask => TrustPolicyAction::Ask,
            atman_runtime::trust::PolicyAction::Deny => TrustPolicyAction::Deny,
        })
    };
    TrustProjection {
        mode: match trust.mode {
            atman_runtime::trust::TrustMode::Calm => TrustMode::Calm,
            atman_runtime::trust::TrustMode::Steady => TrustMode::Steady,
            atman_runtime::trust::TrustMode::Eager => TrustMode::Eager,
            atman_runtime::trust::TrustMode::Reckless => TrustMode::Reckless,
        },
        theme: match trust.theme {
            atman_runtime::trust::Theme::Default => TrustTheme::Default,
            atman_runtime::trust::Theme::Wuxia => TrustTheme::Wuxia,
            atman_runtime::trust::Theme::Animal => TrustTheme::Animal,
            atman_runtime::trust::Theme::Weather => TrustTheme::Weather,
            atman_runtime::trust::Theme::Drink => TrustTheme::Drink,
        },
        escalation: match trust.escalation {
            atman_runtime::trust::EscalationPolicy::Deny => TrustEscalation::Deny,
            atman_runtime::trust::EscalationPolicy::Ask => TrustEscalation::Ask,
            atman_runtime::trust::EscalationPolicy::Allow => TrustEscalation::Allow,
        },
        eager_tiers: TrustTierOverrides {
            tier0: action(trust.tiers.eager.tier0),
            tier1: action(trust.tiers.eager.tier1),
            tier2: action(trust.tiers.eager.tier2),
            tier3: action(trust.tiers.eager.tier3),
            tier4: action(trust.tiers.eager.tier4),
        },
        eager_risks: TrustRiskOverrides {
            outside_workspace: action(trust.risks.eager.outside_workspace),
            network: action(trust.risks.eager.network),
            irreversible: action(trust.risks.eager.irreversible),
            filesystem_write: action(trust.risks.eager.filesystem_write),
            process_spawn: action(trust.risks.eager.process_spawn),
            repository_mutation: action(trust.risks.eager.repository_mutation),
        },
    }
}

pub(crate) fn runtime_trust_config(trust: &TrustProjection) -> atman_runtime::trust::TrustConfig {
    let action = |action: Option<TrustPolicyAction>| {
        action.map(|action| match action {
            TrustPolicyAction::Auto => atman_runtime::trust::PolicyAction::Auto,
            TrustPolicyAction::Ask => atman_runtime::trust::PolicyAction::Ask,
            TrustPolicyAction::Deny => atman_runtime::trust::PolicyAction::Deny,
        })
    };
    atman_runtime::trust::TrustConfig {
        mode: match trust.mode {
            TrustMode::Calm => atman_runtime::trust::TrustMode::Calm,
            TrustMode::Steady => atman_runtime::trust::TrustMode::Steady,
            TrustMode::Eager => atman_runtime::trust::TrustMode::Eager,
            TrustMode::Reckless => atman_runtime::trust::TrustMode::Reckless,
        },
        theme: match trust.theme {
            TrustTheme::Default => atman_runtime::trust::Theme::Default,
            TrustTheme::Wuxia => atman_runtime::trust::Theme::Wuxia,
            TrustTheme::Animal => atman_runtime::trust::Theme::Animal,
            TrustTheme::Weather => atman_runtime::trust::Theme::Weather,
            TrustTheme::Drink => atman_runtime::trust::Theme::Drink,
        },
        escalation: match trust.escalation {
            TrustEscalation::Deny => atman_runtime::trust::EscalationPolicy::Deny,
            TrustEscalation::Ask => atman_runtime::trust::EscalationPolicy::Ask,
            TrustEscalation::Allow => atman_runtime::trust::EscalationPolicy::Allow,
        },
        tiers: atman_runtime::trust::TierPolicyConfig {
            eager: atman_runtime::trust::TierPolicyOverrides {
                tier0: action(trust.eager_tiers.tier0),
                tier1: action(trust.eager_tiers.tier1),
                tier2: action(trust.eager_tiers.tier2),
                tier3: action(trust.eager_tiers.tier3),
                tier4: action(trust.eager_tiers.tier4),
            },
        },
        risks: atman_runtime::trust::RiskPolicyConfig {
            eager: atman_runtime::trust::RiskPolicyOverrides {
                outside_workspace: action(trust.eager_risks.outside_workspace),
                network: action(trust.eager_risks.network),
                irreversible: action(trust.eager_risks.irreversible),
                filesystem_write: action(trust.eager_risks.filesystem_write),
                process_spawn: action(trust.eager_risks.process_spawn),
                repository_mutation: action(trust.eager_risks.repository_mutation),
            },
        },
    }
}

fn message_projection(
    message: &atman_runtime::message::Message,
    seq: u64,
    checkpoint_index: Option<usize>,
) -> MessageProjection {
    MessageProjection {
        role: match message.role {
            atman_runtime::message::MessageRole::User => MessageRole::User,
            atman_runtime::message::MessageRole::Assistant => MessageRole::Assistant,
            atman_runtime::message::MessageRole::System => MessageRole::System,
            atman_runtime::message::MessageRole::Tool => MessageRole::Tool,
        },
        origin: match message.origin {
            atman_runtime::message::MessageOrigin::User => MessageOrigin::User,
            atman_runtime::message::MessageOrigin::Watcher => MessageOrigin::Watcher,
            atman_runtime::message::MessageOrigin::Interjection => MessageOrigin::Interjection,
            atman_runtime::message::MessageOrigin::Internal => MessageOrigin::Internal,
        },
        turn_id: TurnId(message.turn_id.0),
        parts: message
            .parts
            .iter()
            .enumerate()
            .map(|(index, part)| message_part(part, message.part_id(seq, checkpoint_index, index)))
            .collect(),
    }
}

fn interjection_projection(
    injection: &atman_runtime::injection::Injection,
) -> InterjectionProjection {
    InterjectionProjection {
        id: injection.id.0,
        turn_id: TurnId(injection.turn_id.0),
        run_id: injection
            .flow_run_id
            .as_ref()
            .map(|run_id| FlowRunId(run_id.0)),
        text: injection.text.clone(),
        level: match injection.level {
            atman_runtime::injection::InjectionLevel::L1Nudge => {
                atman_proto::InterjectionLevel::Nudge
            }
            atman_runtime::injection::InjectionLevel::L2CourseCorrect => {
                atman_proto::InterjectionLevel::CourseCorrect
            }
            atman_runtime::injection::InjectionLevel::L3Redirect => {
                atman_proto::InterjectionLevel::Redirect
            }
            atman_runtime::injection::InjectionLevel::L4HardStop => {
                atman_proto::InterjectionLevel::HardStop
            }
        },
        state: match injection.state {
            atman_runtime::injection::InjectionState::Pending => {
                atman_proto::InterjectionState::Pending
            }
            atman_runtime::injection::InjectionState::Injected => {
                atman_proto::InterjectionState::Injected
            }
            atman_runtime::injection::InjectionState::Cancelled => {
                atman_proto::InterjectionState::Cancelled
            }
        },
        redirect_target: injection.redirect_target.clone(),
        created_at: injection.created_at,
        source: match &injection.source {
            atman_runtime::injection::InjectionSource::User => InterjectionSource::User,
            atman_runtime::injection::InjectionSource::Watcher {
                watcher_id,
                kind,
                handle,
            } => InterjectionSource::Watcher {
                watcher_id: watcher_id.clone(),
                kind: kind.clone(),
                handle: handle.clone(),
            },
        },
    }
}

pub(crate) fn approval_request_projection(
    payload: &atman_runtime::permission_audit::PermissionRequestAudit,
    state: ApprovalState,
) -> Option<ApprovalRequestProjection> {
    Some(ApprovalRequestProjection {
        id: payload.request_id.as_ref()?.0,
        session_id: payload.session_id.clone(),
        requesting_run_id: FlowRunId(payload.requesting_run_id.0),
        parent_run_id: payload.parent_run_id.as_ref().map(|id| FlowRunId(id.0)),
        root_run_id: FlowRunId(payload.root_run_id.0),
        tool_use_id: payload.tool_use_id.clone(),
        tool_name: payload.tool.clone(),
        intent: payload
            .call_intent
            .as_ref()
            .map(|intent| intent.as_str().to_owned()),
        tier: tier_number(payload.tier),
        execution_boundary: payload.execution_boundary.map(|boundary| match boundary {
            atman_runtime::permission::ExecutionBoundary::Sandboxed => {
                ApprovalExecutionBoundary::Sandboxed
            }
            atman_runtime::permission::ExecutionBoundary::Direct => {
                ApprovalExecutionBoundary::Direct
            }
        }),
        provenance: ApprovalProvenanceProjection {
            cwd: payload.provenance.cwd.clone(),
            path: payload.provenance.path.clone(),
            path_origin: payload.provenance.path_origin.clone(),
            workspace_id: payload.provenance.workspace_id.clone(),
            workspace_root: payload.provenance.workspace_root.clone(),
            repository_root: payload.provenance.repository_root.clone(),
            network: payload.provenance.network,
            risks: payload.provenance.risks.clone(),
            targets: payload.provenance.targets.clone(),
        },
        state,
        target: Some(approval_target(&payload.target)),
        group_ids: payload.group_ids.iter().map(|id| id.0).collect(),
        policy: ApprovalPolicyProjection {
            snapshot_id: payload.policy.snapshot_id.clone(),
            rule_id: payload.policy.rule_id.clone(),
        },
        escalation_path: payload
            .escalation_path
            .iter()
            .map(|hop| ApprovalEscalationHopProjection {
                target: approval_target(&hop.target),
                actor: hop.actor.as_ref().map(approval_actor),
                action: hop.action.clone(),
                reason: hop.reason.clone(),
                at: hop.at,
            })
            .collect(),
        decision_id: payload.decision_id.clone(),
        actor: payload.actor.as_ref().map(approval_actor),
        scope: payload.scope.as_ref().map(approval_scope),
        reason: payload.reason.clone(),
        at: payload.at,
        revision: payload.revision,
    })
}

pub(crate) fn approval_group_projection(
    payload: &atman_runtime::permission_audit::PermissionGroupAudit,
    resolved: bool,
) -> ApprovalGroupProjection {
    ApprovalGroupProjection {
        id: payload.group_id.0,
        owner: match &payload.owner {
            atman_runtime::permission_audit::PermissionGroupAuditOwner::Flow { run_id } => {
                ApprovalGroupOwnerProjection::Flow {
                    run_id: FlowRunId(run_id.0),
                }
            }
            atman_runtime::permission_audit::PermissionGroupAuditOwner::User { session_id } => {
                ApprovalGroupOwnerProjection::User {
                    session_id: session_id.clone(),
                }
            }
            atman_runtime::permission_audit::PermissionGroupAuditOwner::System => {
                ApprovalGroupOwnerProjection::System
            }
        },
        label: payload.label.clone(),
        request_ids: payload.request_ids.iter().map(|id| id.0).collect(),
        revision: payload.revision,
        resolved,
        at: payload.at,
    }
}

fn approval_target(
    target: &atman_runtime::permission_audit::PermissionAuditTarget,
) -> ApprovalTarget {
    match target {
        atman_runtime::permission_audit::PermissionAuditTarget::Flow { run_id } => {
            ApprovalTarget::Flow {
                run_id: FlowRunId(run_id.0),
            }
        }
        atman_runtime::permission_audit::PermissionAuditTarget::User => ApprovalTarget::User,
    }
}

fn approval_actor(
    actor: &atman_runtime::permission_audit::PermissionProjectionActor,
) -> ApprovalActorProjection {
    match actor {
        atman_runtime::permission_audit::PermissionProjectionActor::Policy {
            policy_version,
            rule_id,
        } => ApprovalActorProjection::Policy {
            policy_version: policy_version.clone(),
            rule_id: rule_id.clone(),
        },
        atman_runtime::permission_audit::PermissionProjectionActor::Flow { session_id, run_id } => {
            ApprovalActorProjection::Flow {
                session_id: session_id.clone(),
                run_id: FlowRunId(run_id.0),
            }
        }
        atman_runtime::permission_audit::PermissionProjectionActor::User {
            session_id,
            principal_id,
        } => ApprovalActorProjection::User {
            session_id: session_id.clone(),
            principal_id: principal_id.clone(),
        },
        atman_runtime::permission_audit::PermissionProjectionActor::System { component } => {
            ApprovalActorProjection::System {
                component: component.clone(),
            }
        }
        atman_runtime::permission_audit::PermissionProjectionActor::UnknownLegacy { label } => {
            ApprovalActorProjection::UnknownLegacy {
                label: label.clone(),
            }
        }
    }
}

fn approval_scope(
    scope: &atman_runtime::permission_audit::PermissionAuditScope,
) -> ApprovalScopeProjection {
    match scope {
        atman_runtime::permission_audit::PermissionAuditScope::CurrentCall => {
            ApprovalScopeProjection::CurrentCall
        }
        atman_runtime::permission_audit::PermissionAuditScope::ChildRunSameTool {
            run_id,
            tool_name,
        } => ApprovalScopeProjection::ChildRunSameTool {
            run_id: FlowRunId(run_id.0),
            tool_name: tool_name.clone(),
        },
        atman_runtime::permission_audit::PermissionAuditScope::ChildRunSamePathRule {
            run_id,
            tool_name,
            workspace_relative_path,
        } => ApprovalScopeProjection::ChildRunSamePathRule {
            run_id: FlowRunId(run_id.0),
            tool_name: tool_name.clone(),
            workspace_relative_path: workspace_relative_path.clone(),
        },
    }
}

fn pending_form_projection(
    pending: &atman_runtime::form::PendingForm,
) -> atman_proto::PendingFormProjection {
    atman_proto::PendingFormProjection {
        id: pending.form_id.clone(),
        run_id: FlowRunId(pending.run_id.0),
        tool_use_id: pending.tool_use_id.clone(),
        emitted_at: pending.emitted_at,
        questions: pending
            .form
            .questions
            .iter()
            .map(|question| {
                let (kind, prompt, options, min, max, placeholder, multiline) = match &question.kind
                {
                    atman_runtime::form::FormKind::Confirm { prompt } => (
                        atman_proto::FormQuestionKind::Confirm,
                        prompt.clone(),
                        Vec::new(),
                        None,
                        None,
                        None,
                        false,
                    ),
                    atman_runtime::form::FormKind::SingleSelect { prompt, options } => (
                        atman_proto::FormQuestionKind::SingleSelect,
                        prompt.clone(),
                        options.clone(),
                        None,
                        None,
                        None,
                        false,
                    ),
                    atman_runtime::form::FormKind::MultiSelect {
                        prompt,
                        options,
                        min,
                        max,
                    } => (
                        atman_proto::FormQuestionKind::MultiSelect,
                        prompt.clone(),
                        options.clone(),
                        *min,
                        *max,
                        None,
                        false,
                    ),
                    atman_runtime::form::FormKind::Text {
                        prompt,
                        placeholder,
                        multiline,
                    } => (
                        atman_proto::FormQuestionKind::Text,
                        prompt.clone(),
                        Vec::new(),
                        None,
                        None,
                        placeholder.clone(),
                        *multiline,
                    ),
                };
                atman_proto::FormQuestionProjection {
                    id: question.id.clone(),
                    kind,
                    prompt,
                    options,
                    min,
                    max,
                    placeholder,
                    multiline,
                }
            })
            .collect(),
    }
}

fn compact_review_projection(
    pending: &atman_runtime::session::PendingCompactReview,
) -> atman_proto::CompactReviewProjection {
    atman_proto::CompactReviewProjection {
        id: pending.review_id.clone(),
        context_id: pending
            .context_id
            .as_ref()
            .map(|id| atman_proto::ContextId(id.0)),
        summary: pending.summary.clone(),
        slice_preview: pending.slice_preview.clone(),
        slice_count: pending.slice_count,
        range_start: pending.range_start,
        range_end: pending.range_end,
        tokens_before: pending.tokens_before,
        emitted_at: pending.emitted_at,
    }
}

fn message_part(
    part: &atman_runtime::message::MessagePart,
    id: Option<atman_runtime::message::MessagePartId>,
) -> MessagePart {
    match part {
        atman_runtime::message::MessagePart::ContextRecord(record) => MessagePart::ContextRecord {
            key: record.key().into(),
            content: match record.body() {
                atman_runtime::context_plan::ContextRecordBody::Text { text } => text.clone(),
                atman_runtime::context_plan::ContextRecordBody::CapabilityDelta { delta } => {
                    delta.to_string()
                }
                atman_runtime::context_plan::ContextRecordBody::Tombstone => String::new(),
            },
            digest: record.digest().as_str().into(),
            revision: record.revision(),
        },
        atman_runtime::message::MessagePart::CompactSummary {
            summary,
            seq_start,
            seq_end,
            count,
        } => MessagePart::CompactSummary {
            summary: summary.clone(),
            seq_start: *seq_start,
            seq_end: *seq_end,
            count: *count,
        },
        atman_runtime::message::MessagePart::Text { text } => {
            MessagePart::Text { text: text.clone() }
        }
        atman_runtime::message::MessagePart::Thinking { thinking, .. } => MessagePart::Thinking {
            thinking: thinking.clone(),
        },
        atman_runtime::message::MessagePart::Image { source, .. } => {
            let (artifact_id, name) = match &source.data {
                ImageData::Base64 { .. } => (None, None),
                ImageData::Path { path } => (
                    None,
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                ),
                ImageData::Artifact { id, name, .. } => (Some(id.clone()), name.clone()),
            };
            MessagePart::Image {
                id: id.map(|id| atman_proto::MessagePartId(id.0)),
                media_type: source.media_type.clone(),
                artifact_id,
                name,
                detail: match source.detail {
                    atman_runtime::provider::ImageDetail::Low => ImageDetail::Low,
                    atman_runtime::provider::ImageDetail::High => ImageDetail::High,
                    atman_runtime::provider::ImageDetail::Original => ImageDetail::Original,
                    atman_runtime::provider::ImageDetail::Auto => ImageDetail::Auto,
                },
            }
        }
        atman_runtime::message::MessagePart::ToolUse {
            id,
            name,
            input,
            intent,
        } => MessagePart::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            intent: intent.as_ref().map(|intent| intent.as_str().into()),
        },
        atman_runtime::message::MessagePart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => MessagePart::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
    }
}

fn workflow_projection(workflow: &RuntimeWorkflowProjection) -> WorkflowProjection {
    WorkflowProjection {
        turn_id: TurnId(workflow.graph().turn_id.0),
        roots: workflow.graph().root.iter().map(workflow_node).collect(),
    }
}

fn workflow_node(node: &atman_runtime::workflow::WorkflowNode) -> WorkflowNodeProjection {
    WorkflowNodeProjection {
        id: node.id.clone(),
        kind: match &node.kind {
            atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, flow_name } => {
                WorkflowNodeKind::Flow {
                    run_id: parse_run_id(run_id),
                    flow_name: flow_name.clone(),
                }
            }
            atman_runtime::workflow::WorkflowNodeKind::Stmt { node_kind } => {
                WorkflowNodeKind::Statement {
                    kind: workflow_statement_kind(node_kind),
                }
            }
            atman_runtime::workflow::WorkflowNodeKind::ToolCall {
                tool_use_id,
                tool,
                args_preview,
                call_intent,
                result_preview,
            } => WorkflowNodeKind::ToolCall {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool.clone(),
                args_preview: args_preview.clone(),
                intent: call_intent.as_ref().map(|intent| intent.as_str().into()),
                result_preview: result_preview.clone(),
            },
            atman_runtime::workflow::WorkflowNodeKind::Subflow { run_id, flow_name } => {
                WorkflowNodeKind::Subflow {
                    run_id: parse_run_id(run_id),
                    flow_name: flow_name.clone(),
                }
            }
            atman_runtime::workflow::WorkflowNodeKind::FanoutBranch { branch_index } => {
                WorkflowNodeKind::FanoutBranch {
                    branch_index: *branch_index,
                }
            }
        },
        label: node.label.clone(),
        state: match node.status {
            atman_runtime::workflow::NodeStatus::Pending => WorkflowNodeState::Pending,
            atman_runtime::workflow::NodeStatus::Running => WorkflowNodeState::Running,
            atman_runtime::workflow::NodeStatus::Ok => WorkflowNodeState::Succeeded,
            atman_runtime::workflow::NodeStatus::Err => WorkflowNodeState::Failed,
            atman_runtime::workflow::NodeStatus::Cancelled => WorkflowNodeState::Cancelled,
        },
        started_at: node.started_at,
        finished_at: node.ended_at,
        output_preview: node.output_preview.clone(),
        children: node.children.iter().map(workflow_node).collect(),
        parallel: matches!(
            node.parallelism,
            atman_runtime::workflow::Parallelism::Parallel
        ),
        approval: node.approval.as_ref().map(|approval| match approval {
            atman_runtime::workflow::ApprovalState::Pending { level, preview } => {
                atman_proto::ApprovalProjection::Pending {
                    level: level.clone(),
                    preview: preview.clone(),
                }
            }
            atman_runtime::workflow::ApprovalState::Approved => {
                atman_proto::ApprovalProjection::Approved
            }
            atman_runtime::workflow::ApprovalState::Denied { reason } => {
                atman_proto::ApprovalProjection::Denied {
                    reason: reason.clone(),
                }
            }
        }),
        llm_usage: node.llm_stats.as_ref().map(|usage| LlmUsageProjection {
            model: usage.model.clone(),
            provider: usage.provider.clone(),
            call_purpose: llm_call_purpose(usage.context_call_purpose),
            call_scope: llm_call_scope(usage.context_call_scope),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read,
            cache_write_tokens: usage.cache_write,
            wallclock_ms: usage.wallclock_ms,
            ttft_ms: usage.ttft_ms,
            tokens_per_second: usage.tokens_per_second,
        }),
    }
}

fn workflow_statement_kind(kind: &atman_runtime::nodegraph::NodeKind) -> WorkflowStatementKind {
    match kind {
        atman_runtime::nodegraph::NodeKind::Llm { model } => WorkflowStatementKind::Llm {
            model: model.clone(),
        },
        atman_runtime::nodegraph::NodeKind::ToolCall { path } => {
            WorkflowStatementKind::ToolCall { path: path.clone() }
        }
        atman_runtime::nodegraph::NodeKind::Fanout { collect } => WorkflowStatementKind::Fanout {
            collect: match collect {
                atman_runtime::nodegraph::FanoutMode::All => WorkflowFanoutMode::All,
                atman_runtime::nodegraph::FanoutMode::First => WorkflowFanoutMode::First,
            },
        },
        atman_runtime::nodegraph::NodeKind::UserConfirm => WorkflowStatementKind::UserConfirm,
        atman_runtime::nodegraph::NodeKind::Subflow { name } => {
            WorkflowStatementKind::Subflow { name: name.clone() }
        }
        atman_runtime::nodegraph::NodeKind::Message { role } => {
            WorkflowStatementKind::Message { role: role.clone() }
        }
        atman_runtime::nodegraph::NodeKind::FixUntilTest => WorkflowStatementKind::FixUntilTest,
        atman_runtime::nodegraph::NodeKind::When { condition_preview } => {
            WorkflowStatementKind::When {
                condition_preview: condition_preview.clone(),
            }
        }
        atman_runtime::nodegraph::NodeKind::Loop => WorkflowStatementKind::Loop,
        atman_runtime::nodegraph::NodeKind::Return => WorkflowStatementKind::Return,
    }
}

fn todo_projection(todo: atman_runtime::memory::todo::Todo) -> TodoProjection {
    TodoProjection {
        id: todo.id.0.to_string(),
        where_: todo.where_,
        why: todo.why,
        how: todo.how,
        expected_result: todo.expected_result,
        state: match todo.status {
            atman_runtime::memory::todo::TodoStatus::Pending => TodoState::Pending,
            atman_runtime::memory::todo::TodoStatus::InProgress => TodoState::InProgress,
            atman_runtime::memory::todo::TodoStatus::Done => TodoState::Done,
            atman_runtime::memory::todo::TodoStatus::Cancelled => TodoState::Cancelled,
        },
    }
}

fn plan_projection(plan: atman_runtime::memory::plan::Plan) -> PlanProjection {
    PlanProjection {
        id: plan.id,
        title: plan.title,
        steps: plan
            .steps
            .into_iter()
            .map(|step| PlanStepProjection {
                index: step.index,
                text: step.text,
                done: step.done,
                done_at: step.done_at,
            })
            .collect(),
        created_at: plan.created_at,
        updated_at: plan.updated_at,
    }
}

fn context_projection(context: &atman_runtime::ContextSnapshot) -> ContextProjection {
    ContextProjection {
        model: context.model.clone(),
        provider: context.provider.clone(),
        input_tokens: context.tokens_in,
        output_tokens: context.tokens_out,
        window_tokens: context.window_tokens,
        window_budget: context.window_budget,
        cost_usd: context.cost_usd,
        cache_read_tokens: context.cache_read,
        cache_write_tokens: context.cache_write,
        last_ttft_ms: context.last_ttft_ms,
        last_tokens_per_second: context.last_tokens_per_sec,
        memory_recent_count: context.memory_recent_count,
        usage_buckets: context
            .usage_buckets
            .iter()
            .map(|bucket| ContextUsageBucketProjection {
                provider: bucket.provider.clone(),
                model: bucket.model.clone(),
                call_purpose: llm_call_purpose(bucket.call_purpose),
                call_scope: llm_call_scope(bucket.call_scope),
                calls: bucket.calls,
                input_tokens: bucket.tokens_in,
                output_tokens: bucket.tokens_out,
                cache_read_tokens: bucket.cache_read,
                cache_write_tokens: bucket.cache_write,
            })
            .collect(),
        mcp_servers: context
            .mcp_servers
            .iter()
            .map(mcp_server_projection)
            .collect(),
    }
}

fn llm_call_purpose(purpose: atman_runtime::context_plan::ContextCallPurpose) -> LlmCallPurpose {
    match purpose {
        atman_runtime::context_plan::ContextCallPurpose::General => LlmCallPurpose::General,
        atman_runtime::context_plan::ContextCallPurpose::Classification => {
            LlmCallPurpose::Classification
        }
        atman_runtime::context_plan::ContextCallPurpose::Extraction => LlmCallPurpose::Extraction,
        atman_runtime::context_plan::ContextCallPurpose::BranchGeneration => {
            LlmCallPurpose::BranchGeneration
        }
        atman_runtime::context_plan::ContextCallPurpose::Compaction => LlmCallPurpose::Compaction,
        atman_runtime::context_plan::ContextCallPurpose::InterjectionClassification => {
            LlmCallPurpose::InterjectionClassification
        }
    }
}

fn llm_call_scope(scope: atman_runtime::context_plan::ContextCallScope) -> LlmCallScope {
    match scope {
        atman_runtime::context_plan::ContextCallScope::Root => LlmCallScope::Root,
        atman_runtime::context_plan::ContextCallScope::Child => LlmCallScope::Child,
        atman_runtime::context_plan::ContextCallScope::Detached => LlmCallScope::Detached,
    }
}

fn merged_usage(events: &UsageProjection, watch: &UsageProjection) -> UsageProjection {
    UsageProjection {
        input_tokens: events.input_tokens.max(watch.input_tokens),
        output_tokens: events.output_tokens.max(watch.output_tokens),
        cache_read_tokens: events.cache_read_tokens.max(watch.cache_read_tokens),
        cache_write_tokens: events.cache_write_tokens.max(watch.cache_write_tokens),
        cost_usd: events.cost_usd.max(watch.cost_usd),
        llm_calls: events.llm_calls.max(watch.llm_calls),
    }
}

fn mcp_server_projection(status: &atman_runtime::mcp::McpServerStatus) -> McpServerProjection {
    McpServerProjection {
        name: status.name.clone(),
        transport: match status.transport {
            atman_runtime::mcp::TransportKind::Stdio => McpTransportProjection::Stdio,
            atman_runtime::mcp::TransportKind::Http => McpTransportProjection::Http,
            atman_runtime::mcp::TransportKind::Sse => McpTransportProjection::Sse,
        },
        state: match &status.state {
            atman_runtime::mcp::McpServerState::Disabled => McpServerStateProjection::Disabled,
            atman_runtime::mcp::McpServerState::Pending => McpServerStateProjection::Pending,
            atman_runtime::mcp::McpServerState::Connecting => McpServerStateProjection::Connecting,
            atman_runtime::mcp::McpServerState::Connected { tools, .. } => {
                McpServerStateProjection::Connected {
                    tools: tools
                        .iter()
                        .map(|tool| McpToolProjection {
                            name: tool.name.clone(),
                            description: tool.description.clone(),
                        })
                        .collect(),
                }
            }
            atman_runtime::mcp::McpServerState::Error { message } => {
                McpServerStateProjection::Error {
                    message: message.clone(),
                }
            }
            atman_runtime::mcp::McpServerState::Disconnected { message } => {
                McpServerStateProjection::Disconnected {
                    message: message.clone(),
                }
            }
            atman_runtime::mcp::McpServerState::Timeout { message } => {
                McpServerStateProjection::Timeout {
                    message: message.clone(),
                }
            }
        },
    }
}

fn event_run_id(event: &Event) -> Option<&atman_runtime::event::FlowRunId> {
    match event {
        Event::FlowStart { run_id, .. }
        | Event::FlowEnd { run_id, .. }
        | Event::FlowGraph { run_id, .. }
        | Event::FlowNodeStart { run_id, .. }
        | Event::FlowNodeEnd { run_id, .. }
        | Event::ToolNode { run_id, .. }
        | Event::ToolPendingApproval { run_id, .. }
        | Event::ToolApproved { run_id, .. }
        | Event::ToolDenied { run_id, .. } => Some(run_id),
        Event::LlmCall { run_id, .. }
        | Event::ToolResultMsg {
            flow_run_id: run_id,
            ..
        } => run_id.as_ref(),
        Event::PermissionRequestCreated { payload }
        | Event::PermissionRequestTargeted { payload }
        | Event::PermissionRequestDeferred { payload }
        | Event::PermissionRequestApproved { payload }
        | Event::PermissionRequestDenied { payload }
        | Event::PermissionRequestCancelled { payload }
        | Event::UnrestrictedExecution { payload } => Some(&payload.requesting_run_id),
        _ => None,
    }
}

fn parse_run_id(run_id: &str) -> FlowRunId {
    FlowRunId(uuid::Uuid::parse_str(run_id).unwrap_or_else(|_| uuid::Uuid::nil()))
}

fn orphan_turn_id() -> atman_runtime::event::TurnId {
    atman_runtime::event::TurnId(uuid::Uuid::nil())
}

fn tier_number(tier: atman_runtime::tool::Tier) -> u8 {
    match tier {
        atman_runtime::tool::Tier::Zero => 0,
        atman_runtime::tool::Tier::One => 1,
        atman_runtime::tool::Tier::Two => 2,
        atman_runtime::tool::Tier::Three => 3,
        atman_runtime::tool::Tier::Four => 4,
    }
}

pub(crate) fn workspace_state(state: &str) -> ResourceState {
    match state {
        "starting" | "allocating" => ResourceState::Starting,
        "active" | "running" => ResourceState::Running,
        "dirty" => ResourceState::Dirty,
        "retained" => ResourceState::Retained,
        "releasing" | "terminating" => ResourceState::Terminating,
        "released" => ResourceState::Released,
        "failed" | "error" => ResourceState::Failed,
        "lost" => ResourceState::Lost,
        "orphaned" => ResourceState::Orphaned,
        _ => ResourceState::Running,
    }
}

fn resource_is_terminal(state: &str) -> bool {
    matches!(state, "released" | "failed" | "error" | "lost" | "orphaned")
}

pub(crate) fn task_resource_id(task_id: &atman_runtime::TaskId) -> ResourceId {
    ResourceId::task(task_id.0)
}

fn task_resource_state(status: atman_runtime::TaskStatus) -> ResourceState {
    match status {
        atman_runtime::TaskStatus::Running => ResourceState::Running,
        atman_runtime::TaskStatus::Killing => ResourceState::Terminating,
        atman_runtime::TaskStatus::Ok | atman_runtime::TaskStatus::Killed => ResourceState::Exited,
        atman_runtime::TaskStatus::Err => ResourceState::Failed,
    }
}

fn task_resource_is_terminal(status: atman_runtime::TaskStatus) -> bool {
    matches!(
        status,
        atman_runtime::TaskStatus::Ok
            | atman_runtime::TaskStatus::Err
            | atman_runtime::TaskStatus::Killed
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::{FlowRunId as RuntimeRunId, TurnId as RuntimeTurnId};
    use atman_runtime::message::Message;

    fn envelope(seq: u64, ts: chrono::DateTime<chrono::Utc>, event: Event) -> EventEnvelope {
        EventEnvelope {
            seq,
            ts,
            context_id: None,
            event,
        }
    }

    #[test]
    fn terminal_signal_redaction_preserves_non_utf8_bytes() {
        let secret = "sk-abcdefghijklmnop1234567890";
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(format!("token={secret}").as_bytes());
        bytes.push(0xfe);
        let event = atman_proto::ProjectionEventEnvelope {
            schema_version: atman_proto::PROJECTION_EVENT_SCHEMA_VERSION,
            daemon_generation: atman_proto::DaemonGeneration("test".into()),
            session_id: SessionId(uuid::Uuid::now_v7()),
            cursor: EventCursor(1),
            ts: chrono::Utc::now(),
            event: atman_proto::ServerEvent::Signal {
                signal: atman_proto::SessionSignal::TerminalBytes {
                    resource_id: ResourceId("task:test".into()),
                    bytes,
                },
            },
        };

        let redacted =
            redacted_projection_event(&event, Some(&atman_runtime::redact::Redactor::builtin()))
                .unwrap();
        let atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::TerminalBytes { bytes, .. },
        } = redacted.event
        else {
            panic!("expected terminal signal");
        };
        assert_eq!(bytes.first(), Some(&0xff));
        assert_eq!(bytes.last(), Some(&0xfe));
        assert!(!String::from_utf8_lossy(&bytes).contains(secret));
        assert!(String::from_utf8_lossy(&bytes).contains("<REDACTED:openai_api_key>"));
    }

    #[test]
    fn mcp_projection_preserves_transport_tools_and_failure_details() {
        let connected = mcp_server_projection(&atman_runtime::mcp::McpServerStatus {
            name: "catalog".into(),
            transport: atman_runtime::mcp::TransportKind::Http,
            state: atman_runtime::mcp::McpServerState::Connected {
                tool_count: 1,
                tools: vec![atman_runtime::mcp::McpToolInfo {
                    name: "search".into(),
                    description: Some("Search the catalog".into()),
                }],
            },
        });
        assert_eq!(connected.transport, McpTransportProjection::Http);
        assert_eq!(
            connected.state,
            McpServerStateProjection::Connected {
                tools: vec![McpToolProjection {
                    name: "search".into(),
                    description: Some("Search the catalog".into()),
                }],
            }
        );

        let failed = mcp_server_projection(&atman_runtime::mcp::McpServerStatus {
            name: "catalog".into(),
            transport: atman_runtime::mcp::TransportKind::Sse,
            state: atman_runtime::mcp::McpServerState::Error {
                message: "authentication failed".into(),
            },
        });
        assert_eq!(failed.transport, McpTransportProjection::Sse);
        assert_eq!(
            failed.state,
            McpServerStateProjection::Error {
                message: "authentication failed".into(),
            }
        );
    }

    #[test]
    fn interleaved_run_turn_anchors_survive_replay_and_snapshot_resume() {
        let sid = SessionId(uuid::Uuid::now_v7());
        let first_turn = RuntimeTurnId::now();
        let second_turn = RuntimeTurnId::now();
        let first_run = RuntimeRunId::now();
        let second_run = RuntimeRunId::now();
        let child_run = RuntimeRunId::now();
        let at = chrono::Utc::now();
        let events: Vec<_> = [
            Event::TurnStart {
                turn_id: first_turn.clone(),
            },
            Event::TurnStart {
                turn_id: second_turn.clone(),
            },
            Event::FlowStart {
                run_id: first_run.clone(),
                turn_id: Some(first_turn.clone()),
                flow_name: "first".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
            },
            Event::FlowStart {
                run_id: second_run.clone(),
                turn_id: Some(second_turn.clone()),
                flow_name: "second".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
            },
            Event::FlowStart {
                run_id: child_run.clone(),
                turn_id: None,
                flow_name: "legacy-child".into(),
                parent_run_id: Some(first_run.clone()),
                parent_node_id: None,
                spawned: true,
            },
            Event::FlowEnd {
                run_id: first_run.clone(),
                flow_name: "first".into(),
                status: FlowStatus::Ok,
            },
            Event::TurnEnd {
                turn_id: first_turn.clone(),
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(i, event)| envelope(i as u64 + 1, at, event))
        .collect();
        let mut live = SessionProjector::new(sid.clone(), None);
        for event in &events[..4] {
            live.apply_envelope(event);
        }
        let mut resumed: SessionProjector =
            serde_json::from_value(serde_json::to_value(&live).unwrap()).unwrap();
        for event in &events[4..] {
            live.apply_envelope(event);
            resumed.apply_envelope(event);
        }
        let replay = SessionProjector::from_events(sid.clone(), None, &events);
        assert_eq!(live.projection(), replay.projection());
        assert_eq!(live.projection(), resumed.projection());
        for (run_id, turn_id) in [
            (&first_run, &first_turn),
            (&second_run, &second_turn),
            (&child_run, &first_turn),
        ] {
            let run = live
                .projection()
                .runs
                .iter()
                .find(|run| run.id.0 == run_id.0)
                .unwrap();
            assert_eq!(run.turn_id, Some(TurnId(turn_id.0)));
            assert_eq!(live.run_turns.get(run_id), Some(turn_id));
        }
        assert_eq!(live.projection().workflows.len(), 2);
        for (turn_id, expected_roots) in [
            (
                &first_turn,
                vec![first_run.to_string(), child_run.to_string()],
            ),
            (&second_turn, vec![second_run.to_string()]),
        ] {
            let workflow = live
                .projection()
                .workflows
                .iter()
                .find(|workflow| workflow.turn_id.0 == turn_id.0)
                .unwrap();
            assert_eq!(
                workflow
                    .roots
                    .iter()
                    .map(|node| node.id.clone())
                    .collect::<Vec<_>>(),
                expected_roots
            );
        }

        let mut registered = SessionProjector::new(sid, None);
        registered.register_run(
            FlowRunId(first_run.0),
            first_turn.clone(),
            "first".into(),
            at,
        );
        assert_eq!(
            registered.projection().runs[0].turn_id,
            Some(TurnId(first_turn.0))
        );
        registered.apply_envelope(&events[1]);
        let mut legacy_start = events[2].clone();
        if let Event::FlowStart { turn_id, .. } = &mut legacy_start.event {
            *turn_id = None;
        }
        registered.apply_envelope(&legacy_start);
        assert_eq!(registered.run_turns.get(&first_run), Some(&first_turn));
        let mut legacy_run = serde_json::to_value(&registered.projection().runs[0]).unwrap();
        legacy_run.as_object_mut().unwrap().remove("turn_id");
        assert!(
            serde_json::from_value::<RunProjection>(legacy_run)
                .unwrap()
                .turn_id
                .is_none()
        );
    }

    #[test]
    fn replay_builds_typed_transcript_workflow_and_usage_without_image_bytes() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let turn_id = RuntimeTurnId::now();
        let run_id = RuntimeRunId::now();
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(3);
        let mut image_message = Message::user_text(turn_id.clone(), "inspect");
        image_message
            .parts
            .push(atman_runtime::message::MessagePart::Image {
                id: None,
                source: atman_runtime::message::ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Base64 {
                        data: "secret-binary".into(),
                    },
                    detail: atman_runtime::provider::ImageDetail::Auto,
                },
            });
        let events = vec![
            envelope(
                1,
                started_at,
                Event::TurnStart {
                    turn_id: turn_id.clone(),
                },
            ),
            envelope(
                2,
                started_at,
                Event::FlowStart {
                    turn_id: None,
                    run_id: run_id.clone(),
                    flow_name: "agent".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ),
            envelope(
                3,
                started_at + chrono::Duration::seconds(1),
                Event::UserMsg {
                    turn_id,
                    flow_run_id: Some(run_id.clone()),
                    message: image_message,
                },
            ),
            envelope(
                4,
                started_at + chrono::Duration::seconds(2),
                Event::LlmCall {
                    model: "reasoning-model".into(),
                    provider: "openai-compatible".into(),
                    context_plan_id: None,
                    managed_context: None,
                    context_epoch: None,
                    context_tokens: None,
                    usage_source: None,
                    context_call_purpose: None,
                    context_call_identity: None,
                    context_cache: None,
                    assistant_tool_batch_width: None,
                    usage: atman_runtime::provider::TokenUsage {
                        input: 10,
                        cached_input: 20,
                        output: 5,
                        cache_write: 2,
                        reasoning_tokens: 1,
                    },
                    wallclock_ms: 100,
                    ttft_ms: Some(10),
                    tokens_per_second: Some(50.0),
                    status: Default::default(),
                    run_id: Some(run_id.clone()),
                    node_id: None,
                },
            ),
            envelope(
                5,
                started_at + chrono::Duration::seconds(3),
                Event::FlowEnd {
                    run_id,
                    flow_name: "agent".into(),
                    status: FlowStatus::Ok,
                },
            ),
        ];

        let projector = SessionProjector::from_events(session_id, None, &events);
        let projection = projector.projection();
        assert_eq!(projection.lifecycle, SessionLifecycle::Idle);
        assert_eq!(projection.runs[0].started_at, started_at);
        assert_eq!(projection.runs[0].state, RunLifecycle::Succeeded);
        assert_eq!(
            projection.workflows[0].roots[0].started_at,
            Some(started_at)
        );
        assert_eq!(projection.usage.input_tokens, 32);
        assert_eq!(projection.usage.cache_read_tokens, 20);
        let encoded = serde_json::to_string(&projection.transcript).unwrap();
        assert!(!encoded.contains("secret-binary"));
    }

    #[test]
    fn attachment_updates_converge_across_scopes_checkpoints_and_snapshot_resume() {
        use atman_runtime::event::{ContextId, EventSink};
        use atman_runtime::message::{
            AttachmentPatch, AttachmentTarget, ImageSource, MessagePartId,
        };

        for typed in [false, true] {
            for checkpoint in [false, true] {
                for stored_id in [None, Some(MessagePartId(uuid::Uuid::now_v7()))] {
                    let sink = EventSink::new();
                    let root_run = RuntimeRunId::now();
                    let child_run = RuntimeRunId::now();
                    let inline_run = RuntimeRunId::now();
                    for (run_id, parent_run_id, spawned) in [
                        (root_run.clone(), None, false),
                        (child_run.clone(), Some(root_run), true),
                        (inline_run.clone(), Some(child_run.clone()), false),
                    ] {
                        sink.emit(Event::FlowStart {
                            turn_id: None,
                            run_id,
                            flow_name: "agent".into(),
                            parent_run_id,
                            parent_node_id: None,
                            spawned,
                        });
                    }
                    let root = if typed {
                        sink.clone().with_context(ContextId::now())
                    } else {
                        sink.clone()
                    };
                    let child = if typed {
                        sink.clone().with_context(ContextId::now())
                    } else {
                        sink.clone()
                    };
                    if typed {
                        root.emit(Event::ContextCreated {
                            base: None,
                            inheritance: atman_runtime::event::ContextInheritance::Full,
                        });
                        child.emit(Event::ContextCreated {
                            base: None,
                            inheritance: atman_runtime::event::ContextInheritance::Full,
                        });
                    }
                    let mut image = Message::user_text(RuntimeTurnId::now(), "caption");
                    image
                        .parts
                        .push(atman_runtime::message::MessagePart::Image {
                            id: stored_id,
                            source: ImageSource {
                                media_type: "image/png".into(),
                                data: ImageData::Base64 {
                                    data: "AA==".into(),
                                },
                                detail: Default::default(),
                            },
                        });
                    let message = |owner| Event::UserMsg {
                        turn_id: image.turn_id.clone(),
                        flow_run_id: owner,
                        message: image.clone(),
                    };
                    let root_seq = root.emit_returning_seq(message(None));
                    let mut child_seq = child.emit_returning_seq(message(Some(child_run.clone())));
                    if checkpoint {
                        child_seq = child.emit_returning_seq(Event::Checkpoint {
                            session_id: "session".into(),
                            flow_run_id: Some(inline_run.clone()),
                            messages: vec![image.clone(), image.clone()],
                            window_tokens: 0,
                        });
                    }
                    let session_id = SessionId(uuid::Uuid::now_v7());
                    let meta = Some(atman_runtime::session_meta::SessionMeta::default());
                    let mut projector = SessionProjector::from_events(
                        session_id.clone(),
                        meta.clone(),
                        &sink.snapshot_envelopes(),
                    );
                    let mut restored: SessionProjector =
                        serde_json::from_value(serde_json::to_value(&projector).unwrap()).unwrap();
                    let apply = |projector: &mut SessionProjector,
                                 restored: &mut SessionProjector,
                                 event| {
                        child.emit(event);
                        let events = sink.snapshot_envelopes();
                        let envelope = events.last().unwrap();
                        let delta = projector.apply_envelope(envelope);
                        assert_eq!(delta, restored.apply_envelope(envelope));
                        assert_eq!(projector.projection(), restored.projection());
                        assert_eq!(
                            projector.projection(),
                            SessionProjector::from_events(
                                session_id.clone(),
                                meta.clone(),
                                &events
                            )
                            .projection()
                        );
                        delta
                    };
                    let patch = |target, reason: &str| Event::AttachmentDegraded {
                        turn_id: None,
                        flow_run_id: Some(inline_run.clone()),
                        patch: AttachmentPatch {
                            target,
                            file_basename: "image.png".into(),
                            reason: reason.into(),
                        },
                    };
                    // Neither another owner's message nor a checkpoint event is a legacy message address.
                    let wrong_seq = if checkpoint { child_seq } else { root_seq };
                    assert!(
                        apply(
                            &mut projector,
                            &mut restored,
                            patch(
                                AttachmentTarget::Legacy {
                                    message_seq: wrong_seq,
                                    part_index: 1
                                },
                                "wrong address"
                            )
                        )
                        .is_none()
                    );
                    let id = image
                        .part_id(child_seq, checkpoint.then_some(0), 1)
                        .unwrap();
                    let target = if !checkpoint && stored_id.is_none() {
                        AttachmentTarget::Legacy {
                            message_seq: child_seq,
                            part_index: 1,
                        }
                    } else {
                        AttachmentTarget::Part { part_id: id }
                    };
                    let delta =
                        apply(&mut projector, &mut restored, patch(target, "unreadable")).unwrap();
                    assert!(matches!(
                        delta.changes.as_slice(),
                        [ProjectionChange::TranscriptReplace { .. }]
                    ));
                    assert!(
                        apply(&mut projector, &mut restored, patch(target, "duplicate")).is_none()
                    );
                    let messages = projector
                        .projection()
                        .transcript
                        .iter()
                        .filter_map(|item| match item {
                            TranscriptItem::Message { message, .. } => Some(message),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert!(matches!(messages[0].parts[1], MessagePart::Image { .. }));
                    assert!(
                        matches!(&messages[1].parts[1], MessagePart::Text { text } if text.ends_with("image.png — unreadable]"))
                    );
                    if checkpoint {
                        assert_eq!(
                            matches!(messages[2].parts[1], MessagePart::Text { .. }),
                            stored_id.is_some()
                        );
                    }
                    for message in &messages {
                        assert_eq!(
                            message.parts[0],
                            MessagePart::Text {
                                text: "caption".into()
                            }
                        );
                    }
                    apply(&mut projector, &mut restored, message(Some(child_run)));
                    assert!(
                        matches!(projector.projection().transcript.last(), Some(TranscriptItem::Message { message, .. }) if matches!(message.parts[1], MessagePart::Image { .. }))
                    );
                }
            }
        }
    }

    #[test]
    fn duplicate_runtime_sequence_is_ignored() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let turn_id = RuntimeTurnId::now();
        let event = envelope(7, chrono::Utc::now(), Event::TurnStart { turn_id });
        assert!(projector.apply_envelope(&event).is_none());
        assert!(projector.apply_envelope(&event).is_none());
        assert_eq!(projector.last_runtime_seq(), 7);
        assert_eq!(projector.projection().revision, Revision(0));
    }

    #[test]
    fn dirty_workspace_remains_actionable_in_the_resource_projection() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let run_id = RuntimeRunId::now();
        let mut projector = SessionProjector::new(session_id, None);

        projector.apply_envelope(&envelope(
            1,
            chrono::Utc::now(),
            Event::WorkspaceLifecycle {
                run_id: run_id.clone(),
                workspace_id: "flow-workspace".into(),
                path: "/tmp/flow-workspace".into(),
                state: "dirty".into(),
                cleanup_error: None,
                reconciliation_reason: None,
            },
        ));

        let resource = &projector.projection().resources[0];
        assert_eq!(resource.kind, ResourceKind::Workspace);
        assert_eq!(resource.state, ResourceState::Dirty);
        assert_eq!(resource.owner_run_id, FlowRunId(run_id.0));
        assert_eq!(resource.finished_at, None);
    }

    #[test]
    fn task_lifecycle_projects_and_reaps_managed_resources() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let task_id = atman_runtime::TaskId::now();
        let run_id = RuntimeRunId::now();
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(2);
        let finished_at = chrono::Utc::now();
        let running = Event::TaskLifecycle {
            task_id: task_id.clone(),
            kind: atman_runtime::TaskKind::Bash,
            run_id: Some(run_id.clone()),
            source_handle: "bg_1".into(),
            label: "build workspace".into(),
            command: Some("cargo build".into()),
            workspace_id: None,
            status: atman_runtime::TaskStatus::Running,
            termination: None,
        };
        let finished = Event::TaskLifecycle {
            task_id: task_id.clone(),
            kind: atman_runtime::TaskKind::Bash,
            run_id: Some(run_id.clone()),
            source_handle: "bg_1".into(),
            label: "build workspace".into(),
            command: Some("cargo build".into()),
            workspace_id: None,
            status: atman_runtime::TaskStatus::Ok,
            termination: None,
        };

        projector.apply_envelope(&envelope(1, started_at, running));
        projector.apply_envelope(&envelope(2, finished_at, finished));
        let resource = &projector.projection().resources[0];
        assert_eq!(resource.id, task_resource_id(&task_id));
        assert_eq!(resource.kind, ResourceKind::BackgroundProcess);
        assert_eq!(resource.state, ResourceState::Exited);
        assert_eq!(resource.owner_run_id, FlowRunId(run_id.0));
        assert_eq!(resource.started_at, Some(started_at));
        assert_eq!(resource.finished_at, Some(finished_at));
        assert_eq!(
            resource.details.get("command").map(String::as_str),
            Some("cargo build")
        );

        let delta = projector
            .apply_envelope(&envelope(3, finished_at, Event::TaskReaped { task_id }))
            .unwrap();
        assert!(projector.projection().resources.is_empty());
        assert!(matches!(
            delta.changes.as_slice(),
            [ProjectionChange::ResourceRemove { .. }]
        ));
    }

    #[test]
    fn terminal_size_updates_one_resource_and_is_idempotent() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let task_id = atman_runtime::TaskId::now();
        let run_id = RuntimeRunId::now();
        projector.apply_envelope(&envelope(
            1,
            chrono::Utc::now(),
            Event::TaskLifecycle {
                task_id: task_id.clone(),
                kind: atman_runtime::TaskKind::Terminal,
                run_id: Some(run_id.clone()),
                source_handle: "term_1".into(),
                label: "interactive shell".into(),
                command: Some("sh".into()),
                workspace_id: None,
                status: atman_runtime::TaskStatus::Running,
                termination: None,
            },
        ));
        let resource_id = task_resource_id(&task_id);
        let before = projector.projection().revision;

        let delta = projector.set_terminal_size(&resource_id, 42, 120).unwrap();
        assert_eq!(delta.base_revision, before);
        assert_eq!(delta.changes.len(), 1);
        let resource = &projector.projection().resources[0];
        assert_eq!(resource.details["rows"], "42");
        assert_eq!(resource.details["cols"], "120");
        assert!(projector.set_terminal_size(&resource_id, 42, 120).is_none());
        assert!(
            projector
                .set_terminal_size(&ResourceId("task:missing".into()), 42, 120)
                .is_none()
        );
        projector.apply_envelope(&envelope(
            2,
            chrono::Utc::now(),
            Event::TerminalFinalState {
                handle: "term_1".into(),
                screen: atman_runtime::tools::term::TerminalScreen {
                    rows: 50,
                    cols: 140,
                    cells: Vec::new(),
                    cursor: None,
                    alt_screen: false,
                },
                state: atman_runtime::tools::term::TermStateSnapshot::Exited { exit_code: None },
            },
        ));
        projector.apply_envelope(&envelope(
            3,
            chrono::Utc::now(),
            Event::TaskLifecycle {
                task_id,
                kind: atman_runtime::TaskKind::Terminal,
                run_id: Some(run_id),
                source_handle: "term_1".into(),
                label: "interactive shell".into(),
                command: Some("sh".into()),
                workspace_id: None,
                status: atman_runtime::TaskStatus::Ok,
                termination: None,
            },
        ));
        let resource = &projector.projection().resources[0];
        assert_eq!(resource.details["rows"], "50");
        assert_eq!(resource.details["cols"], "140");
    }

    #[test]
    fn generation_reconciliation_is_a_durable_replay_boundary() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let turn_id = RuntimeTurnId::now();
        let run_id = RuntimeRunId::now();
        let task_id = atman_runtime::TaskId::now();
        let operation_id = atman_runtime::event::CompactionOperationId::now();
        let context_id = atman_runtime::event::ContextId::now();
        let started_at = chrono::Utc::now() - chrono::Duration::seconds(2);
        let reconciled_at = chrono::Utc::now();
        let mut events = vec![
            envelope(
                1,
                started_at,
                Event::TurnStart {
                    turn_id: turn_id.clone(),
                },
            ),
            envelope(
                2,
                started_at,
                Event::FlowStart {
                    run_id: run_id.clone(),
                    turn_id: Some(turn_id),
                    flow_name: "interrupted".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ),
            envelope(
                3,
                started_at,
                Event::TaskLifecycle {
                    task_id: task_id.clone(),
                    kind: atman_runtime::TaskKind::Bash,
                    run_id: Some(run_id.clone()),
                    source_handle: "bg_interrupted".into(),
                    label: "interrupted command".into(),
                    command: Some("cargo check".into()),
                    workspace_id: None,
                    status: atman_runtime::TaskStatus::Running,
                    termination: None,
                },
            ),
            EventEnvelope {
                seq: 4,
                ts: started_at,
                context_id: Some(context_id.clone()),
                event: Event::CompactionStarted {
                    operation_id: operation_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    range_start: 2,
                    range_end: 5,
                    compacted_count: 4,
                    before_tokens: 8_000,
                },
            },
        ];
        let mut live = SessionProjector::from_events(session_id.clone(), None, &events);
        let recovery = live
            .generation_reconciliation_event("generation-after-restart")
            .unwrap();
        let serialized = serde_json::to_value(&recovery).unwrap();
        assert_eq!(serialized["type"], "generation_reconciled");
        let recovery: Event = serde_json::from_value(serialized).unwrap();
        let recovery = envelope(5, reconciled_at, recovery);
        live.apply_envelope(&recovery);
        events.push(recovery);

        let run = live
            .projection()
            .runs
            .iter()
            .find(|item| item.id.0 == run_id.0)
            .unwrap();
        assert_eq!(run.state, RunLifecycle::Lost);
        assert_eq!(run.finished_at, Some(reconciled_at));
        assert_eq!(
            run.error.as_deref(),
            Some("daemon restarted before a terminal event")
        );
        let resource = live
            .projection()
            .resources
            .iter()
            .find(|item| item.id == task_resource_id(&task_id))
            .unwrap();
        assert_eq!(resource.state, ResourceState::Orphaned);
        assert_eq!(resource.finished_at, Some(reconciled_at));
        assert_eq!(
            resource
                .details
                .get("reconciled_by_generation")
                .map(String::as_str),
            Some("generation-after-restart")
        );
        assert!(live.projection().compactions.is_empty());
        assert!(live.projection().transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::Compaction {
                operation_id: Some(id),
                outcome: CompactionOutcome::Abandoned,
                ..
            } if id.0 == operation_id.0
        )));
        assert_eq!(live.projection().lifecycle, SessionLifecycle::Idle);
        assert!(
            live.generation_reconciliation_event("another-generation")
                .is_none()
        );

        let replayed = SessionProjector::from_events(session_id, None, &events);
        assert_eq!(live.projection(), replayed.projection());
    }

    #[test]
    fn compaction_operations_remain_distinct_and_reconcile_after_disconnect() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let mut projector = SessionProjector::new(session_id, None);
        let first = atman_runtime::event::CompactionOperationId::now();
        let second = atman_runtime::event::CompactionOperationId::now();
        let first_context = atman_runtime::event::ContextId::now();
        let second_context = atman_runtime::event::ContextId::now();
        let run_id = RuntimeRunId::now();
        let at = chrono::Utc::now();
        for (seq, operation_id, context_id) in [
            (1, first.clone(), first_context.clone()),
            (2, second.clone(), second_context.clone()),
        ] {
            projector.apply_envelope(&EventEnvelope {
                seq,
                ts: at,
                context_id: Some(context_id),
                event: Event::CompactionStarted {
                    operation_id,
                    flow_run_id: Some(run_id.clone()),
                    range_start: 3,
                    range_end: 9,
                    compacted_count: 7,
                    before_tokens: 10_000,
                },
            });
        }
        projector.append_compaction_text(&second, "second");
        projector.append_compaction_text(&first, "first");

        assert_eq!(projector.projection().compactions.len(), 2);
        assert_eq!(projector.projection().compactions[0].summary, "first");
        assert_eq!(projector.projection().compactions[1].summary, "second");
        projector.apply_envelope(&EventEnvelope {
            seq: 3,
            ts: at,
            context_id: Some(first_context),
            event: Event::CompactionSummary {
                operation_id: Some(first.clone()),
                session_id: "session".into(),
                flow_run_id: Some(run_id),
                range_start: 3,
                range_end: 9,
                compacted_count: 7,
                before_tokens: 10_000,
                after_tokens: 2_000,
                summary: "first complete".into(),
            },
        });

        assert_eq!(projector.projection().compactions.len(), 1);
        assert_eq!(projector.projection().compactions[0].id.0, second.0);
        let snapshot = projector.snapshot();
        let restored: SessionProjection =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        assert_eq!(restored, snapshot);

        let delta = projector.reconcile_disconnected().unwrap();
        assert!(projector.projection().compactions.is_empty());
        assert!(matches!(
            delta.changes.as_slice(),
            [
                ProjectionChange::TranscriptReplace { .. },
                ProjectionChange::CompactionsReplace { compactions }
            ] if compactions.is_empty()
        ));
        assert!(
            projector
                .projection()
                .transcript
                .iter()
                .any(|item| matches!(
                    item,
                    TranscriptItem::Compaction {
                        operation_id: Some(operation_id),
                        outcome: CompactionOutcome::Finished,
                        ..
                    } if operation_id.0 == first.0
                ))
        );
        assert!(
            projector
                .projection()
                .transcript
                .iter()
                .any(|item| matches!(
                    item,
                    TranscriptItem::Compaction {
                        operation_id: Some(operation_id),
                        outcome: CompactionOutcome::Abandoned,
                        ..
                    } if operation_id.0 == second.0
                ))
        );
    }

    #[test]
    fn watch_overlays_increment_projection_revision_only_when_changed() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let delta = projector.set_goal(Some("Ship clients".into())).unwrap();
        assert_eq!(delta.base_revision, Revision(0));
        assert_eq!(delta.revision, Revision(1));
        assert!(projector.set_goal(Some("Ship clients".into())).is_none());
    }

    #[test]
    fn trust_watch_projects_the_complete_session_policy() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let trust = atman_runtime::trust::TrustConfig {
            mode: atman_runtime::trust::TrustMode::Eager,
            theme: atman_runtime::trust::Theme::Wuxia,
            escalation: atman_runtime::trust::EscalationPolicy::Allow,
            tiers: atman_runtime::trust::TierPolicyConfig {
                eager: atman_runtime::trust::TierPolicyOverrides {
                    tier2: Some(atman_runtime::trust::PolicyAction::Deny),
                    ..Default::default()
                },
            },
            risks: atman_runtime::trust::RiskPolicyConfig {
                eager: atman_runtime::trust::RiskPolicyOverrides {
                    network: Some(atman_runtime::trust::PolicyAction::Auto),
                    ..Default::default()
                },
            },
        };

        let delta = projector.set_trust(trust.clone()).unwrap();
        assert_eq!(projector.projection().trust.mode, TrustMode::Eager);
        assert_eq!(projector.projection().trust.theme, TrustTheme::Wuxia);
        assert_eq!(
            projector.projection().trust.escalation,
            TrustEscalation::Allow
        );
        assert_eq!(
            projector.projection().trust.eager_tiers.tier2,
            Some(TrustPolicyAction::Deny)
        );
        assert_eq!(
            projector.projection().trust.eager_risks.network,
            Some(TrustPolicyAction::Auto)
        );
        assert!(matches!(
            delta.changes.as_slice(),
            [ProjectionChange::TrustSet { .. }]
        ));
        assert_eq!(runtime_trust_config(&projector.projection().trust), trust);
        assert!(projector.set_trust(trust).is_none());
    }

    #[test]
    fn watch_usage_arriving_before_its_event_is_not_double_counted() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        projector.set_context(atman_runtime::ContextSnapshot {
            model: "reasoning-model".into(),
            provider: "openai-compatible".into(),
            tokens_in: 14,
            tokens_out: 5,
            cache_read: 4,
            cache_write: 0,
            last_ttft_ms: 12,
            last_tokens_per_sec: 34.5,
            usage_buckets: vec![atman_runtime::ContextUsageBucket {
                provider: "openai-compatible".into(),
                model: "reasoning-model".into(),
                call_purpose: atman_runtime::context_plan::ContextCallPurpose::Extraction,
                call_scope: atman_runtime::context_plan::ContextCallScope::Child,
                calls: 1,
                tokens_in: 14,
                tokens_out: 5,
                cache_read: 4,
                cache_write: 0,
            }],
            ..Default::default()
        });
        let context = &projector.projection().context;
        assert_eq!(context.input_tokens, 14);
        assert_eq!(context.output_tokens, 5);
        assert_eq!(context.last_ttft_ms, 12);
        assert_eq!(context.last_tokens_per_second, 34.5);
        assert_eq!(context.usage_buckets.len(), 1);
        assert_eq!(
            context.usage_buckets[0].call_purpose,
            LlmCallPurpose::Extraction
        );
        assert_eq!(context.usage_buckets[0].call_scope, LlmCallScope::Child);
        assert_eq!(context.usage_buckets[0].input_tokens, 14);
        projector.apply_envelope(&envelope(
            1,
            chrono::Utc::now(),
            Event::LlmCall {
                model: "reasoning-model".into(),
                provider: "openai-compatible".into(),
                context_plan_id: None,
                managed_context: None,
                context_epoch: None,
                context_tokens: None,
                usage_source: None,
                context_call_purpose: None,
                context_call_identity: None,
                context_cache: None,
                assistant_tool_batch_width: None,
                usage: atman_runtime::provider::TokenUsage {
                    input: 10,
                    output: 5,
                    cached_input: 4,
                    ..Default::default()
                },
                wallclock_ms: 10,
                ttft_ms: None,
                tokens_per_second: None,
                status: Default::default(),
                run_id: None,
                node_id: None,
            },
        ));

        assert_eq!(projector.projection().usage.input_tokens, 14);
        assert_eq!(projector.projection().usage.output_tokens, 5);
        assert_eq!(projector.projection().usage.cache_read_tokens, 4);
        assert_eq!(projector.projection().usage.llm_calls, 1);
    }

    #[test]
    fn workflow_llm_usage_retains_call_identity() {
        let node = atman_runtime::workflow::WorkflowNode {
            id: "run".into(),
            kind: atman_runtime::workflow::WorkflowNodeKind::Flow {
                run_id: uuid::Uuid::now_v7().to_string(),
                flow_name: "agent".into(),
            },
            label: "agent".into(),
            status: atman_runtime::workflow::NodeStatus::Running,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: atman_runtime::workflow::Parallelism::Serial,
            approval: None,
            llm_stats: Some(atman_runtime::workflow::LlmStats {
                model: "helper".into(),
                provider: "openai-compatible".into(),
                context_call_purpose:
                    atman_runtime::context_plan::ContextCallPurpose::Classification,
                context_call_scope: atman_runtime::context_plan::ContextCallScope::Detached,
                ..Default::default()
            }),
        };

        let usage = workflow_node(&node).llm_usage.unwrap();
        assert_eq!(usage.call_purpose, LlmCallPurpose::Classification);
        assert_eq!(usage.call_scope, LlmCallScope::Detached);
    }

    #[test]
    fn workflow_statement_projection_is_structured_and_exhaustive() {
        use atman_runtime::nodegraph::{FanoutMode, NodeKind};

        let cases = [
            (
                NodeKind::Llm {
                    model: Some("reasoning-model".into()),
                },
                WorkflowStatementKind::Llm {
                    model: Some("reasoning-model".into()),
                },
            ),
            (
                NodeKind::ToolCall {
                    path: "fs.read".into(),
                },
                WorkflowStatementKind::ToolCall {
                    path: "fs.read".into(),
                },
            ),
            (
                NodeKind::Fanout {
                    collect: FanoutMode::All,
                },
                WorkflowStatementKind::Fanout {
                    collect: WorkflowFanoutMode::All,
                },
            ),
            (
                NodeKind::Fanout {
                    collect: FanoutMode::First,
                },
                WorkflowStatementKind::Fanout {
                    collect: WorkflowFanoutMode::First,
                },
            ),
            (NodeKind::UserConfirm, WorkflowStatementKind::UserConfirm),
            (
                NodeKind::Subflow {
                    name: "agent".into(),
                },
                WorkflowStatementKind::Subflow {
                    name: "agent".into(),
                },
            ),
            (
                NodeKind::Message {
                    role: "assistant".into(),
                },
                WorkflowStatementKind::Message {
                    role: "assistant".into(),
                },
            ),
            (NodeKind::FixUntilTest, WorkflowStatementKind::FixUntilTest),
            (
                NodeKind::When {
                    condition_preview: "ready".into(),
                },
                WorkflowStatementKind::When {
                    condition_preview: "ready".into(),
                },
            ),
            (NodeKind::Loop, WorkflowStatementKind::Loop),
            (NodeKind::Return, WorkflowStatementKind::Return),
        ];

        for (runtime, public) in cases {
            assert_eq!(workflow_statement_kind(&runtime), public);
        }
    }

    #[test]
    fn approval_projection_retains_the_complete_audit_record() {
        use atman_runtime::permission::{
            ExecutionBoundary, PermissionGroupId, PermissionRequestId,
        };
        use atman_runtime::permission_audit::{
            PermissionAuditScope, PermissionAuditTarget, PermissionEscalationAuditHop,
            PermissionGroupAudit, PermissionGroupAuditOwner, PermissionPolicyReference,
            PermissionProjectionActor, PermissionProvenanceSummary, PermissionRequestAudit,
        };
        use atman_runtime::tool::Tier;

        let request_id = PermissionRequestId::now();
        let group_id = PermissionGroupId::now();
        let requesting_run_id = RuntimeRunId::now();
        let parent_run_id = RuntimeRunId::now();
        let root_run_id = RuntimeRunId::now();
        let at = chrono::Utc::now();
        let payload = PermissionRequestAudit {
            request_id: Some(request_id.clone()),
            revision: 7,
            session_id: "session".into(),
            requesting_run_id: requesting_run_id.clone(),
            parent_run_id: Some(parent_run_id.clone()),
            root_run_id: root_run_id.clone(),
            tool_use_id: "tool-use".into(),
            tool: "bash.spawn".into(),
            call_intent: Some(atman_runtime::message::ToolCallIntent::new("检查进程").unwrap()),
            tier: Tier::Four,
            execution_boundary: Some(ExecutionBoundary::Direct),
            provenance: PermissionProvenanceSummary {
                cwd: Some("/workspace".into()),
                path: Some("/workspace/src/main.rs".into()),
                path_origin: Some("ExplicitInside".into()),
                workspace_id: Some("workspace".into()),
                workspace_root: Some("/workspace".into()),
                repository_root: Some("/workspace".into()),
                network: true,
                risks: ["ProcessSpawn".into()].into_iter().collect(),
                targets: vec!["/workspace/src/main.rs".into()],
            },
            target: PermissionAuditTarget::Flow {
                run_id: parent_run_id.clone(),
            },
            group_ids: vec![group_id.clone()],
            policy: PermissionPolicyReference {
                snapshot_id: "blake3:policy".into(),
                rule_id: "mode=Eager".into(),
            },
            escalation_path: vec![PermissionEscalationAuditHop {
                target: PermissionAuditTarget::User,
                actor: Some(PermissionProjectionActor::Policy {
                    policy_version: "blake3:policy".into(),
                    rule_id: "mode=Eager".into(),
                }),
                action: Some("defer".into()),
                reason: Some("user authority required".into()),
                at,
            }],
            decision_id: Some("decision".into()),
            actor: Some(PermissionProjectionActor::User {
                session_id: "session".into(),
                principal_id: Some("principal".into()),
            }),
            scope: Some(PermissionAuditScope::ChildRunSamePathRule {
                run_id: requesting_run_id.clone(),
                tool_name: "bash.spawn".into(),
                workspace_relative_path: "src/main.rs".into(),
            }),
            reason: Some("approved by user".into()),
            at,
        };

        let public = approval_request_projection(&payload, ApprovalState::Approved).unwrap();
        assert_eq!(public.id, request_id.0);
        assert_eq!(public.requesting_run_id.0, requesting_run_id.0);
        assert_eq!(public.parent_run_id.unwrap().0, parent_run_id.0);
        assert_eq!(public.root_run_id.0, root_run_id.0);
        assert_eq!(public.tool_use_id, "tool-use");
        assert_eq!(public.intent.as_deref(), Some("检查进程"));
        assert_eq!(
            public.execution_boundary,
            Some(ApprovalExecutionBoundary::Direct)
        );
        assert!(public.provenance.risks.contains("ProcessSpawn"));
        assert_eq!(public.group_ids, vec![group_id.0]);
        assert_eq!(public.policy.snapshot_id, "blake3:policy");
        assert!(matches!(
            public.escalation_path[0].actor.as_ref(),
            Some(ApprovalActorProjection::Policy { .. })
        ));
        assert!(matches!(
            public.actor.as_ref(),
            Some(ApprovalActorProjection::User { .. })
        ));
        assert!(matches!(
            public.scope.as_ref(),
            Some(ApprovalScopeProjection::ChildRunSamePathRule { .. })
        ));
        assert_eq!(public.reason.as_deref(), Some("approved by user"));
        assert_eq!(public.at, at);
        assert_eq!(public.revision, 7);

        let group = approval_group_projection(
            &PermissionGroupAudit {
                group_id: group_id.clone(),
                owner: PermissionGroupAuditOwner::User {
                    session_id: "session".into(),
                },
                label: "process checks".into(),
                request_ids: vec![request_id],
                revision: 8,
                at,
            },
            true,
        );
        assert!(group.resolved);
        assert!(matches!(
            group.owner,
            ApprovalGroupOwnerProjection::User { .. }
        ));
        assert_eq!(group.revision, 8);
    }

    #[test]
    fn primary_models_are_projected_per_run_without_helper_pollution() {
        use atman_runtime::context_plan::{
            ContextCallIdentity, ContextCallPurpose, ContextCallScope,
        };

        let session_id = SessionId(uuid::Uuid::now_v7());
        let root_run_id = RuntimeRunId::now();
        let child_run_id = RuntimeRunId::now();
        let started_at = chrono::Utc::now();
        let mut projector = SessionProjector::new(session_id, None);
        projector.apply_envelope(&envelope(
            1,
            started_at,
            Event::FlowStart {
                turn_id: None,
                run_id: root_run_id.clone(),
                flow_name: "agent".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
            },
        ));
        projector.apply_envelope(&envelope(
            2,
            started_at,
            Event::FlowStart {
                turn_id: None,
                run_id: child_run_id.clone(),
                flow_name: "subagent".into(),
                parent_run_id: Some(root_run_id.clone()),
                parent_node_id: Some("spawn".into()),
                spawned: true,
            },
        ));

        let llm_call = |model: &str,
                        provider: &str,
                        purpose: ContextCallPurpose,
                        scope: ContextCallScope,
                        run_id: RuntimeRunId| Event::LlmCall {
            model: model.into(),
            provider: provider.into(),
            context_plan_id: None,
            managed_context: None,
            context_epoch: None,
            context_tokens: None,
            usage_source: None,
            context_call_purpose: Some(purpose),
            context_call_identity: Some(ContextCallIdentity {
                scope,
                session_id: Some("session".into()),
                flow_run_id: (scope == ContextCallScope::Child).then_some(run_id.clone()),
            }),
            context_cache: None,
            assistant_tool_batch_width: None,
            usage: Default::default(),
            wallclock_ms: 1,
            ttft_ms: None,
            tokens_per_second: None,
            status: Default::default(),
            run_id: Some(run_id),
            node_id: None,
        };

        projector.apply_envelope(&envelope(
            3,
            started_at,
            llm_call(
                "root-model",
                "root-provider",
                ContextCallPurpose::General,
                ContextCallScope::Root,
                root_run_id.clone(),
            ),
        ));
        projector.apply_envelope(&envelope(
            4,
            started_at,
            llm_call(
                "child-model",
                "child-provider",
                ContextCallPurpose::General,
                ContextCallScope::Child,
                child_run_id.clone(),
            ),
        ));
        projector.apply_envelope(&envelope(
            5,
            started_at,
            llm_call(
                "root-helper",
                "helper-provider",
                ContextCallPurpose::Extraction,
                ContextCallScope::Root,
                root_run_id.clone(),
            ),
        ));
        projector.apply_envelope(&envelope(
            6,
            started_at,
            llm_call(
                "child-helper",
                "helper-provider",
                ContextCallPurpose::Classification,
                ContextCallScope::Child,
                child_run_id.clone(),
            ),
        ));

        let root = projector
            .projection()
            .runs
            .iter()
            .find(|run| run.id.0 == root_run_id.0)
            .unwrap();
        assert_eq!(root.model.as_deref(), Some("root-model"));
        assert_eq!(root.provider.as_deref(), Some("root-provider"));
        let child = projector
            .projection()
            .runs
            .iter()
            .find(|run| run.id.0 == child_run_id.0)
            .unwrap();
        assert_eq!(child.model.as_deref(), Some("child-model"));
        assert_eq!(child.provider.as_deref(), Some("child-provider"));
        assert_eq!(projector.projection().context.model, "root-model");
        assert_eq!(projector.projection().context.provider, "root-provider");

        for (index, scoped) in [false, true].into_iter().enumerate() {
            let seq = 7 + 2 * index as u64;
            let mut event = llm_call(
                "separate-input-model",
                "separate-input-provider",
                ContextCallPurpose::General,
                ContextCallScope::Root,
                root_run_id.clone(),
            );
            let Event::LlmCall {
                managed_context,
                usage,
                ..
            } = &mut event
            else {
                unreachable!();
            };
            *managed_context = Some(scoped);
            usage.input = 25;
            let mut record = envelope(seq, started_at, event);
            record.context_id = scoped.then(atman_runtime::event::ContextId::now);
            if scoped {
                let mut created = envelope(
                    seq - 1,
                    started_at,
                    Event::ContextCreated {
                        base: None,
                        inheritance: atman_runtime::event::ContextInheritance::Full,
                    },
                );
                created.context_id = record.context_id.clone();
                projector.apply_envelope(&created);
            }
            projector.apply_envelope(&record);
            assert_eq!(projector.projection().context.model, "root-model");
            assert_eq!(projector.projection().context.provider, "root-provider");
            assert_eq!(projector.projection().usage.llm_calls, index as u64 + 5);
            assert_eq!(
                projector.projection().usage.input_tokens,
                (index as u64 + 1) * 25
            );
            let run = projector
                .projection()
                .runs
                .iter()
                .find(|run| run.id.0 == root_run_id.0)
                .unwrap();
            assert_eq!(run.model.as_deref(), Some("separate-input-model"));
        }
    }

    #[test]
    fn context_rewrite_replaces_only_the_target_run_message_range() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let turn_id = RuntimeTurnId::now();
        let run_id = RuntimeRunId::now();
        let other_run_id = RuntimeRunId::now();
        let now = chrono::Utc::now();
        let events = vec![
            envelope(
                1,
                now,
                Event::UserMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    message: Message::user_text(turn_id.clone(), "old user"),
                },
            ),
            envelope(
                2,
                now,
                Event::UserMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(other_run_id),
                    message: Message::user_text(turn_id.clone(), "other run"),
                },
            ),
            envelope(
                3,
                now,
                Event::AssistantMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    message: Message::assistant_text(turn_id.clone(), "old assistant"),
                },
            ),
            envelope(
                4,
                now,
                Event::SystemMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    message: Message::system_text(turn_id, "summary"),
                },
            ),
            envelope(
                5,
                now,
                Event::ContextCompact {
                    session_id: session_id.to_string(),
                    flow_run_id: Some(run_id),
                    before_tokens: 100,
                    after_tokens: 20,
                    compacted_range_start: 0,
                    compacted_range_end: 1,
                    summary_text: Some("summary".into()),
                    replacement_msg_seq: Some(4),
                },
            ),
        ];

        let projector = SessionProjector::from_events(session_id, None, &events);
        let texts = projector
            .projection()
            .transcript
            .iter()
            .filter_map(transcript_text)
            .collect::<Vec<_>>();
        assert_eq!(texts, ["summary", "other run"]);
    }

    #[test]
    fn checkpoint_replaces_messages_without_dropping_non_message_entries() {
        let session_id = SessionId(uuid::Uuid::now_v7());
        let turn_id = RuntimeTurnId::now();
        let run_id = RuntimeRunId::now();
        let now = chrono::Utc::now();
        let events = vec![
            envelope(
                1,
                now,
                Event::UserMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    message: Message::user_text(turn_id.clone(), "old"),
                },
            ),
            envelope(
                2,
                now,
                Event::MermaidDiagram {
                    source: "graph TD".into(),
                },
            ),
            envelope(
                3,
                now,
                Event::Checkpoint {
                    session_id: session_id.to_string(),
                    flow_run_id: Some(run_id),
                    messages: vec![Message::user_text(turn_id.clone(), "kept")],
                    window_tokens: 5,
                },
            ),
        ];

        let projector = SessionProjector::from_events(session_id, None, &events);
        assert_eq!(
            projector
                .projection()
                .transcript
                .iter()
                .filter_map(transcript_text)
                .collect::<Vec<_>>(),
            ["kept"]
        );
        assert!(matches!(
            projector.projection().transcript[1],
            TranscriptItem::Mermaid { .. }
        ));
    }

    #[test]
    fn captured_steering_preserves_slots_across_compaction_replay_and_client_deltas() {
        use atman_runtime::injection::{Injection, InjectionState};
        for owner_kind in ["root", "legacy-child", "typed-child"] {
            let scoped = owner_kind == "typed-child";
            for checkpoint in [false, true] {
                let session = atman_runtime::Session::open_ephemeral();
                let sid = SessionId(uuid::Uuid::now_v7());
                let turn = RuntimeTurnId::now();
                let root = RuntimeRunId::now();
                let child = RuntimeRunId::now();
                let inline = RuntimeRunId::now();
                let sink = session.sink().clone();
                for (run, parent, spawned) in [
                    (&root, None, false),
                    (&child, Some(root.clone()), true),
                    (&inline, Some(child.clone()), false),
                ] {
                    sink.emit(Event::FlowStart {
                        run_id: run.clone(),
                        turn_id: Some(turn.clone()),
                        flow_name: "test".into(),
                        parent_run_id: parent,
                        parent_node_id: None,
                        spawned,
                    });
                }
                let run = if owner_kind == "root" { &root } else { &inline };
                let other_run = if owner_kind == "root" { &child } else { &root };
                let context_id = atman_runtime::event::ContextId::now();
                let owner = if scoped {
                    sink.clone().with_context(context_id.clone())
                } else {
                    sink.clone()
                };
                if scoped {
                    owner.emit(Event::ContextCreated {
                        base: None,
                        inheritance: atman_runtime::event::ContextInheritance::Full,
                    });
                }
                let user = Message::user_text(turn.clone(), "old user");
                owner.emit(Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: Some(run.clone()),
                    message: user.clone(),
                });
                sink.emit(Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: Some(other_run.clone()),
                    message: Message::user_text(turn.clone(), "other run"),
                });
                let mut steering = Message::user_text(turn.clone(), "captured steering");
                steering.origin = atman_runtime::message::MessageOrigin::Interjection;
                for (state, captured) in [
                    (InjectionState::Pending, true),
                    (InjectionState::Cancelled, true),
                    (InjectionState::Injected, false),
                    (InjectionState::Injected, true),
                ] {
                    let mut injection = Injection::new_pending(turn.clone(), "steering");
                    injection.flow_run_id = Some(run.clone());
                    injection.state = state;
                    owner.emit(Event::UserInject {
                        turn_id: turn.clone(),
                        injection,
                        context_message: captured.then(|| steering.clone()),
                    });
                }
                let assistant = Message::assistant_text(turn.clone(), "after steering");
                owner.emit(Event::AssistantMsg {
                    turn_id: turn.clone(),
                    flow_run_id: Some(run.clone()),
                    message: assistant.clone(),
                });
                if checkpoint {
                    owner.emit(Event::Checkpoint {
                        session_id: sid.to_string(),
                        flow_run_id: Some(run.clone()),
                        messages: vec![user, steering, assistant],
                        window_tokens: 10,
                    });
                }
                let replacement = owner.emit_returning_seq(Event::SystemMsg {
                    turn_id: turn.clone(),
                    flow_run_id: Some(run.clone()),
                    message: Message::system_text(turn.clone(), "summary"),
                });
                owner.emit(Event::ContextCompact {
                    session_id: sid.to_string(),
                    flow_run_id: Some(run.clone()),
                    before_tokens: 100,
                    after_tokens: 20,
                    compacted_range_start: 0,
                    compacted_range_end: 1,
                    summary_text: Some("summary".into()),
                    replacement_msg_seq: Some(replacement),
                });
                let mut projector = SessionProjector::new(sid.clone(), None);
                let mut restored: SessionProjector =
                    serde_json::from_value(serde_json::to_value(&projector).unwrap()).unwrap();
                let generation = atman_proto::DaemonGeneration("test-generation".into());
                let mut client = atman_client::SessionState::new(
                    atman_proto::SessionSnapshot {
                        schema_version: atman_proto::SNAPSHOT_SCHEMA_VERSION,
                        daemon_generation: generation.clone(),
                        cursor: EventCursor(0),
                        projection: projector.snapshot(),
                    },
                    &generation,
                )
                .unwrap();
                let events = sink.snapshot_envelopes();
                for (index, event) in events.iter().enumerate() {
                    let delta = projector.apply_envelope(event);
                    assert_eq!(delta, restored.apply_envelope(event));
                    if let Some(delta) = delta {
                        if matches!(event.event, Event::UserInject { .. }) {
                            let expected = usize::from(event.event.context_message().is_some());
                            let items: Vec<_> = delta
                                .changes
                                .iter()
                                .flat_map(|change| match change {
                                    ProjectionChange::TranscriptAppend { items } => {
                                        items.as_slice()
                                    }
                                    _ => &[],
                                })
                                .collect();
                            assert_eq!(items.len(), expected);
                            assert!(items.iter().all(|item| matches!(item, TranscriptItem::Message { message, .. } if message.origin == MessageOrigin::Interjection)));
                            assert!(delta.changes.iter().any(|change| matches!(
                                change,
                                ProjectionChange::InteractionsSet { .. }
                            )));
                        }
                        let cursor = EventCursor(client.cursor().0 + 1);
                        client
                            .apply_updates(&atman_proto::GetSessionUpdatesResponse {
                                daemon_generation: generation.clone(),
                                events: vec![atman_proto::ProjectionEventEnvelope {
                                    schema_version: atman_proto::PROJECTION_EVENT_SCHEMA_VERSION,
                                    daemon_generation: generation.clone(),
                                    session_id: sid.clone(),
                                    cursor,
                                    ts: event.ts,
                                    event: atman_proto::ServerEvent::ProjectionDelta { delta },
                                }],
                                next_cursor: cursor,
                                has_more: false,
                                resync_required: None,
                            })
                            .unwrap();
                    }
                    assert_eq!(client.projection(), projector.projection());
                    assert_eq!(
                        SessionProjector::from_events(sid.clone(), None, &events[..=index])
                            .projection(),
                        projector.projection()
                    );
                    restored =
                        serde_json::from_value(serde_json::to_value(&projector).unwrap()).unwrap();
                }
                let target_texts = projector
                    .projection()
                    .transcript
                    .iter()
                    .filter_map(|item| match item {
                        TranscriptItem::Message {
                            run_id: Some(id), ..
                        } if id.0 == run.0 => transcript_text(item),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(target_texts, ["summary", "after steering"]);
                assert_eq!(
                    projector
                        .projection()
                        .transcript
                        .iter()
                        .filter_map(transcript_text)
                        .filter(|text| *text == "other run")
                        .count(),
                    1
                );
                if owner_kind == "legacy-child" {
                    continue;
                }
                let base = if scoped {
                    atman_runtime::event::ContextBase::Context {
                        context_id,
                        through_seq: sink.published_seq(),
                    }
                } else {
                    atman_runtime::event::ContextBase::LegacyRoot {
                        through_seq: sink.published_seq(),
                    }
                };
                let context =
                    atman_runtime::projection::context::replay_context(&events, &base).unwrap();
                assert_eq!(
                    context
                        .window()
                        .iter()
                        .map(|(_, m)| m.text_concat())
                        .collect::<Vec<_>>(),
                    target_texts
                );
            }
        }
    }

    fn transcript_text(item: &TranscriptItem) -> Option<&str> {
        let TranscriptItem::Message { message, .. } = item else {
            return None;
        };
        message.parts.iter().find_map(|part| match part {
            MessagePart::Text { text } => Some(text.as_str()),
            _ => None,
        })
    }
}
