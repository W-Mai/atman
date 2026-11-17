//! History intents and receipts use the original native owner and authenticated operator.

use super::{SessionService, core_address, history_wire};
use anyhow::Result;
use atman_proto::{history::*, methods::Method};
use atman_runtime::{
    flow_runtime::catalog::SessionCatalog, flow_store::local_operator::LocalOperator,
};
use serde_json::Value;

impl SessionService {
    pub(super) async fn history_branches(&self, request: HistoryBranchesRequest) -> Result<Value> {
        let worker = self.host.flow(&core_address(request.flow)).await?;
        let page = worker
            .history_branches(
                super::wire::core_head(request.expected_head),
                request.offset,
                request.limit,
            )
            .await?;
        Ok(serde_json::to_value(history_wire::branches(page))?)
    }

    pub(super) async fn history_review(&self, request: HistoryReviewRequest) -> Result<Value> {
        let worker = self.host.flow(&core_address(request.flow.clone())).await?;
        Ok(serde_json::to_value(history_wire::review(
            worker
                .review_operator_history_operation(
                    LocalOperator::new(self.operator),
                    history_wire::intent(request, self.operator),
                    |_| Ok(()),
                )
                .await?,
        )?)?)
    }

    pub(super) async fn history_begin(&self, request: HistoryOperationRequest) -> Result<Value> {
        let worker = self.host.flow(&core_address(request.flow.clone())).await?;
        Ok(serde_json::to_value(history_wire::record(
            worker
                .begin_operator_history_operation(
                    LocalOperator::new(self.operator),
                    history_wire::command(request, self.operator),
                    |_| Ok(()),
                )
                .await?,
        )?)?)
    }

    pub(super) async fn history_action(
        &self,
        method: Method,
        request: HistoryOperationQuery,
    ) -> Result<Value> {
        let worker = self.host.flow(&core_address(request.flow)).await?;
        let operator = LocalOperator::new(self.operator);
        let record = match method {
            Method::FlowHistoryStop => {
                worker
                    .stop_operator_history_operation(operator, request.request_id, |_| Ok(()))
                    .await?
            }
            Method::FlowHistoryApply => {
                worker
                    .apply_operator_history_operation(operator, request.request_id, |_| Ok(()))
                    .await?
            }
            Method::FlowHistoryAbort => {
                worker
                    .abort_operator_history_operation(operator, request.request_id, |_| Ok(()))
                    .await?
            }
            _ => anyhow::bail!("unsupported history action"),
        };
        Ok(serde_json::to_value(history_wire::record(record)?)?)
    }
}

pub(super) async fn status(
    state: &crate::DaemonState,
    request: HistoryOperationQuery,
) -> Result<Value> {
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let operation = catalog
        .observe_operator_history_operation(
            &core_address(request.flow.clone()),
            LocalOperator::new(state.operator_id()),
            request.request_id,
            |_| Ok(()),
        )
        .await?;
    Ok(serde_json::to_value(HistoryStatusResponse {
        flow: request.flow,
        request_id: request.request_id,
        operation: operation.map(history_wire::record).transpose()?,
    })?)
}
