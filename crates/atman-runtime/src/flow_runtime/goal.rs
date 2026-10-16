//! Goal changes use the instance's serialized writer, independently of execution control.

use std::io;

use super::FlowWorker;
use crate::{
    event::AtmanUuid,
    flow_store::goal::{CommittedGoalUpdate, GoalRequest},
    memory::goal::GoalRecord,
};

#[derive(Clone, Debug)]
pub struct GoalView {
    pub goal: Option<GoalRecord>,
    pub execution: atman_rt::FlowInstance<AtmanUuid>,
}

impl FlowWorker {
    pub async fn goal(&self) -> io::Result<GoalView> {
        self.request(|store| {
            Ok(GoalView {
                goal: store.goal().cloned(),
                execution: store.state().clone(),
            })
        })
        .await
    }

    /// The trusted host authenticates the sender; the callback authorizes only new changes.
    pub async fn edit_goal(
        &self,
        request: GoalRequest,
        authorize_new: impl FnOnce(&GoalRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedGoalUpdate> {
        let context = self.context.clone();
        self.request(move |store| {
            let previous = store.cursor().sequence;
            let record = store.edit_goal(request, authorize_new)?;
            if record.receipt.sequence > previous {
                context.project_instance_goal(record.goal.clone());
            }
            Ok(record)
        })
        .await
    }
}
