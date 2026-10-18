//! Per-instance plans and todos committed through the flow journal.

use std::{collections::HashMap, io};

use atman_rt::InstanceState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowStore, inbox::SenderIdentity, invalid};
use crate::{
    event_log::journal::CommitReceipt,
    memory::{
        MemoryId,
        plan::{Plan, PlanEntry},
        todo::{Todo, TodoEntry, TodoStatus},
    },
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanningEdit {
    AddTodo {
        #[serde(rename = "where")]
        where_: String,
        why: String,
        how: String,
        expected_result: String,
    },
    SetTodoStatus {
        id: MemoryId,
        status: TodoStatus,
    },
    DeleteTodo {
        id: MemoryId,
    },
    ClearTodos,
    WritePlan {
        id: String,
        title: String,
        steps: Vec<String>,
    },
    TickPlan {
        id: Option<String>,
        step_index: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanningRequest {
    pub id: Uuid,
    /// Authenticated by the host, never accepted from model-supplied identity fields.
    pub sender: SenderIdentity,
    /// Optional optimistic concurrency for interactive edits; independent tool actions are serialized.
    pub expected_revision: Option<u64>,
    pub edit: PlanningEdit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanningUpdate {
    pub request: PlanningRequest,
    pub new_id: Uuid,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PlanningSnapshot {
    pub revision: u64,
    pub todos: Vec<Todo>,
    pub plans: Vec<Plan>,
}

impl PlanningSnapshot {
    pub fn plan(&self, id: Option<&str>) -> Option<&Plan> {
        match id {
            Some(id) => self.plans.iter().find(|plan| plan.id == id),
            None => self.plans.iter().max_by_key(|plan| plan.updated_at),
        }
    }

    fn apply(&mut self, update: &PlanningUpdate) -> io::Result<PlanningOutcome> {
        let result = match &update.request.edit {
            PlanningEdit::AddTodo {
                where_,
                why,
                how,
                expected_result,
            } => {
                let id = MemoryId(update.new_id);
                TodoEntry::Add(Todo {
                    id: id.clone(),
                    where_: where_.clone(),
                    why: why.clone(),
                    how: how.clone(),
                    expected_result: expected_result.clone(),
                    status: TodoStatus::Pending,
                })
                .apply(&mut self.todos);
                PlanningOutcome::Todo(id)
            }
            PlanningEdit::SetTodoStatus { id, status } => {
                if !self.todos.iter().any(|todo| &todo.id == id) {
                    return Err(invalid("todo does not exist"));
                }
                TodoEntry::Update {
                    id: id.clone(),
                    status: status.clone(),
                }
                .apply(&mut self.todos);
                PlanningOutcome::Todo(id.clone())
            }
            PlanningEdit::DeleteTodo { id } => {
                TodoEntry::Delete { id: id.clone() }.apply(&mut self.todos);
                PlanningOutcome::Todo(id.clone())
            }
            PlanningEdit::ClearTodos => {
                self.todos.clear();
                PlanningOutcome::TodosCleared
            }
            PlanningEdit::WritePlan { id, title, steps } => {
                let plan = Plan::new_at(id, title, steps.clone(), update.at);
                PlanEntry::Upsert(plan).apply(&mut self.plans);
                PlanningOutcome::Plan(self.plan(Some(id)).expect("upserted plan").clone())
            }
            PlanningEdit::TickPlan { id, step_index } => {
                let plan = self
                    .plan(id.as_deref())
                    .ok_or_else(|| invalid("plan does not exist"))?;
                if !plan.steps.iter().any(|step| step.index == *step_index) {
                    return Err(invalid("plan step does not exist"));
                }
                let id = plan.id.clone();
                PlanEntry::Tick {
                    plan_id: id.clone(),
                    step_index: *step_index,
                    at: update.at,
                }
                .apply(&mut self.plans);
                PlanningOutcome::Plan(self.plan(Some(&id)).expect("updated plan").clone())
            }
        };
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("planning revision exhausted"))?;
        Ok(result)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PlanningOutcome {
    Todo(MemoryId),
    TodosCleared,
    Plan(Plan),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommittedPlanningUpdate {
    pub update: PlanningUpdate,
    pub revision: u64,
    pub outcome: PlanningOutcome,
    pub receipt: CommitReceipt,
}

#[derive(Default)]
pub(super) struct Planning {
    current: PlanningSnapshot,
    records: HashMap<(SenderIdentity, Uuid), CommittedPlanningUpdate>,
}

impl Planning {
    fn previous(&self, request: &PlanningRequest) -> io::Result<Option<&CommittedPlanningUpdate>> {
        match self.records.get(&(request.sender.clone(), request.id)) {
            Some(record) if &record.update.request != request => Err(invalid(
                "planning request identity conflicts with recorded data",
            )),
            other => Ok(other),
        }
    }

    pub(super) fn prepare(
        &self,
        state: &atman_rt::FlowInstance<crate::event::AtmanUuid>,
        update: &PlanningUpdate,
    ) -> io::Result<(PlanningSnapshot, PlanningOutcome)> {
        if !matches!(state.state(), InstanceState::Open(_))
            || update.request.id.is_nil()
            || update.new_id.is_nil()
            || self.previous(&update.request)?.is_some()
            || update
                .request
                .expected_revision
                .is_some_and(|revision| revision != self.current.revision)
        {
            return Err(invalid("stale or invalid planning update"));
        }
        let mut next = self.current.clone();
        let result = next.apply(update)?;
        Ok((next, result))
    }

    pub(super) fn apply(
        &mut self,
        update: PlanningUpdate,
        next: PlanningSnapshot,
        outcome: PlanningOutcome,
        receipt: CommitReceipt,
    ) -> CommittedPlanningUpdate {
        let record = CommittedPlanningUpdate {
            update,
            revision: next.revision,
            outcome,
            receipt,
        };
        self.current = next;
        self.records.insert(
            (
                record.update.request.sender.clone(),
                record.update.request.id,
            ),
            record.clone(),
        );
        record
    }
}

impl FlowStore {
    pub fn planning(&self) -> &PlanningSnapshot {
        &self.planning.current
    }

    /// Authenticate the sender before calling; retry receipts survive closure and later edits.
    pub fn edit_planning(
        &mut self,
        request: PlanningRequest,
        authorize_new: impl FnOnce(&PlanningRequest) -> io::Result<()>,
    ) -> io::Result<CommittedPlanningUpdate> {
        if let Some(record) = self.planning.previous(&request)? {
            return Ok(record.clone());
        }
        authorize_new(&request)?;
        let update = PlanningUpdate {
            request,
            new_id: Uuid::now_v7(),
            at: Utc::now(),
        };
        let (next, outcome) = self.planning.prepare(&self.state, &update)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::PlanningUpdated(Box::new(update.clone()))])?;
        let committed = self.planning.apply(update, next, outcome, receipt.clone());
        self.cursor = receipt;
        Ok(committed)
    }
}
