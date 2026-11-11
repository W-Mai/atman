use super::*;
use crate::flow_store::{
    branches::{ConversationHistory, HistoryHeadChange},
    context::{CommittedContextCheckpoint, ContextCheckpointRef},
    conversation::ConversationReadKind,
    goal::{GoalRequest, GoalUpdate},
    replay::FlowReplay,
};
use crate::memory::goal::{GoalActor, GoalEdit, GoalPauseReason, GoalRecord, GoalStatus};

#[cfg(test)]
mod tests;

pub(crate) struct SelectedHistory {
    pub head: HistoryHead,
    pub messages: Vec<crate::message::Message>,
    pub history: Vec<crate::message::Message>,
    pub checkpoint_epoch: Option<String>,
}

impl HistoryOperations {
    fn application(
        &self,
        sender: &SenderIdentity,
        id: Uuid,
        state: &atman_rt::FlowInstance<AtmanUuid>,
        head: &HistoryHead,
        goal: Option<&GoalRecord>,
    ) -> io::Result<(HistoryHeadChange, Option<GoalRequest>)> {
        let record = self.validate_abort(sender, id)?;
        if !matches!(record.status, HistoryOperationStatus::Ready { .. })
            || record.request.expected_head != *head
            || record.request.goal != goal.map(GoalVersion::from)
            || !matches!(state.state(), InstanceState::Open(activity) if !matches!(activity, InstanceActivity::Active(_)))
        {
            return Err(invalid(
                "history application requires unchanged reviewed head and Goal after execution cleanup",
            ));
        }
        let change = HistoryHeadChange {
            operation: id,
            expected: head.clone(),
            selection: record.selection.clone(),
        };
        let pause = goal
            .filter(|goal| goal.status == GoalStatus::Active)
            .map(|goal| GoalRequest {
                id,
                sender: sender.clone(),
                expected: Some(GoalVersion::from(goal)),
                expected_head: None,
                edit: GoalEdit::Pause {
                    actor: GoalActor::User,
                    reason: GoalPauseReason::UserRequested,
                    summary: String::new(),
                },
                start_turn: false,
            });
        Ok((change, pause))
    }
}

fn selection_view(selection: &HistorySelection, through: u64) -> (ConversationBranchId, u64) {
    match selection {
        HistorySelection::Create { parent, .. } => (parent.branch, parent.cursor.sequence),
        HistorySelection::Restore { branch } => (*branch, through),
    }
}

fn checkpoint_at<'a>(
    history: &ConversationHistory,
    checkpoints: &'a [ContextCheckpointRef],
    branch: ConversationBranchId,
    through: u64,
) -> io::Result<Option<&'a ContextCheckpointRef>> {
    let spans = history.ancestry_through(branch, through)?;
    Ok(checkpoints.iter().rev().find(|record| {
        let sequence = record.receipt.sequence();
        sequence <= through
            && spans
                .get(spans.partition_point(|span| span.last_sequence < sequence))
                .is_some_and(|span| span.first_sequence <= sequence)
    }))
}

impl FlowStore {
    /// Atomically select reviewed history and pause its active Goal. Clients install their selected context before exposing the resulting view.
    pub fn apply_history_operation(
        &mut self,
        sender: SenderIdentity,
        id: Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()>,
    ) -> io::Result<HistoryOperationRecord> {
        self.apply_history_with_context(sender, id, authorize)
            .map(|(record, _)| record)
    }

    pub(crate) fn apply_history_with_context(
        &mut self,
        sender: SenderIdentity,
        id: Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()>,
    ) -> io::Result<(HistoryOperationRecord, Option<SelectedHistory>)> {
        let record = self
            .history_operations
            .find(&sender, id)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        authorize(record)?;
        if matches!(record.status, HistoryOperationStatus::Applied { .. }) {
            return Ok((record.clone(), None));
        }
        let (change, pause) = self.history_operations.application(
            &sender,
            id,
            &self.state,
            self.inbox.head(),
            self.goal(),
        )?;
        let projection = self
            .branches
            .as_ref()
            .ok_or_else(|| invalid("history application requires durable native ancestry"))?;
        let sequence = self
            .cursor
            .sequence()
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let prepared = projection.prepare(change.clone(), sequence, |receipt| {
            self.journal.previous_durable_receipt(receipt).map(drop)
        })?;
        let (branch, through) = selection_view(&change.selection, self.cursor.sequence());
        let checkpoint_id = checkpoint_at(
            &projection.history,
            self.checkpoints.records(),
            branch,
            through,
        )?
        .map(|record| record.id);
        let checkpoint = checkpoint_id
            .map(|id| self.context_checkpoint(id))
            .transpose()?
            .flatten();
        // Decode before publication so unavailable selected context cannot be reported as a successful selection.
        let selected = SelectedHistory {
            head: prepared.head.clone(),
            messages: self.conversation_at(ConversationReadKind::ModelContext, branch, through)?,
            history: self.conversation_at(ConversationReadKind::History, branch, through)?,
            checkpoint_epoch: checkpoint
                .as_ref()
                .map(|checkpoint| {
                    crate::flow_store::context::context_digest(&checkpoint.checkpoint.messages)
                })
                .transpose()?,
        };
        let at = Utc::now();
        let update = pause.map(|request| GoalUpdate {
            request,
            new_goal_id: Uuid::now_v7(),
            at,
            input: None,
        });
        let goal = update
            .as_ref()
            .map(|update| self.goals.prepare(&self.state, update))
            .transpose()?;
        let event = HistoryOperationEvent::Applied {
            sender,
            id,
            head: prepared.head.clone(),
            at,
        };
        let mut facts = vec![
            FlowFact::HistoryOperation(Box::new(event.clone())),
            FlowFact::HistoryHeadChanged(Box::new(change)),
        ];
        if let Some(update) = &update {
            facts.push(FlowFact::GoalUpdated(Box::new(update.clone())));
        }
        let receipt = self.journal.commit(&facts)?;
        if let (Some(update), Some(goal)) = (update, goal) {
            self.inbox.goal_changed(goal.as_ref(), &receipt);
            self.task_scopes.goal_changed(goal.as_ref(), &receipt);
            self.goals.apply(update, goal, receipt.clone());
        }
        self.inbox.select_head(prepared.head.clone());
        self.branches
            .as_mut()
            .expect("validated native ancestry")
            .install(prepared, receipt.require_durable()?.clone());
        self.checkpoints.select_latest(checkpoint);
        self.cursor = receipt.clone();
        Ok((
            self.history_operations.apply(event, receipt),
            Some(selected),
        ))
    }
}

impl FlowReplay {
    pub(in crate::flow_store) fn apply_history_selection(
        &mut self,
        directory: &std::path::Path,
        receipt: FlowPosition,
        facts: Vec<FlowFact>,
        prefix: &crate::event_log::journal::JournalPrefix<'_>,
    ) -> io::Result<()> {
        let [
            FlowFact::HistoryOperation(event),
            FlowFact::HistoryHeadChanged(change),
            rest @ ..,
        ] = facts.as_slice()
        else {
            return Err(crate::flow_store::branches::unsupported());
        };
        let HistoryOperationEvent::Applied {
            sender,
            id,
            head,
            at,
        } = event.as_ref()
        else {
            return Err(invalid("history application requires an Applied receipt"));
        };
        let (_, state, inbox) = self
            .projection
            .as_ref()
            .ok_or_else(|| invalid("history application has no Flow creation"))?;
        let (expected_change, expected_pause) = self.history_operations.application(
            sender,
            *id,
            state,
            inbox.head(),
            self.goals.current(),
        )?;
        if change.as_ref() != &expected_change {
            return Err(invalid(
                "history change differs from its reviewed operation",
            ));
        }
        let update = match rest {
            [] => None,
            [FlowFact::GoalUpdated(update)] => Some(update.as_ref()),
            _ => return Err(invalid("invalid history application batch")),
        };
        if update.map(|update| &update.request) != expected_pause.as_ref()
            || update.is_some_and(|update| update.at != *at)
        {
            return Err(invalid(
                "history application has no matching Goal disposition",
            ));
        }
        let goal = update
            .map(|update| self.goals.prepare(state, update))
            .transpose()?;
        let projection = self
            .branches
            .as_ref()
            .ok_or_else(|| invalid("history application requires durable native ancestry"))?;
        let prepared = projection.prepare(expected_change, receipt.sequence(), |cursor| {
            prefix.validate_receipt(cursor)
        })?;
        if &prepared.head != head {
            return Err(invalid(
                "history application head differs from its prepared selection",
            ));
        }
        let (branch, through) = selection_view(&change.selection, receipt.sequence() - 1);
        let plan = self.conversation_plan(
            &projection.history,
            branch,
            through,
            ConversationReadKind::ModelContext,
        )?;
        let window = plan.read(|position| {
            let batch = prefix.read::<FlowFact>(position.require_durable()?)?;
            crate::flow_store::images::resolve(directory, &batch.facts)
        })?;
        let checkpoint = checkpoint_at(
            &projection.history,
            self.checkpoints.records(),
            branch,
            through,
        )?
        .map(|record| {
            let batch = prefix.read::<FlowFact>(record.receipt.require_durable()?)?;
            crate::flow_store::images::resolve(directory, &batch.facts)?
                .into_iter()
                .find_map(|fact| match fact {
                    FlowFact::ContextCheckpoint(checkpoint) if checkpoint.id == record.id => {
                        Some(CommittedContextCheckpoint {
                            checkpoint: *checkpoint,
                            receipt: record.receipt.clone(),
                        })
                    }
                    _ => None,
                })
                .ok_or_else(|| invalid("selected checkpoint is absent from its original batch"))
        })
        .transpose()?;
        let inbox = &mut self.projection.as_mut().expect("validated Flow creation").2;
        if let (Some(update), Some(goal)) = (update, goal) {
            inbox.goal_changed(goal.as_ref(), &receipt);
            self.task_scopes.goal_changed(goal.as_ref(), &receipt);
            self.goals.apply(update.clone(), goal, receipt.clone());
        }
        inbox.select_head(prepared.head.clone());
        self.branches
            .as_mut()
            .expect("validated native ancestry")
            .install(prepared, receipt.require_durable()?.clone());
        self.checkpoints.select_latest(checkpoint);
        self.context_window = window;
        self.history_operations
            .apply(*event.clone(), receipt.clone());
        super::super::activity::observe(&mut self.activity, &receipt, &facts);
        self.cursor = Some(receipt);
        Ok(())
    }
}
