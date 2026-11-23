//! Authenticated, read-only conversation pages for the local browser.

use std::num::NonZeroUsize;

use atman_proto::{
    JsonRpcError,
    conversation::{
        ConversationArtifact, ConversationBefore, ConversationBody,
        ConversationCapacityKind as WireCapacity, ConversationItem, ConversationItemId,
        ConversationPage, ConversationPart, ConversationRole, ConversationWorkMember,
        ConversationWorkRole, ReadConversationArtifactRequest, ReadConversationArtifactResponse,
        ReadConversationRequest, ReadConversationResponse,
    },
    history::HistorySource,
};
use atman_runtime::{
    flow_runtime::catalog::{
        ConversationArtifactRequest, ConversationArtifactResult, ConversationArtifactVerification,
        SessionCatalog,
    },
    flow_store::{
        branches::{
            ConversationCapacity, ConversationCapacityKind, ConversationEvidence,
            ConversationItemBody, ConversationItemId as CoreItemId,
            ConversationWorkRole as CoreWorkRole, RecentConversationPage,
            RecentConversationRequest,
        },
        source_capture::SourceSelection,
    },
    message::{MessagePart, MessageRole},
    projection::flow::FlowRecordId,
};

use super::{address, core_address, events, wire};
use crate::DaemonState;

const MAX_TOOL_PREVIEW_BYTES: usize = 2048;

pub(super) async fn read(
    state: &DaemonState,
    wire: ReadConversationRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    let flow = wire.flow.clone();
    let include_source_parts = wire.include_source_parts;
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_recent_conversation(core_request(wire), |_| Ok(()))
        .await
        .map_err(|_| {
            JsonRpcError::application("Conversation is unavailable or its history changed")
        })?;
    let result = match result {
        Ok(page) => project(page, include_source_parts),
        Err(value) => capacity(flow, value),
    };
    serde_json::to_value(result)
        .map_err(|_| JsonRpcError::internal("Conversation response could not be encoded"))
}

pub(super) fn validate_artifact(
    request: &ReadConversationArtifactRequest,
) -> Result<(), JsonRpcError> {
    let locator = &request.locator;
    if !(4..=32768).contains(&request.limit)
        || request
            .verification
            .as_ref()
            .is_some_and(|step| step.id.is_nil())
        || locator.source.branch.is_nil()
        || locator.head.branch.is_nil()
        || locator.source.cursor.journal_id.is_nil()
        || locator.item.commit == 0
        || locator.item.commit > locator.source.cursor.sequence
        || !locator.id.strip_prefix("out_").is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(JsonRpcError::invalid_params(
            "artifact range requires an observed conversation result and a bounded byte limit",
        ));
    }
    Ok(())
}

pub(super) async fn artifact(
    state: &DaemonState,
    request: ReadConversationArtifactRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    use ReadConversationArtifactResponse as Reply;
    let locator = request.locator;
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_conversation_artifact(
            ConversationArtifactRequest {
                source: SourceSelection {
                    flow: core_address(locator.source.flow.clone()),
                    branch: locator.source.branch.into(),
                    cursor: events::receipt(locator.source.cursor.clone()),
                },
                head: wire::core_head(locator.head.clone()),
                item: CoreItemId {
                    record: FlowRecordId {
                        commit: locator.item.commit,
                        fact: locator.item.fact,
                        item: locator.item.item,
                    },
                    message: locator.item.message,
                },
                part: locator.part,
                id: locator.id.clone(),
                offset: request.offset,
                limit: request.limit as usize,
                verification: request.verification.map(|step| {
                    if step.cancel {
                        ConversationArtifactVerification::Cancel(step.id)
                    } else {
                        ConversationArtifactVerification::Read(step.id)
                    }
                }),
            },
            state.conversation_artifacts.clone(),
            |_| Ok(()),
        )
        .await;
    let reply = match result {
        Ok(ConversationArtifactResult::Ready(range)) => Reply::Ready {
            locator: Box::new(locator),
            content: range.content,
            offset: range.offset,
            next_offset: range.next_offset,
            total_bytes: range.total_bytes,
            has_more: range.has_more,
        },
        Ok(ConversationArtifactResult::Capacity(value)) => Reply::Capacity {
            kind: capacity_kind(value.kind),
            required_at_least: value.required_at_least,
            allowed: value.allowed,
        },
        Ok(ConversationArtifactResult::VerificationBudget {
            required,
            allowed,
            progressive,
        }) => Reply::VerificationBudget {
            required,
            allowed,
            progressive,
        },
        Ok(ConversationArtifactResult::Verifying {
            verified_bytes,
            total_bytes,
        }) => Reply::Verifying {
            locator: Box::new(locator),
            verified_bytes,
            total_bytes,
        },
        Ok(ConversationArtifactResult::VerificationCancelled) => Reply::VerificationCancelled,
        Ok(ConversationArtifactResult::SourceChanged) => Reply::SourceChanged,
        Ok(ConversationArtifactResult::ContentChanged) => Reply::ContentChanged,
        Ok(ConversationArtifactResult::InvalidRange) => Reply::InvalidRange,
        Ok(ConversationArtifactResult::Unavailable) | Err(_) => Reply::Unavailable,
    };
    serde_json::to_value(reply)
        .map_err(|_| JsonRpcError::internal("Artifact response could not be encoded"))
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
    ReadConversationResponse::Capacity {
        flow,
        kind: capacity_kind(value.kind),
        required_at_least: value.required_at_least,
        allowed: value.allowed,
    }
}

fn capacity_kind(kind: ConversationCapacityKind) -> WireCapacity {
    match kind {
        ConversationCapacityKind::SourceRecord => WireCapacity::SourceRecord,
        ConversationCapacityKind::DecodeWork => WireCapacity::DecodeWork,
        ConversationCapacityKind::RetainedIndex => WireCapacity::RetainedIndex,
        ConversationCapacityKind::BranchMetadata => WireCapacity::BranchMetadata,
        ConversationCapacityKind::SelectionWork => WireCapacity::SelectionWork,
        ConversationCapacityKind::PageSourceBytes => WireCapacity::PageSourceBytes,
        ConversationCapacityKind::ScanBytes => WireCapacity::ScanBytes,
        ConversationCapacityKind::ReadersBusy => WireCapacity::ReadersBusy,
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

fn tool_result(
    mut content: String,
    failed: bool,
    artifact: Option<ConversationArtifact>,
) -> ConversationPart {
    let mut end = content.len().min(MAX_TOOL_PREVIEW_BYTES);
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let preview_truncated = end < content.len();
    content.truncate(end);
    ConversationPart::ToolResult {
        failed,
        preview: (!content.is_empty()).then_some(content),
        preview_truncated,
        artifact,
    }
}

fn project(page: RecentConversationPage, include_source_parts: bool) -> ReadConversationResponse {
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
        .map(|item| {
            project_item(
                item,
                include_source_parts,
                include_source_parts.then_some(&flow),
            )
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

pub(super) fn project_item(
    item: atman_runtime::flow_store::branches::ConversationItem,
    include_source_parts: bool,
    approval_flow: Option<&atman_proto::flows::FlowAddress>,
) -> ConversationItem {
    let mut source_parts = None;
    let body = match item.body {
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
            source_parts = include_source_parts.then(Vec::new);
            let mut append = |part, ordinal| {
                parts.push(part);
                if let Some(ordinals) = &mut source_parts {
                    ordinals.push(ordinal);
                }
            };
            if let Some(value) = &authored {
                if let Some(quote) = &value.quote {
                    append(
                        ConversationPart::Quote {
                            text: quote.text.clone(),
                        },
                        None,
                    );
                }
                append(
                    ConversationPart::Text {
                        text: value.prompt.clone(),
                    },
                    None,
                );
            }
            for (ordinal, part) in message.parts.into_iter().enumerate() {
                let part = match part {
                    MessagePart::Text { text } if authored.is_none() => {
                        ConversationPart::Text { text }
                    }
                    MessagePart::PeerInput { text, .. } => ConversationPart::Text { text },
                    MessagePart::Thinking { thinking, .. } => {
                        ConversationPart::Thinking { text: thinking }
                    }
                    MessagePart::ToolUse { name, id, .. } => ConversationPart::ToolCall {
                        name,
                        approval_key: item
                            .work
                            .as_ref()
                            .filter(|work| {
                                approval_flow.is_some_and(|flow| address(&work.lease.flow) == *flow)
                            })
                            .and_then(|work| work.publication_run.as_ref())
                            .and_then(|run| {
                                super::approvals::tool_call_key(approval_flow?, run.0.0, &id)
                            }),
                    },
                    MessagePart::ToolResult {
                        content, is_error, ..
                    } => {
                        let artifact = include_source_parts
                            .then(|| {
                                item.tool_artifacts
                                    .iter()
                                    .find(|artifact| artifact.part as usize == ordinal)
                            })
                            .flatten()
                            .map(|artifact| ConversationArtifact {
                                id: artifact.output.id.clone(),
                                bytes: artifact.output.bytes,
                            });
                        tool_result(content, is_error, artifact)
                    }
                    MessagePart::Image { .. } => ConversationPart::Image,
                    _ => continue,
                };
                append(part, Some(ordinal));
            }
            ConversationBody::Message {
                role,
                turn: message.turn_id.0.0,
                parts,
            }
        }
    };
    ConversationItem {
        id: item_id(item.id),
        body,
        source_parts,
        user_turn: item.user_turn.as_ref().map(super::history_wire::cutoff),
        work: item
            .work
            .filter(|_| include_source_parts)
            .map(|work| ConversationWorkMember {
                lease: super::live::turn(work.lease),
                role: match work.role {
                    CoreWorkRole::Invocation => ConversationWorkRole::Invocation,
                    CoreWorkRole::Work => ConversationWorkRole::Work,
                    CoreWorkRole::FinalAnswer => ConversationWorkRole::FinalAnswer,
                },
                publication: work.publication,
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_preview_clips_without_splitting_utf8_and_keeps_original_status() {
        let prefix = "a".repeat(MAX_TOOL_PREVIEW_BYTES - 1);
        let content = format!("{prefix}🦀suffix");
        assert_eq!(
            serde_json::to_value(tool_result(content, true, None)).unwrap(),
            serde_json::json!({
                "kind": "tool_result", "failed": true, "preview": prefix, "preview_truncated": true,
            })
        );
        assert_eq!(
            serde_json::to_value(tool_result(String::new(), false, None)).unwrap(),
            serde_json::json!({"kind": "tool_result", "failed": false})
        );
        let literal = "<script>text</script> out_fixture";
        assert_eq!(
            serde_json::to_value(tool_result(literal.into(), false, None)).unwrap()["preview"],
            literal
        );
    }
}
