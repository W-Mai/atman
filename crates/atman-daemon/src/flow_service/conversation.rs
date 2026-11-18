//! Authenticated, read-only conversation pages for the local browser.

use std::num::NonZeroUsize;

use atman_proto::{
    JsonRpcError,
    conversation::{
        ConversationBefore, ConversationBody, ConversationCapacityKind as WireCapacity,
        ConversationItem, ConversationItemId, ConversationPage, ConversationPart, ConversationRole,
        ReadConversationRequest, ReadConversationResponse,
    },
    history::HistorySource,
};
use atman_runtime::{
    flow_runtime::catalog::SessionCatalog,
    flow_store::{
        branches::{
            ConversationCapacity, ConversationCapacityKind, ConversationEvidence,
            ConversationItemBody, ConversationItemId as CoreItemId, RecentConversationPage,
            RecentConversationRequest,
        },
        source_capture::SourceSelection,
    },
    message::{MessagePart, MessageRole},
    projection::flow::FlowRecordId,
};

use super::{address, core_address, events, wire};
use crate::DaemonState;

pub(super) async fn read(
    state: &DaemonState,
    wire: ReadConversationRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    let flow = wire.flow.clone();
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_recent_conversation(core_request(wire), |_| Ok(()))
        .await
        .map_err(|_| {
            JsonRpcError::application("Conversation is unavailable or its history changed")
        })?;
    let result = match result {
        Ok(page) => project(page),
        Err(value) => capacity(flow, value),
    };
    serde_json::to_value(result)
        .map_err(|_| JsonRpcError::internal("Conversation response could not be encoded"))
}

fn core_request(wire: ReadConversationRequest) -> RecentConversationRequest {
    let expected = wire.before.as_ref().map(|before| SourceSelection {
        flow: core_address(before.source.flow.clone()),
        branch: before.source.branch.into(),
        cursor: events::receipt(before.source.cursor.clone()),
    });
    RecentConversationRequest {
        flow: core_address(wire.flow.clone()),
        expected,
        expected_head: wire
            .before
            .as_ref()
            .map(|before| wire::core_head(before.head.clone())),
        before: wire.before.map(|before| CoreItemId {
            record: FlowRecordId {
                commit: before.item.commit,
                fact: before.item.fact,
                item: before.item.item,
            },
            message: before.item.message,
        }),
        limit: NonZeroUsize::new(wire.limit as usize).expect("validated page limit"),
    }
}

fn capacity(
    flow: atman_proto::flows::FlowAddress,
    value: ConversationCapacity,
) -> ReadConversationResponse {
    let kind = match value.kind {
        ConversationCapacityKind::SourceRecord => WireCapacity::SourceRecord,
        ConversationCapacityKind::DecodeWork => WireCapacity::DecodeWork,
        ConversationCapacityKind::RetainedIndex => WireCapacity::RetainedIndex,
        ConversationCapacityKind::BranchMetadata => WireCapacity::BranchMetadata,
        ConversationCapacityKind::SelectionWork => WireCapacity::SelectionWork,
        ConversationCapacityKind::PageSourceBytes => WireCapacity::PageSourceBytes,
        ConversationCapacityKind::ScanBytes => WireCapacity::ScanBytes,
        ConversationCapacityKind::ReadersBusy => WireCapacity::ReadersBusy,
    };
    ReadConversationResponse::Capacity {
        flow,
        kind,
        required_at_least: value.required_at_least,
        allowed: value.allowed,
    }
}

fn item_id(value: CoreItemId) -> ConversationItemId {
    ConversationItemId {
        commit: value.record.commit,
        fact: value.record.fact,
        item: value.record.item,
        message: value.message,
    }
}

fn project(page: RecentConversationPage) -> ReadConversationResponse {
    let boundary = page.boundary;
    let flow = address(&boundary.source.flow);
    let source = HistorySource {
        flow: flow.clone(),
        branch: *boundary.source.branch.as_uuid(),
        cursor: events::cursor(&boundary.source.cursor),
    };
    let head = wire::head(&boundary.observed_head);
    let before = page.before.map(|item| ConversationBefore {
        source: source.clone(),
        head: head.clone(),
        item: item_id(item),
    });
    let trailing_bytes = match boundary.evidence {
        ConversationEvidence::DiskPrefix { trailing_bytes } => trailing_bytes,
        ConversationEvidence::OwnerAcknowledged => 0,
    };
    let items = page
        .items
        .into_iter()
        .map(|item| ConversationItem {
            id: item_id(item.id),
            body: match item.body {
                ConversationItemBody::Deferred {
                    required_at_least,
                    allowed,
                } => ConversationBody::Deferred {
                    required_at_least,
                    allowed,
                },
                ConversationItemBody::Inline(message) => {
                    let role = match message.role {
                        MessageRole::User => ConversationRole::User,
                        MessageRole::Assistant => ConversationRole::Assistant,
                        MessageRole::System => ConversationRole::System,
                        MessageRole::Tool => ConversationRole::Tool,
                    };
                    let authored = message.input_presentation().cloned();
                    let mut parts = Vec::new();
                    if let Some(value) = &authored {
                        if let Some(quote) = &value.quote {
                            parts.push(ConversationPart::Quote {
                                text: quote.text.clone(),
                            });
                        }
                        parts.push(ConversationPart::Text {
                            text: value.prompt.clone(),
                        });
                    }
                    parts.extend(message.parts.into_iter().filter_map(|part| match part {
                        MessagePart::Text { text } if authored.is_none() => {
                            Some(ConversationPart::Text { text })
                        }
                        MessagePart::PeerInput { text, .. } => {
                            Some(ConversationPart::Text { text })
                        }
                        MessagePart::Thinking { thinking, .. } => {
                            Some(ConversationPart::Thinking { text: thinking })
                        }
                        MessagePart::ToolUse { name, .. } => {
                            Some(ConversationPart::ToolCall { name })
                        }
                        MessagePart::ToolResult { is_error, .. } => {
                            Some(ConversationPart::ToolResult { failed: is_error })
                        }
                        MessagePart::Image { .. } => Some(ConversationPart::Image),
                        _ => None,
                    }));
                    ConversationBody::Message {
                        role,
                        turn: message.turn_id.0.0,
                        parts,
                    }
                }
            },
        })
        .collect();
    ReadConversationResponse::Ready {
        page: Box::new(ConversationPage {
            flow,
            source,
            head,
            trailing_bytes,
            items,
            before,
        }),
    }
}
