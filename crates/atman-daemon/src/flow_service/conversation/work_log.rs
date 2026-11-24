use atman_proto::conversation::{
    ConversationInheritedLog, ConversationInheritedLogs, ConversationLogRecord,
    ConversationLogSource, ConversationWorkLogs, ReadConversationLogRequest,
    ReadConversationLogResponse,
};
use atman_runtime::{
    flow_runtime::catalog::ConversationLogRequest,
    flow_store::branches::{ConversationLogSource as OriginalSource, InheritedLogSource},
};

use super::*;

pub(in crate::flow_service) fn validate(
    request: &ReadConversationLogRequest,
) -> Result<(), JsonRpcError> {
    let locator = &request.locator;
    let receipt = &locator.receipt;
    if !(4..=32768).contains(&request.limit)
        || request.verification.id.is_nil()
        || locator.source.branch.is_nil()
        || locator.source.branch != locator.head.branch
        || receipt.journal_id.is_nil()
        || receipt.journal_id != locator.source.cursor.journal_id
        || receipt.sequence == 0
        || receipt.sequence != locator.record.commit
        || receipt.sequence > locator.source.cursor.sequence
        || receipt.digest.len() != 64
        || !receipt.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(JsonRpcError::invalid_params(
            "log requires an observed checkpoint, verification ID, and bounded byte range",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) async fn read(
    state: &DaemonState,
    request: ReadConversationLogRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    use ReadConversationLogResponse as Reply;

    let locator = request.locator;
    let verification = if request.verification.cancel {
        ConversationArtifactVerification::Cancel(request.verification.id)
    } else {
        ConversationArtifactVerification::Read(request.verification.id)
    };
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_conversation_log(
            ConversationLogRequest {
                source: core_source(&locator.source),
                head: wire::core_head(locator.head.clone()),
                record: FlowRecordId {
                    commit: locator.record.commit,
                    fact: locator.record.fact,
                    item: locator.record.item,
                },
                receipt: events::receipt(locator.receipt.clone()),
                offset: request.offset,
                limit: request.limit as usize,
                verification,
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
        Ok(ConversationArtifactResult::Verifying {
            verified_bytes,
            total_bytes,
        }) => Reply::Verifying {
            locator: Box::new(locator),
            verified_bytes,
            total_bytes,
        },
        Ok(ConversationArtifactResult::Capacity(value)) => Reply::Capacity {
            kind: capacity_kind(value.kind),
            required_at_least: value.required_at_least,
            allowed: value.allowed,
        },
        Ok(ConversationArtifactResult::VerificationBudget {
            required, allowed, ..
        }) => Reply::VerificationBudget { required, allowed },
        Ok(ConversationArtifactResult::VerificationCancelled) => Reply::VerificationCancelled,
        Ok(ConversationArtifactResult::SourceChanged) => Reply::SourceChanged,
        Ok(ConversationArtifactResult::ContentChanged) => Reply::ContentChanged,
        Ok(ConversationArtifactResult::InvalidRange) => Reply::InvalidRange,
        Ok(ConversationArtifactResult::Unavailable) | Err(_) => Reply::Unavailable,
    };
    serde_json::to_value(reply)
        .map_err(|_| JsonRpcError::internal("Log response could not be encoded"))
}

pub(super) fn project(sources: Vec<OriginalSource>, truncated: bool) -> ConversationWorkLogs {
    let entries = sources
        .into_iter()
        .map(|source| ConversationLogSource {
            receipt: events::cursor(&source.receipt),
            record: ConversationLogRecord {
                commit: source.record.commit,
                fact: source.record.fact,
                item: source.record.item,
            },
            lease: super::super::live::turn(source.lease),
            handle: source.handle,
            bytes: source.bytes,
            drained: source.drained,
        })
        .collect();
    ConversationWorkLogs { entries, truncated }
}

pub(super) fn project_inherited(
    sources: Vec<InheritedLogSource>,
    truncated: bool,
) -> ConversationInheritedLogs {
    let entries = sources
        .into_iter()
        .map(|source| ConversationInheritedLog {
            receipt: events::cursor(&source.receipt),
            record: ConversationLogRecord {
                commit: source.record.commit,
                fact: source.record.fact,
                item: source.record.item,
            },
            original: address(&source.original),
            handle: source.handle,
            bytes: source.bytes,
        })
        .collect();
    ConversationInheritedLogs { entries, truncated }
}
