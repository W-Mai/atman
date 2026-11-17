//! Goal changes use the instance's serialized writer, independently of execution control.

use std::io;

use super::FlowWorker;
use crate::{
    event::AtmanUuid,
    flow_store::goal::{CommittedGoalUpdate, GoalRequest},
    memory::goal::GoalRecord,
};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct GoalView {
    pub goal: Option<GoalRecord>,
    #[serde(with = "crate::flow_store::execution::instance_wire")]
    pub execution: atman_rt::FlowInstance<AtmanUuid>,
    pub history_head: crate::flow_store::branches::HistoryHead,
}

impl FlowWorker {
    /// Reconcile the original local operator receipt before applying a new goal change.
    pub async fn edit_operator_goal(
        &self,
        operator: crate::flow_store::local_operator::LocalOperator,
        request: GoalRequest,
        authorize_new: impl FnOnce(&GoalRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedGoalUpdate> {
        self.edit_goal_as(Some(operator), None, request, authorize_new)
            .await
    }

    /// Observe the original goal update from one serialized owner snapshot.
    pub async fn operator_goal_update(
        &self,
        operator: crate::flow_store::local_operator::LocalOperator,
        id: uuid::Uuid,
    ) -> io::Result<Option<CommittedGoalUpdate>> {
        self.request(move |store| store.operator_goal_update(operator, id))
            .await
    }

    pub async fn goal(&self) -> io::Result<GoalView> {
        self.request(|store| {
            Ok(GoalView {
                goal: store.goal().cloned(),
                execution: store.state().clone(),
                history_head: store.history_head(),
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
        self.edit_goal_as(None, None, request, authorize_new).await
    }

    pub(crate) async fn edit_goal_with_authority(
        &self,
        authority: std::sync::Arc<crate::peer_authority::AuthorityGuard>,
        request: GoalRequest,
        authorize_new: impl FnOnce(&GoalRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedGoalUpdate> {
        self.edit_goal_as(None, Some(authority), request, authorize_new)
            .await
    }

    async fn edit_goal_as(
        &self,
        operator: Option<crate::flow_store::local_operator::LocalOperator>,
        authority: Option<std::sync::Arc<crate::peer_authority::AuthorityGuard>>,
        request: GoalRequest,
        authorize_new: impl FnOnce(&GoalRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedGoalUpdate> {
        let context = self.context.clone();
        let command = move |store: &mut crate::flow_store::FlowStore| {
            let previous = store.cursor().sequence();
            let record = match operator {
                Some(operator) => store.edit_operator_goal(operator, request, authorize_new)?,
                None => store.edit_goal(request, authorize_new)?,
            };
            if record.receipt.sequence() > previous {
                context.project_instance_goal(record.goal.clone());
            }
            Ok(record)
        };
        match authority {
            Some(authority) => self.request_with_authority(authority, command).await,
            None => self.request(command).await,
        }
    }
}
