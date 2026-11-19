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

pub(super) async fn recover_input(
    state: &crate::DaemonState,
    request: HistoryRecoverInputRequest,
) -> Result<Value> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let value = catalog
        .recover_operator_history_input(
            &core_address(request.flow.clone()),
            LocalOperator::new(state.operator_id()),
            request.request_id,
            request.input,
            |_| Ok(()),
        )
        .await?;
    let head = super::wire::head(&value.head);
    anyhow::ensure!(
        head == request.applied_head,
        "Recovered input belongs to a different applied head"
    );
    // Bound the aggregate decoded attachment bytes and return no host paths or executable settings.
    let response = tokio::task::spawn_blocking(move || -> Result<_> {
        let images = value
            .retained_images(4 * 1024 * 1024)?
            .into_iter()
            .map(|bytes| atman_proto::InlineImage {
                data_base64: STANDARD.encode(bytes),
                name: None,
            })
            .collect();
        let (text, quote) = match value.content.presentation {
            Some(presentation) => (
                presentation.prompt,
                presentation.quote.map(|quote| quote.text),
            ),
            None => (value.content.text, None),
        };
        anyhow::ensure!(
            text.len()
                .saturating_add(quote.as_ref().map_or(0, String::len))
                <= 256 * 1024,
            "Recovered text exceeds its read limit"
        );
        Ok(HistoryRecoveredInput {
            flow: request.flow,
            request_id: value.operation,
            input: value.input,
            head,
            acceptance: super::wire::cursor(&value.acceptance)?,
            recoverable_inputs: value.recoverable_inputs,
            text,
            quote,
            images,
        })
    })
    .await??;
    Ok(serde_json::to_value(response)?)
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
