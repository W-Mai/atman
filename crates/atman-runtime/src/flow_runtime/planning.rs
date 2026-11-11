//! Serialized planning updates and shared instance projections.

use std::io;

use super::FlowWorker;
use crate::flow_store::planning::{CommittedPlanningUpdate, PlanningRequest, PlanningSnapshot};

impl FlowWorker {
    pub async fn planning(&self) -> io::Result<PlanningSnapshot> {
        self.request(|store| Ok(store.planning().clone())).await
    }

    /// Authenticate before enqueueing; authorize new writes at the storage commit boundary.
    pub async fn edit_planning(
        &self,
        request: PlanningRequest,
        authorize_new: impl FnOnce(&PlanningRequest) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommittedPlanningUpdate> {
        let context = self.context.clone();
        self.request(move |store| {
            let previous = store.cursor().sequence();
            let committed = store.edit_planning(request, authorize_new)?;
            if committed.receipt.sequence() > previous {
                context.project_instance_planning(store.planning());
            }
            Ok(committed)
        })
        .await
    }
}

impl crate::tool::ToolCtx {
    pub(crate) async fn edit_planning(
        &self,
        edit: crate::flow_store::planning::PlanningEdit,
    ) -> Result<Option<crate::flow_store::planning::PlanningOutcome>, crate::RuntimeError> {
        let Some(client) = self.instance_client()? else {
            return Ok(None);
        };
        client
            .edit_planning(uuid::Uuid::now_v7(), None, edit)
            .await
            .map(|record| Some(record.outcome))
            .map_err(|error| crate::RuntimeError::ToolFailed(format!("planning: {error}")))
    }
}
