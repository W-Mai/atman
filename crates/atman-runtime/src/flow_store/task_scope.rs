//! Explicit cancellation groups owned by a Flow, independent of creation provenance.

use std::{collections::BTreeMap, io};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CommitReceipt, FlowFact, FlowRef, FlowStore, invalid};
use crate::memory::goal::GoalRecord;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TaskScopeRef {
    pub owner: FlowRef,
    pub id: Uuid,
}

impl TaskScopeRef {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.id.is_nil()
            || self.owner.session_id.0.is_nil()
            || self.owner.instance_id.0.0.is_nil()
        {
            return Err(invalid("task scope requires a nonempty owner and identity"));
        }
        Ok(())
    }
}

/// Pause/resume revisions do not change the work to which a scope is bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopeGoal {
    pub id: String,
    pub objective: String,
}

impl From<&GoalRecord> for ScopeGoal {
    fn from(goal: &GoalRecord) -> Self {
        Self {
            id: goal.id.clone(),
            objective: goal.objective.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskScopeDefinition {
    pub id: Uuid,
    pub name: String,
    pub goal: Option<ScopeGoal>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskScopeCreated {
    pub definition: TaskScopeDefinition,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeCancelReason {
    Requested,
    GoalChanged,
    OwnerClosed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScopeCancellation {
    pub reason: ScopeCancelReason,
    pub receipt: CommitReceipt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskScopeRecord {
    pub created: TaskScopeCreated,
    pub receipt: CommitReceipt,
    pub cancellation: Option<ScopeCancellation>,
}

#[derive(Default)]
pub(super) struct TaskScopes {
    records: BTreeMap<Uuid, TaskScopeRecord>,
    binding: Option<(TaskScopeRef, CommitReceipt)>,
}

impl TaskScopes {
    pub fn records(&self) -> impl Iterator<Item = &TaskScopeRecord> {
        self.records.values()
    }

    pub fn binding(&self) -> Option<&TaskScopeRef> {
        self.binding.as_ref().map(|(scope, _)| scope)
    }

    fn validate_definition(
        &self,
        definition: &TaskScopeDefinition,
        goal: Option<&GoalRecord>,
    ) -> io::Result<()> {
        if definition.id.is_nil()
            || definition.name.trim().is_empty()
            || self.records.contains_key(&definition.id)
            || definition
                .goal
                .as_ref()
                .is_some_and(|bound| goal.map(ScopeGoal::from).as_ref() != Some(bound))
        {
            return Err(invalid("invalid task scope definition or goal binding"));
        }
        Ok(())
    }

    pub fn register(
        &mut self,
        created: TaskScopeCreated,
        goal: Option<&GoalRecord>,
        receipt: CommitReceipt,
    ) -> io::Result<()> {
        self.validate_definition(&created.definition, goal)?;
        self.records.insert(
            created.definition.id,
            TaskScopeRecord {
                created,
                receipt,
                cancellation: None,
            },
        );
        Ok(())
    }

    pub fn cancel(&mut self, id: Uuid, receipt: CommitReceipt) -> io::Result<()> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| invalid("unknown task scope"))?;
        if record.cancellation.is_some() {
            return Err(invalid("task scope already cancelled"));
        }
        record.cancellation = Some(ScopeCancellation {
            reason: ScopeCancelReason::Requested,
            receipt,
        });
        Ok(())
    }

    pub fn goal_changed(&mut self, goal: Option<&GoalRecord>, receipt: &CommitReceipt) {
        let current = goal.map(ScopeGoal::from);
        for record in self.records.values_mut() {
            if record.cancellation.is_none()
                && record
                    .created
                    .definition
                    .goal
                    .as_ref()
                    .is_some_and(|bound| current.as_ref() != Some(bound))
            {
                record.cancellation = Some(ScopeCancellation {
                    reason: ScopeCancelReason::GoalChanged,
                    receipt: receipt.clone(),
                });
            }
        }
    }

    pub fn close(&mut self, receipt: &CommitReceipt) {
        for record in self.records.values_mut() {
            if record.cancellation.is_none() {
                record.cancellation = Some(ScopeCancellation {
                    reason: ScopeCancelReason::OwnerClosed,
                    receipt: receipt.clone(),
                });
            }
        }
    }

    fn validate_binding(&self, scope: &TaskScopeRef, address: &FlowRef) -> io::Result<()> {
        scope.validate()?;
        if self.binding.is_some() || scope.owner == *address {
            return Err(invalid("invalid or duplicate task scope binding"));
        }
        Ok(())
    }

    pub fn bind(
        &mut self,
        scope: TaskScopeRef,
        address: &FlowRef,
        receipt: CommitReceipt,
    ) -> io::Result<()> {
        self.validate_binding(&scope, address)?;
        self.binding = Some((scope, receipt));
        Ok(())
    }
}

impl FlowStore {
    pub fn task_scopes(&self) -> impl Iterator<Item = &TaskScopeRecord> {
        self.task_scopes.records()
    }

    pub fn task_scope_binding(&self) -> Option<&TaskScopeRef> {
        self.task_scopes.binding()
    }

    /// The host authenticates the owner before calling. Matching retries retain terminal status and the creation receipt.
    pub fn create_task_scope(
        &mut self,
        definition: TaskScopeDefinition,
    ) -> io::Result<TaskScopeRecord> {
        if let Some(record) = self.task_scopes.records.get(&definition.id) {
            return if record.created.definition == definition {
                Ok(record.clone())
            } else {
                Err(invalid(
                    "task scope identity conflicts with recorded definition",
                ))
            };
        }
        if !matches!(self.state.state(), atman_rt::InstanceState::Open(_)) {
            return Err(invalid("only open instances may own task scopes"));
        }
        self.task_scopes
            .validate_definition(&definition, self.goal())?;
        let created = TaskScopeCreated {
            definition,
            at: Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::TaskScopeCreated(Box::new(created.clone()))])?;
        let id = created.definition.id;
        self.task_scopes
            .register(created, self.goals.current(), receipt.clone())?;
        self.cursor = receipt;
        Ok(self.task_scopes.records[&id].clone())
    }

    pub fn cancel_task_scope(&mut self, id: Uuid) -> io::Result<CommitReceipt> {
        let record = self
            .task_scopes
            .records
            .get(&id)
            .ok_or_else(|| invalid("unknown task scope"))?;
        if let Some(cancelled) = &record.cancellation {
            return Ok(cancelled.receipt.clone());
        }
        let receipt = self.journal.commit(&[FlowFact::TaskScopeCancelled(id)])?;
        self.task_scopes.cancel(id, receipt.clone())?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Before binding, the host verifies that the referenced owner is open and its scope is active. Membership cannot be changed after activation or reassigned to another scope.
    pub fn bind_task_scope(&mut self, scope: TaskScopeRef) -> io::Result<CommitReceipt> {
        if let Some((previous, receipt)) = &self.task_scopes.binding {
            return if previous == &scope {
                Ok(receipt.clone())
            } else {
                Err(invalid("flow already belongs to another task scope"))
            };
        }
        if !matches!(self.state.state(), atman_rt::InstanceState::Prepared) {
            return Err(invalid("task scope membership must precede activation"));
        }
        self.task_scopes
            .validate_binding(&scope, self.state.address())?;
        let receipt = self
            .journal
            .commit(&[FlowFact::TaskScopeBound(scope.clone())])?;
        self.task_scopes
            .bind(scope, self.state.address(), receipt.clone())?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
