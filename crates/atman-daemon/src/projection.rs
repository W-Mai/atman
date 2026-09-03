use std::collections::HashMap;

use atman_proto::{
    ApprovalGroupProjection, ApprovalRequestProjection, ApprovalState, ApprovalTarget,
    ContextProjection, FlowRunId, ImageDetail, InteractionProjection, InterjectionProjection,
    InterjectionSource, LlmUsageProjection, McpServerProjection, MessageOrigin, MessagePart,
    MessageProjection, MessageRole, NameSource, NoticeLevel, PlanProjection, PlanStepProjection,
    ProjectionChange, ProjectionDelta, ResourceId, ResourceKind, ResourceProjection, ResourceState,
    Revision, RunLifecycle, RunProjection, SessionId, SessionLifecycle, SessionMetadataProjection,
    SessionProjection, TodoProjection, TodoState, TranscriptItem, TurnId, UsageProjection,
    WorkflowNodeKind, WorkflowNodeProjection, WorkflowNodeState, WorkflowProjection,
};
use atman_runtime::event::{Event, EventEnvelope, FlowStatus};
use atman_runtime::message::ImageData;
use atman_runtime::projection::workflow::WorkflowProjection as RuntimeWorkflowProjection;

pub(crate) struct SessionProjector {
    projection: SessionProjection,
    current_turn: Option<atman_runtime::event::TurnId>,
    run_turns: HashMap<atman_runtime::event::FlowRunId, atman_runtime::event::TurnId>,
    workflows: Vec<(atman_runtime::event::TurnId, RuntimeWorkflowProjection)>,
    event_usage: UsageProjection,
    watch_usage: UsageProjection,
    last_runtime_seq: u64,
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
                goal: None,
                todos: Vec::new(),
                plans: Vec::new(),
                context: ContextProjection::default(),
                interactions: InteractionProjection::default(),
                resources: Vec::new(),
                usage: UsageProjection::default(),
            },
            current_turn: None,
            run_turns: HashMap::new(),
            workflows: Vec::new(),
            event_usage: UsageProjection::default(),
            watch_usage: UsageProjection::default(),
            last_runtime_seq: 0,
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

    pub(crate) fn reconcile_disconnected(&mut self) -> Option<ProjectionDelta> {
        let mut changes = Vec::new();
        for run in &mut self.projection.runs {
            if matches!(
                run.state,
                RunLifecycle::Queued
                    | RunLifecycle::Starting
                    | RunLifecycle::Running
                    | RunLifecycle::WaitingInput
                    | RunLifecycle::Cancelling
            ) {
                run.state = RunLifecycle::Lost;
                changes.push(ProjectionChange::RunUpsert { run: run.clone() });
            }
        }
        for resource in &mut self.projection.resources {
            if matches!(
                resource.state,
                ResourceState::Starting | ResourceState::Running | ResourceState::Terminating
            ) {
                resource.state = ResourceState::Orphaned;
                changes.push(ProjectionChange::ResourceUpsert {
                    resource: resource.clone(),
                });
            }
        }

        let previous_interactions = self.projection.interactions.clone();
        self.projection.interactions.prompts.clear();
        self.projection.interactions.forms.clear();
        self.projection.interactions.compact_review = None;
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
        if self.projection.lifecycle != SessionLifecycle::Idle {
            self.projection.lifecycle = SessionLifecycle::Idle;
            changes.push(ProjectionChange::LifecycleSet {
                lifecycle: SessionLifecycle::Idle,
            });
        }
        self.commit(changes)
    }

    pub(crate) fn register_run(
        &mut self,
        run_id: FlowRunId,
        flow_name: String,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Option<ProjectionDelta> {
        if self.projection.runs.iter().any(|run| run.id == run_id) {
            return None;
        }
        let run = RunProjection {
            id: run_id,
            flow_name,
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

    pub(crate) fn apply_envelope(&mut self, envelope: &EventEnvelope) -> Option<ProjectionDelta> {
        if envelope.seq <= self.last_runtime_seq {
            return None;
        }
        self.last_runtime_seq = envelope.seq;
        let mut changes = Vec::new();

        match &envelope.event {
            Event::TurnStart { turn_id } => self.current_turn = Some(turn_id.clone()),
            Event::TurnEnd { turn_id } => {
                if self.current_turn.as_ref() == Some(turn_id) {
                    self.current_turn = None;
                }
            }
            Event::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ..
            } => {
                let turn_id = parent_run_id
                    .as_ref()
                    .and_then(|parent| self.run_turns.get(parent))
                    .cloned()
                    .or_else(|| self.current_turn.clone())
                    .unwrap_or_else(orphan_turn_id);
                self.run_turns.insert(run_id.clone(), turn_id);
                let run = RunProjection {
                    id: FlowRunId(run_id.0),
                    flow_name: flow_name.clone(),
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
                        flow_name: String::new(),
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
            Event::UserMsg {
                flow_run_id,
                message,
                ..
            }
            | Event::AssistantMsg {
                flow_run_id,
                message,
                ..
            }
            | Event::ToolResultMsg {
                flow_run_id,
                message,
                ..
            }
            | Event::SystemMsg {
                flow_run_id,
                message,
                ..
            } => self.append_transcript(
                TranscriptItem::Message {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    run_id: flow_run_id.as_ref().map(|id| FlowRunId(id.0)),
                    message: message_projection(message),
                },
                &mut changes,
            ),
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
            Event::CompactionSummary {
                range_start,
                range_end,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => self.append_transcript(
                TranscriptItem::Compaction {
                    seq: envelope.seq,
                    ts: envelope.ts,
                    range_start: *range_start,
                    range_end: *range_end,
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    summary: summary.clone(),
                },
                &mut changes,
            ),
            Event::ContextCompact {
                flow_run_id,
                compacted_range_start,
                compacted_range_end,
                replacement_msg_seq,
                ..
            } => {
                if replacement_msg_seq.is_some_and(|replacement_seq| {
                    self.compact_transcript_messages(
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
                    .map(|message| TranscriptItem::Message {
                        seq: envelope.seq,
                        ts: envelope.ts,
                        run_id: run_id.clone(),
                        message: message_projection(message),
                    })
                    .collect();
                if self.replace_transcript_messages(flow_run_id.as_ref(), replacement) {
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
            } => {
                let resource = ResourceProjection {
                    id: ResourceId(format!("workspace:{workspace_id}")),
                    kind: ResourceKind::Workspace,
                    state: workspace_state(state),
                    owner_run_id: FlowRunId(run_id.0),
                    tool_use_id: None,
                    label: path.clone(),
                    started_at: Some(envelope.ts),
                    finished_at: resource_is_terminal(state).then_some(envelope.ts),
                    details: cleanup_error
                        .iter()
                        .map(|error| ("cleanup_error".into(), error.clone()))
                        .collect(),
                };
                self.upsert_resource(resource.clone());
                changes.push(ProjectionChange::ResourceUpsert { resource });
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
                self.projection.interactions.compact_review = Some(review);
                changes.push(ProjectionChange::InteractionsSet {
                    interactions: self.projection.interactions.clone(),
                });
            }
            Event::CompactReviewResolved { review_id, .. } => {
                if self
                    .projection
                    .interactions
                    .compact_review
                    .as_ref()
                    .is_some_and(|review| review.id == *review_id)
                {
                    self.projection.interactions.compact_review = None;
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
                let group = ApprovalGroupProjection {
                    id: payload.group_id.0,
                    label: payload.label.clone(),
                    request_ids: payload.request_ids.iter().map(|id| id.0).collect(),
                    revision: payload.revision,
                };
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
                usage,
                ..
            } => {
                let previous_usage = self.projection.usage.clone();
                let previous_context = self.projection.context.clone();
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
                self.projection.context.model = model.clone();
                self.projection.context.provider = provider.clone();
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

    pub(crate) fn set_forms(
        &mut self,
        forms: Vec<atman_runtime::form::PendingForm>,
    ) -> Option<ProjectionDelta> {
        let mut forms = forms
            .iter()
            .map(pending_form_projection)
            .collect::<Vec<_>>();
        forms.sort_by_key(|form| form.emitted_at);
        if self.projection.interactions.forms == forms {
            return None;
        }
        self.projection.interactions.forms = forms;
        self.commit(vec![ProjectionChange::InteractionsSet {
            interactions: self.projection.interactions.clone(),
        }])
    }

    pub(crate) fn set_compact_review(
        &mut self,
        review: Option<atman_runtime::session::PendingCompactReview>,
    ) -> Option<ProjectionDelta> {
        let review = review.as_ref().map(compact_review_projection);
        if self.projection.interactions.compact_review == review {
            return None;
        }
        self.projection.interactions.compact_review = review;
        self.commit(vec![ProjectionChange::InteractionsSet {
            interactions: self.projection.interactions.clone(),
        }])
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
        run_id: Option<&atman_runtime::event::FlowRunId>,
        range_start: usize,
        range_end: usize,
        replacement_seq: u64,
    ) -> bool {
        let slots = transcript_message_slots(&self.projection.transcript, run_id);
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
        run_id: Option<&atman_runtime::event::FlowRunId>,
        replacement: Vec<TranscriptItem>,
    ) -> bool {
        let slots = transcript_message_slots(&self.projection.transcript, run_id);
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
        let Some(request_id) = payload.request_id.as_ref() else {
            return;
        };
        let approval = ApprovalRequestProjection {
            id: request_id.0,
            run_id: FlowRunId(payload.requesting_run_id.0),
            tool_name: payload.tool.clone(),
            tier: tier_number(payload.tier),
            state,
            target: Some(match &payload.target {
                atman_runtime::permission_audit::PermissionAuditTarget::Flow { run_id } => {
                    ApprovalTarget::Flow {
                        run_id: FlowRunId(run_id.0),
                    }
                }
                atman_runtime::permission_audit::PermissionAuditTarget::User => {
                    ApprovalTarget::User
                }
            }),
            revision: payload.revision,
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
            .or_else(|| self.current_turn.clone())
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
    let mut value = serde_json::to_value(event)?;
    redactor.redact_json(&mut value);
    Ok(serde_json::from_value(value)?)
}

pub(crate) async fn load_historical_projection(
    session_id: SessionId,
    session_dir: &std::path::Path,
) -> anyhow::Result<SessionProjection> {
    let replay_dir = session_dir.to_path_buf();
    let replay_session_id = session_id.clone();
    let (meta, events, goal) = tokio::task::spawn_blocking(move || {
        let events_path = replay_dir.join("events.jsonl");
        anyhow::ensure!(
            events_path.is_file(),
            "session not found: {replay_session_id}"
        );
        let meta = atman_runtime::session_meta::SessionMeta::load(&replay_dir);
        let events = atman_runtime::event_log::reader::read_event_envelopes(&events_path)?;
        let goal = atman_runtime::memory::goal::GoalStore::at(&replay_dir).get()?;
        Ok::<_, anyhow::Error>((meta, events, goal))
    })
    .await
    .map_err(|error| anyhow::anyhow!("historical session replay task failed: {error}"))??;

    let context = atman_runtime::event_log::reader::context_snapshot_from_envelopes(&events);
    let todo_store = atman_runtime::memory::todo::TodoStore::at(session_dir);
    let plan_store = atman_runtime::memory::plan::PlanStore::at(session_dir);
    let (todos, plans) = tokio::join!(todo_store.list(), plan_store.list());
    let mut projector = SessionProjector::from_events(session_id, meta, &events);
    projector.set_goal((!goal.is_empty()).then_some(goal));
    projector.set_todos(todos?);
    projector.set_plans(plans?);
    projector.set_context(context);
    projector.reconcile_disconnected();
    Ok(projector.snapshot())
}

fn transcript_message_slots(
    transcript: &[TranscriptItem],
    run_id: Option<&atman_runtime::event::FlowRunId>,
) -> Vec<(usize, u64)> {
    let run_id = run_id.map(|id| id.0);
    transcript
        .iter()
        .enumerate()
        .filter_map(|(output_index, item)| match item {
            TranscriptItem::Message {
                seq,
                run_id: item_run_id,
                ..
            } if item_run_id.as_ref().map(|id| id.0) == run_id => Some((output_index, *seq)),
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

fn message_projection(message: &atman_runtime::message::Message) -> MessageProjection {
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
        parts: message.parts.iter().map(message_part).collect(),
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
        summary: pending.summary.clone(),
        slice_preview: pending.slice_preview.clone(),
        slice_count: pending.slice_count,
        range_start: pending.range_start,
        range_end: pending.range_end,
        tokens_before: pending.tokens_before,
        emitted_at: pending.emitted_at,
    }
}

fn message_part(part: &atman_runtime::message::MessagePart) -> MessagePart {
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
        atman_runtime::message::MessagePart::Image { source } => {
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
                    kind: format!("{node_kind:?}"),
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
        window_tokens: context.window_tokens,
        window_budget: context.window_budget,
        cost_usd: context.cost_usd,
        cache_read_tokens: context.cache_read,
        cache_write_tokens: context.cache_write,
        memory_recent_count: context.memory_recent_count,
        mcp_servers: context
            .mcp_servers
            .iter()
            .map(mcp_server_projection)
            .collect(),
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
    let (state, tool_count) = match &status.state {
        atman_runtime::mcp::McpServerState::Disabled => ("disabled", 0),
        atman_runtime::mcp::McpServerState::Pending => ("pending", 0),
        atman_runtime::mcp::McpServerState::Connecting => ("connecting", 0),
        atman_runtime::mcp::McpServerState::Connected { tool_count, .. } => {
            ("connected", *tool_count)
        }
        atman_runtime::mcp::McpServerState::Error { .. } => ("error", 0),
        atman_runtime::mcp::McpServerState::Disconnected { .. } => ("disconnected", 0),
        atman_runtime::mcp::McpServerState::Timeout { .. } => ("timeout", 0),
    };
    McpServerProjection {
        name: status.name.clone(),
        transport: format!("{:?}", status.transport).to_lowercase(),
        state: state.into(),
        tool_count,
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

fn workspace_state(state: &str) -> ResourceState {
    match state {
        "starting" | "allocating" => ResourceState::Starting,
        "active" | "running" => ResourceState::Running,
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

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::{FlowRunId as RuntimeRunId, TurnId as RuntimeTurnId};
    use atman_runtime::message::Message;

    fn envelope(seq: u64, ts: chrono::DateTime<chrono::Utc>, event: Event) -> EventEnvelope {
        EventEnvelope { seq, ts, event }
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
    fn watch_overlays_increment_projection_revision_only_when_changed() {
        let mut projector = SessionProjector::new(SessionId(uuid::Uuid::now_v7()), None);
        let delta = projector.set_goal(Some("Ship clients".into())).unwrap();
        assert_eq!(delta.base_revision, Revision(0));
        assert_eq!(delta.revision, Revision(1));
        assert!(projector.set_goal(Some("Ship clients".into())).is_none());
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
            usage_buckets: vec![atman_runtime::ContextUsageBucket {
                provider: "openai-compatible".into(),
                model: "reasoning-model".into(),
                calls: 1,
                ..Default::default()
            }],
            ..Default::default()
        });
        projector.apply_envelope(&envelope(
            1,
            chrono::Utc::now(),
            Event::LlmCall {
                model: "reasoning-model".into(),
                provider: "openai-compatible".into(),
                context_plan_id: None,
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
