//! Objective facts share the instance journal and never change execution state.

use std::{collections::HashMap, io};

use atman_rt::{FlowInstance, InstanceState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowStore, inbox::SenderIdentity, invalid};
use crate::{
    event::AtmanUuid,
    event_log::journal::CommitReceipt,
    memory::goal::{GoalEdit, GoalRecord},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalVersion {
    pub id: String,
    pub revision: u64,
}

impl From<&GoalRecord> for GoalVersion {
    fn from(goal: &GoalRecord) -> Self {
        Self {
            id: goal.id.clone(),
            revision: goal.revision,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalRequest {
    pub id: Uuid,
    /// Authenticated by the host before calling the storage owner.
    pub sender: SenderIdentity,
    /// None explicitly expects no current objective.
    pub expected: Option<GoalVersion>,
    pub edit: GoalEdit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GoalUpdate {
    pub request: GoalRequest,
    pub new_goal_id: Uuid,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommittedGoalUpdate {
    pub update: GoalUpdate,
    pub goal: Option<GoalRecord>,
    pub receipt: CommitReceipt,
}

#[derive(Default)]
pub(super) struct Goals {
    current: Option<GoalRecord>,
    records: Vec<CommittedGoalUpdate>,
    indices: HashMap<(SenderIdentity, Uuid), usize>,
}

impl Goals {
    pub(super) fn current(&self) -> Option<&GoalRecord> {
        self.current.as_ref()
    }
    fn previous(&self, request: &GoalRequest) -> io::Result<Option<&CommittedGoalUpdate>> {
        let record = self
            .indices
            .get(&(request.sender.clone(), request.id))
            .map(|index| &self.records[*index]);
        match record {
            Some(record) if &record.update.request != request => Err(invalid(
                "goal request identity conflicts with recorded data",
            )),
            record => Ok(record),
        }
    }

    pub fn prepare(
        &self,
        state: &FlowInstance<AtmanUuid>,
        update: &GoalUpdate,
    ) -> io::Result<Option<GoalRecord>> {
        let request = &update.request;
        if !matches!(state.state(), InstanceState::Open(_))
            || request.id.is_nil()
            || update.new_goal_id.is_nil()
            || self.previous(request)?.is_some()
            || request.expected != self.current.as_ref().map(GoalVersion::from)
        {
            return Err(invalid("stale or invalid goal update"));
        }
        let mut record = self.current.clone();
        request.edit.apply(
            &mut record,
            &state.address().instance_id.to_string(),
            None,
            update.new_goal_id,
            update.at,
        )?;
        crate::memory::goal::advance_revision(&self.current, &mut record, update.at)?;
        Ok(record)
    }

    pub fn apply(
        &mut self,
        update: GoalUpdate,
        goal: Option<GoalRecord>,
        receipt: CommitReceipt,
    ) -> CommittedGoalUpdate {
        self.indices.insert(
            (update.request.sender.clone(), update.request.id),
            self.records.len(),
        );
        self.current = goal.clone();
        let record = CommittedGoalUpdate {
            update,
            goal,
            receipt,
        };
        self.records.push(record.clone());
        record
    }
}

impl FlowStore {
    pub fn goal(&self) -> Option<&GoalRecord> {
        self.goals.current.as_ref()
    }

    pub fn goal_updates(&self) -> &[CommittedGoalUpdate] {
        &self.goals.records
    }

    /// Authenticate the sender before calling; matching retries precede new-change authorization.
    pub fn edit_goal(
        &mut self,
        request: GoalRequest,
        authorize_new: impl FnOnce(&GoalRequest) -> io::Result<()>,
    ) -> io::Result<CommittedGoalUpdate> {
        if let Some(record) = self.goals.previous(&request)? {
            return Ok(record.clone());
        }
        authorize_new(&request)?;
        let update = GoalUpdate {
            request,
            new_goal_id: Uuid::now_v7(),
            at: chrono::Utc::now(),
        };
        let goal = self.goals.prepare(&self.state, &update)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::GoalUpdated(Box::new(update.clone()))])?;
        self.inbox.goal_changed(goal.as_ref(), &receipt);
        let record = self.goals.apply(update, goal, receipt.clone());
        self.cursor = receipt;
        Ok(record)
    }
}
