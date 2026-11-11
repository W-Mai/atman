//! Shared fact validation and projection for read-only inspection and writer recovery.

use super::{
    AtmanUuid, FlowContent, FlowDescriptor, FlowFact, FlowInstance, FlowJournal, FlowPosition,
    FlowRef, Inbox, InstanceState, completion, context, continuation, goal, history, invalid, io,
    launch, outbox, planning, process_watch, publication, subscription, trace, waiter, workspace,
};

#[derive(Default)]
pub(super) struct FlowReplay {
    pub(super) projection: Option<(FlowDescriptor, FlowInstance<AtmanUuid>, Inbox)>,
    pub(super) outbox: outbox::Outbox,
    pub(super) publications: publication::Publications,
    pub(super) subscriptions: subscription::Subscriptions,
    pub(super) process_watches: process_watch::ProcessWatches,
    pub(super) timers: super::timer::Timers,
    pub(super) task_scopes: super::task_scope::TaskScopes,
    pub(super) watch_replacements: super::watch_edit::WatchReplacements,
    pub(super) activation: Option<(launch::Activation, FlowPosition)>,
    pub(super) workspace: Option<workspace::WorkspaceRequest>,
    pub(super) closed: Option<FlowPosition>,
    pub(super) completions: completion::Completions,
    pub(super) checkpoints: context::Checkpoints,
    pub(super) names: super::name::Names,
    pub(super) goals: goal::Goals,
    pub(super) planning: planning::Planning,
    pub(super) controls: super::control::Controls,
    pub(super) trace: trace::ExecutionTrace,
    pub(super) history: history::HistoryProgress,
    pub(super) cursor: Option<FlowPosition>,
    pub(super) activity: Option<super::activity::RecordedActivity>,
    // Only the current model window is needed to validate checkpoints during scanning.
    pub(super) context_window: Vec<crate::message::Message>,
}

impl FlowReplay {
    pub(super) fn conversation_plan(
        &self,
        history: &super::branches::ConversationHistory,
        branch: super::source_capture::ConversationBranchId,
        through: u64,
        kind: super::conversation::ConversationReadKind,
    ) -> io::Result<super::conversation::ConversationPlan> {
        let (_, _, inbox) = self
            .projection
            .as_ref()
            .ok_or_else(|| invalid("conversation has no Flow creation"))?;
        let spans = history.ancestry(branch)?;
        Ok(super::conversation::ConversationPlan::select(
            kind,
            self.activation.as_ref().map(|(_, receipt)| receipt),
            self.checkpoints
                .records()
                .iter()
                .map(|record| &record.receipt),
            inbox
                .inputs()
                .iter()
                .filter(|record| record.message_count > 0)
                .map(|record| &record.receipt),
            self.publications
                .records()
                .iter()
                .filter(|record| record.context_run.is_none())
                .map(|record| &record.receipt),
            |receipt| {
                let sequence = receipt.sequence();
                sequence <= through
                    && spans
                        .get(spans.partition_point(|span| span.last_sequence < sequence))
                        .is_some_and(|span| span.first_sequence <= sequence)
            },
        ))
    }

    /// Select only conversation context; lifecycle, effects and pending inputs retain their original facts.
    pub(super) fn select_conversation(
        &mut self,
        dir: &std::path::Path,
        history: &super::branches::ConversationHistory,
        prefix: &crate::event_log::journal::JournalPrefix<'_>,
    ) -> io::Result<()> {
        let (_, state, _) = self
            .projection
            .as_ref()
            .ok_or_else(|| invalid("conversation selection precedes Flow creation"))?;
        if !matches!(state.state(), InstanceState::Open(activity) if !matches!(activity, atman_rt::InstanceActivity::Active(_)))
        {
            return Err(invalid(
                "conversation selection requires completed active execution",
            ));
        }
        let plan = self.conversation_plan(
            history,
            history.head.branch,
            history.observed.sequence,
            super::conversation::ConversationReadKind::ModelContext,
        )?;
        let window = plan.read(|receipt| {
            let batch = prefix.read::<FlowFact>(receipt.require_durable()?)?;
            super::images::resolve(dir, &batch.facts)
        })?;
        self.context_window = window;
        self.projection
            .as_mut()
            .expect("validated Flow creation")
            .2
            .select_head(history.head.clone());
        self.cursor = Some(history.observed.clone().into());
        Ok(())
    }

    pub(super) fn apply(
        &mut self,
        dir: &std::path::Path,
        address: &FlowRef,
        receipt: FlowPosition,
        mut facts: Vec<FlowFact>,
    ) -> io::Result<()> {
        receipt.require_durable()?;
        for fact in &mut facts {
            fact.visit_positions(&mut |position| position.require_durable().map(|_| ()))?;
        }
        FlowFact::validate_batch(&facts)?;
        if facts
            .iter()
            .any(|fact| matches!(fact, FlowFact::HistoryHeadChanged(_)))
        {
            return Err(super::branches::unsupported());
        }
        let resolved = super::images::resolve(dir, &facts)?;
        super::activity::observe(&mut self.activity, &receipt, &facts);
        if let Some((descriptor, _, _)) = &self.projection {
            FlowJournal::validate(
                matches!(descriptor.content, FlowContent::History(_)),
                &facts,
            )?;
        }
        if matches!(facts.last(), Some(FlowFact::ImagesRetained(_))) {
            facts.pop();
        }
        for (fact, owned) in facts.into_iter().zip(resolved) {
            let owned_messages = match owned {
                FlowFact::Activated(record) => record.input.inherited_context,
                FlowFact::InputCommitted(record) => record.content.messages().to_vec(),
                FlowFact::MessagePublished(record) => vec![record.message],
                FlowFact::ContextCheckpoint(record) => record.messages,
                _ => Vec::new(),
            };
            let mut state_changed = false;
            match (&mut self.projection, fact) {
                (None, FlowFact::Created(descriptor)) if &descriptor.address == address => {
                    let state = descriptor.content.initial_state(address.clone())?;
                    let inbox = Inbox::new(super::branches::HistoryHead::initial(
                        address,
                        receipt.journal_id(),
                    ));
                    self.projection = Some((*descriptor, state, inbox));
                }
                (Some((descriptor, _, _)), FlowFact::LegacyRecords(records)) => {
                    let FlowContent::History(archive) = &descriptor.content else {
                        return Err(invalid("legacy records require a history-only flow"));
                    };
                    self.history = self.history.advance(archive, &records)?;
                }
                (Some((_, state, _)), FlowFact::Transition(mut transition)) => {
                    let original = transition.clone();
                    let revision = state.revision();
                    transition.apply(state)?;
                    if original != transition || revision == state.revision() {
                        return Err(invalid("noncanonical flow transition"));
                    }
                    state_changed = true;
                }
                (Some((_, state, _)), FlowFact::Controlled(update)) => {
                    let candidate = self.controls.prepare(state, &update.request)?;
                    state_changed = candidate != *state;
                    *state = candidate;
                    self.controls.apply(*update, state.clone(), receipt.clone());
                }
                (Some((_, state, _)), FlowFact::WorkspaceRequested(request)) => {
                    request.replay(state, self.workspace.as_ref())?;
                    self.workspace = Some(*request);
                }
                (Some((descriptor, state, inbox)), FlowFact::Activated(record)) => {
                    if record.input.scope.as_ref() != self.task_scopes.binding() {
                        return Err(invalid(
                            "activation scope does not match its recorded membership",
                        ));
                    }
                    if record.input.workspace != self.workspace {
                        return Err(invalid(
                            "activation workspace does not match its recorded request",
                        ));
                    }
                    if self.activation.is_some() {
                        return Err(invalid("duplicate instance activation"));
                    }
                    let (candidate, prepared) = record.validate(
                        descriptor,
                        state,
                        inbox,
                        self.cursor
                            .as_ref()
                            .ok_or_else(|| invalid("activation precedes creation"))?,
                    )?;
                    *state = candidate;
                    inbox.state_changed(state, &receipt);
                    record.apply(inbox, &receipt, prepared);
                    self.context_window = owned_messages;
                    publication::trim_context(&mut self.context_window);
                    if let Some(name) = &record.input.name {
                        self.names.initialize(name)?;
                    }
                    self.names.observe_launch(&record.input);
                    self.activation = Some((*record, receipt.clone()));
                }
                (Some((_, state, inbox)), FlowFact::TurnCompleted(completion)) => {
                    inbox.turn_completed(&completion, &receipt);
                    self.completions
                        .replay(state, *completion, receipt.clone())?;
                    inbox.state_changed(state, &receipt);
                }
                (Some((_, state, inbox)), FlowFact::EnvelopeAccepted(envelope)) => {
                    if !state.accepts_input() || &envelope.request.recipient != address {
                        return Err(invalid("acceptance fact targets an unavailable flow"));
                    }
                    let prepared = inbox.validate(&envelope)?;
                    continuation::validate_request(&envelope.request, state, self.goals.current())?;
                    inbox.apply(prepared, receipt.clone());
                }
                (Some((descriptor, state, inbox)), FlowFact::InputCommitted(input)) => {
                    inbox.replay_input(state, &input, receipt.clone())?;
                    self.names.observe_input(&input.content, descriptor);
                    publication::extend_context(&mut self.context_window, &owned_messages);
                }
                (Some((_, state, inbox)), FlowFact::InboxEdited(edit)) => {
                    if !state.accepts_input() {
                        return Err(invalid("inbox edit targets an unavailable flow"));
                    }
                    inbox.replay_edit(*edit, receipt.clone())?;
                }
                (Some((_, _, inbox)), FlowFact::InputScopeInvalidated(observation)) => {
                    inbox.replay_scope_invalidation(*observation, receipt.clone())?;
                }
                (Some((_, state, _)), FlowFact::MessagePublished(publication)) => {
                    self.publications.validate(state, &publication)?;
                    if publication.context_run.is_none() {
                        self.names.observe_message(&publication.message);
                        publication::extend_context(&mut self.context_window, &owned_messages);
                    }
                    self.publications.apply(&publication, receipt.clone());
                }
                (Some((_, state, _)), FlowFact::ExecutionEvents(events)) => {
                    self.trace.validate(&events, state, &self.completions)?;
                    self.trace.apply(events, receipt.clone());
                }
                (Some((_, state, inbox)), FlowFact::GoalUpdated(update)) => {
                    let record = self.goals.prepare(state, &update)?;
                    let prepared = update
                        .input
                        .as_ref()
                        .map(|input| inbox.validate(input))
                        .transpose()?;
                    inbox.goal_changed(record.as_ref(), &receipt);
                    self.task_scopes.goal_changed(record.as_ref(), &receipt);
                    if let Some(input) = prepared {
                        inbox.apply(input, receipt.clone());
                    }
                    self.goals.apply(*update, record, receipt.clone());
                }
                (Some(_), FlowFact::NameUpdated(update)) => {
                    let snapshot = self.names.prepare(&update.request)?;
                    self.names.apply(update.request, snapshot, receipt.clone());
                }
                (Some((_, state, _)), FlowFact::PlanningUpdated(update)) => {
                    let (next, result) = self.planning.prepare(state, &update)?;
                    self.planning.apply(*update, next, result, receipt.clone());
                }
                (Some((_, state, _)), FlowFact::ContextCheckpoint(mut checkpoint)) => {
                    self.checkpoints
                        .validate(state, &checkpoint, &self.context_window)?;
                    self.context_window = owned_messages;
                    checkpoint.messages = self.context_window.clone();
                    publication::trim_context(&mut self.context_window);
                    self.checkpoints.apply(*checkpoint, receipt.clone());
                }
                (Some((_, state, inbox)), FlowFact::FlowSubscribed(record)) => {
                    if !state.accepts_input()
                        || self.process_watches.contains(record.subscription.id)
                    {
                        return Err(invalid("subscription recipient is unavailable"));
                    }
                    subscription::Subscriptions::validate_reply(&record, inbox, address)?;
                    self.subscriptions.register(*record)?;
                }
                (Some((_, _, inbox)), FlowFact::FlowUnsubscribed(id)) => {
                    self.subscriptions.cancel(id, inbox, &receipt)?
                }
                (Some((_, state, inbox)), FlowFact::EventsObserved(observation)) => {
                    if !state.accepts_input() {
                        return Err(invalid("observation recipient is unavailable"));
                    }
                    let prepared = self.subscriptions.validate(&observation, inbox, address)?;
                    self.subscriptions
                        .apply(&observation, inbox, &receipt, prepared);
                }
                (Some((_, state, inbox)), FlowFact::FlowWatchesExpired(expiry)) => {
                    if !state.accepts_input() {
                        return Err(invalid("flow expiry recipient is unavailable"));
                    }
                    let prepared = self
                        .subscriptions
                        .validate_expiry(&expiry, inbox, address)?;
                    self.subscriptions
                        .apply_expiry(*expiry, inbox, &receipt, prepared);
                }
                (Some((_, state, _)), FlowFact::ProcessWatchRegistered(record)) => {
                    if !state.accepts_input() || self.subscriptions.contains(record.watch.id) {
                        return Err(invalid("process watch recipient is unavailable"));
                    }
                    self.process_watches.register(*record)?;
                }
                (Some(_), FlowFact::ProcessWatchCancelled(id)) => {
                    self.process_watches.cancel(id)?
                }
                (Some((_, state, inbox)), FlowFact::WatcherReplaced(edit)) => {
                    if !state.accepts_input() {
                        return Err(invalid("watcher replacement recipient is unavailable"));
                    }
                    self.watch_replacements.apply(
                        *edit,
                        &mut self.subscriptions,
                        &mut self.process_watches,
                        inbox,
                        &receipt,
                    )?;
                }
                (Some((_, state, inbox)), FlowFact::ProcessObserved(observation)) => {
                    if !state.accepts_input() {
                        return Err(invalid("process observation recipient is unavailable"));
                    }
                    let prepared = self
                        .process_watches
                        .validate(&observation, inbox, address)?;
                    self.process_watches
                        .apply(*observation, inbox, &receipt, prepared);
                }
                (Some((_, state, inbox)), FlowFact::ProcessWatchesExpired(expiry)) => {
                    if !state.accepts_input() {
                        return Err(invalid("process expiry recipient is unavailable"));
                    }
                    let prepared = self
                        .process_watches
                        .validate_expiry(&expiry, inbox, address)?;
                    self.process_watches
                        .apply_expiry(*expiry, inbox, &receipt, prepared);
                }
                (Some((_, state, _)), FlowFact::TimerScheduled(record)) => {
                    if !state.accepts_input() {
                        return Err(invalid("timer recipient is unavailable"));
                    }
                    self.timers.register(*record)?;
                }
                (Some(_), FlowFact::TimerCancelled(id)) => self.timers.cancel(id)?,
                (Some((_, state, inbox)), FlowFact::TimersFired(delivery)) => {
                    if !state.accepts_input() {
                        return Err(invalid("timer recipient is unavailable"));
                    }
                    let prepared = self.timers.validate_delivery(&delivery, inbox, address)?;
                    self.timers
                        .apply_delivery(*delivery, inbox, &receipt, prepared);
                }
                (Some((_, state, _)), FlowFact::TaskScopeCreated(created)) => {
                    if !matches!(state.state(), InstanceState::Open(_)) {
                        return Err(invalid("task scope owner is not open"));
                    }
                    self.task_scopes
                        .register(*created, self.goals.current(), receipt.clone())?;
                }
                (Some(_), FlowFact::TaskScopeCancelled(id)) => {
                    self.task_scopes.cancel(id, receipt.clone())?;
                }
                (Some((_, state, _)), FlowFact::TaskScopeBound(scope)) => {
                    if !matches!(state.state(), InstanceState::Prepared) {
                        return Err(invalid("task scope binding follows activation"));
                    }
                    self.task_scopes.bind(scope, address, receipt.clone())?;
                }
                (Some((_, state, inbox)), FlowFact::SendQueued(intent)) => {
                    self.outbox.validate(state, &intent)?;
                    if let Some(reply) = &intent.reply {
                        let record = inbox
                            .waiters
                            .get(reply.correlation)
                            .ok_or_else(|| invalid("send reply waiter is not registered"))?;
                        if record.waiter != *reply || record.status != waiter::ReplyStatus::Pending
                        {
                            return Err(invalid("send reply waiter is no longer pending"));
                        }
                    }
                    self.outbox.apply(&intent, receipt.clone());
                }
                (
                    Some(_),
                    FlowFact::DeliveryConfirmed {
                        request_id,
                        delivery,
                    },
                ) => {
                    self.outbox.validate_confirmation(request_id, &delivery)?;
                    self.outbox.confirm(request_id, delivery, receipt.clone());
                }
                (
                    Some((_, _, inbox)),
                    FlowFact::DeliveryFailed {
                        request_id,
                        failure,
                    },
                ) => {
                    self.outbox.validate_failure(request_id, &failure)?;
                    self.outbox
                        .fail(request_id, failure, receipt.clone(), inbox);
                }
                (Some((_, state, inbox)), FlowFact::ReplyWaiterRegistered(waiter)) => {
                    inbox.validate_waiter(state, &waiter)?;
                    inbox.invalidate();
                    inbox.waiters.register(*waiter, receipt.clone());
                }
                (Some((_, _, inbox)), FlowFact::ReplyWaiterCancelled { lease, correlation }) => {
                    let record = inbox
                        .waiters
                        .get(correlation)
                        .ok_or_else(|| invalid("reply waiter does not exist"))?;
                    if record.waiter.lease != lease || record.status != waiter::ReplyStatus::Pending
                    {
                        return Err(invalid("reply waiter cancellation is stale"));
                    }
                    inbox.invalidate();
                    inbox.waiters.cancel(correlation, receipt.clone());
                }
                _ => {
                    return Err(invalid("missing, duplicate, or mismatched flow creation"));
                }
            }
            if state_changed {
                let (_, state, inbox) = self.projection.as_mut().expect("validated creation");
                inbox.state_changed(state, &receipt);
                if matches!(state.state(), InstanceState::Closed) {
                    self.closed = Some(receipt.clone());
                }
                if matches!(
                    state.state(),
                    InstanceState::Closing { .. } | InstanceState::Closed
                ) {
                    inbox.close(&receipt);
                    self.subscriptions.close();
                    self.process_watches.close();
                    self.timers.close();
                    self.task_scopes.close(&receipt);
                }
            }
        }
        self.cursor = Some(receipt);
        Ok(())
    }
}
