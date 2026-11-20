//! Authenticated authored queue projection and original Flow edits.

use anyhow::Result;
use atman_proto::{JsonRpcError, inbox::*};
use atman_runtime::{
    flow_runtime::catalog::SessionCatalog,
    flow_store::{
        inbox::{
            DeliveryMode, InboxMove, OPERATOR_INBOX_MAX_MOVE, OPERATOR_INBOX_MAX_TEXT_BYTES,
            OperatorInboxChange, OperatorInboxEdit,
        },
        local_operator::LocalOperator,
    },
};

use super::{SessionService, controls, core_address, events, wire};
use crate::DaemonState;

pub(super) fn validate(request: &EditFlowInboxRequest) -> Result<(), JsonRpcError> {
    controls::validate_identity(request.request_id)?;
    controls::validate_identity(request.message_id)?;
    let revision = &request.expected_revision;
    if request.expected_head.branch.is_nil()
        || revision.journal_id.is_nil()
        || revision.sequence == 0
        || revision.digest.len() != 64
        || !revision.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(JsonRpcError::invalid_params(
            "inbox edits require the observed history head and message revision",
        ));
    }
    match &request.change {
        FlowInboxChange::EditText { text } if text.len() > OPERATOR_INBOX_MAX_TEXT_BYTES => Err(
            JsonRpcError::invalid_params("inbox edit text exceeds the supported size"),
        ),
        FlowInboxChange::Move { expected_order, .. }
            if !(2..=OPERATOR_INBOX_MAX_MOVE).contains(&expected_order.len()) =>
        {
            Err(JsonRpcError::invalid_params(
                "inbox move requires the complete observed next-turn order",
            ))
        }
        _ => Ok(()),
    }
}

fn delivery(value: DeliveryMode) -> FlowInboxDelivery {
    match value {
        DeliveryMode::NextTurn => FlowInboxDelivery::NextTurn,
        DeliveryMode::AtBoundary => FlowInboxDelivery::AtBoundary,
    }
}

pub(super) async fn snapshot(
    state: &DaemonState,
    request: ReadFlowInboxRequest,
) -> Result<serde_json::Value> {
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let page = catalog
        .operator_inbox_page(
            &core_address(request.flow.clone()),
            LocalOperator::new(state.operator_id()),
            request.offset,
        )
        .await?;
    Ok(serde_json::to_value(ReadFlowInboxResponse {
        flow: request.flow,
        cursor: wire::cursor(&page.cursor)?,
        head: wire::head(&page.head),
        items: page
            .items
            .into_iter()
            .map(|item| {
                Ok::<_, std::io::Error>(FlowInboxItem {
                    id: item.id,
                    revision: wire::cursor(&item.revision)?,
                    text: item.text,
                    text_truncated: item.text_truncated,
                    image_count: item.image_count,
                    has_quote: item.has_quote,
                    delivery: delivery(item.delivery),
                    held: item.held,
                    separate_turn: item.separate_turn,
                })
            })
            .collect::<Result<_, _>>()?,
        next_turn_lanes: page.next_turn_lanes,
        offset: page.offset,
        total: page.total,
        edits_available: page.edits_available && !state.is_stopping(),
        insertion_available: page.insertion_available && !state.is_stopping(),
    })?)
}

impl SessionService {
    pub(super) async fn edit_inbox(
        &self,
        request: EditFlowInboxRequest,
    ) -> Result<serde_json::Value> {
        let _operation = self.host.retain_host_operation()?;
        let address = core_address(request.flow.clone());
        let session = self.session(address.session_id.0)?;
        let _owner = session.retain_operation().await?;
        let worker = self.host.flow(&address).await?;
        let change = match request.change {
            FlowInboxChange::Cancel => OperatorInboxChange::Cancel,
            FlowInboxChange::EditText { text } => OperatorInboxChange::EditText(text),
            FlowInboxChange::SetDelivery { delivery } => {
                OperatorInboxChange::SetDelivery(match delivery {
                    FlowInboxDelivery::NextTurn => DeliveryMode::NextTurn,
                    FlowInboxDelivery::AtBoundary => DeliveryMode::AtBoundary,
                })
            }
            FlowInboxChange::Move {
                direction,
                expected_order,
            } => OperatorInboxChange::Move {
                direction: match direction {
                    FlowInboxMove::Earlier => InboxMove::Earlier,
                    FlowInboxMove::Later => InboxMove::Later,
                },
                expected_order,
            },
        };
        let receipt = worker
            .edit_operator_inbox(
                LocalOperator::new(self.operator),
                OperatorInboxEdit {
                    expected_head: wire::core_head(request.expected_head),
                    id: request.request_id,
                    message: request.message_id,
                    expected: events::receipt(request.expected_revision).into(),
                    change,
                },
            )
            .await?;
        Ok(serde_json::to_value(EditFlowInboxResponse {
            flow: request.flow,
            request_id: request.request_id,
            message_id: request.message_id,
            receipt: wire::cursor(&receipt)?,
        })?)
    }
}
