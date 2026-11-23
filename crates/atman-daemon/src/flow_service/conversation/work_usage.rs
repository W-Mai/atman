use atman_proto::conversation::{
    ConversationUsageEdits, ConversationUsageRoute, ConversationUsageSource,
    ReadConversationUsageRequest, ReadConversationUsageResponse,
};
use atman_runtime::{
    flow_runtime::catalog::{ConversationUsageRequest, ConversationUsageResult},
    flow_store::branches::WorkUsageSource,
};

use super::*;

pub(in crate::flow_service) fn validate(
    request: &ReadConversationUsageRequest,
) -> Result<(), JsonRpcError> {
    let locator = &request.locator;
    let cursor = &locator.source.cursor;
    if locator.source.flow != locator.lease.flow
        || locator.lease.turn.is_nil()
        || locator.lease.run.is_nil()
        || locator.source.branch.is_nil()
        || locator.source.branch != locator.head.branch
        || cursor.journal_id != locator.source.flow.instance_id
        || cursor.digest.len() != 64
        || !cursor.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        || locator.item.item != 0
        || locator.item.commit == 0
        || locator.item.commit > cursor.sequence
    {
        return Err(JsonRpcError::invalid_params(
            "usage requires an observed conversation row and its invocation",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) async fn read(
    state: &DaemonState,
    request: ReadConversationUsageRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    use ReadConversationUsageResponse as Reply;

    let locator = request.locator;
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_conversation_usage(
            ConversationUsageRequest {
                source: core_source(&locator.source),
                head: wire::core_head(locator.head.clone()),
                item: core_item(locator.item),
                lease: wire::core_turn(locator.lease.clone()),
            },
            |_| Ok(()),
        )
        .await;
    let reply = match result {
        Ok(ConversationUsageResult::Ready(usage)) => Reply::Ready {
            locator: Box::new(locator),
            routes: usage
                .routes
                .into_iter()
                .map(|(route, totals)| ConversationUsageRoute {
                    provider: route.provider,
                    model: route.model,
                    purpose: route.purpose.map(str::to_owned),
                    scope: route.scope.map(str::to_owned),
                    source: match route.source {
                        WorkUsageSource::Provider => ConversationUsageSource::Provider,
                        WorkUsageSource::Mixed => ConversationUsageSource::Mixed,
                        WorkUsageSource::Estimated => ConversationUsageSource::Estimated,
                        WorkUsageSource::Unknown => ConversationUsageSource::Unknown,
                    },
                    calls: totals.calls,
                    failures: totals.failures,
                    input: totals.tokens.prompt_input(),
                    output: totals.tokens.output,
                    cache_read: totals.tokens.cached_input,
                    cache_read_calls: totals.cache_read_calls,
                    cache_write: totals.tokens.cache_write,
                    reasoning: totals.tokens.reasoning_tokens,
                })
                .collect(),
            edits: ConversationUsageEdits {
                applied: usage.edits.applied_edits,
                files: usage.edits.files,
                hunks: usage.edits.hunks,
                insertions: usage.edits.insertions,
                deletions: usage.edits.deletions,
            },
            interrupted_calls: usage.interrupted_calls,
        },
        Ok(ConversationUsageResult::Capacity(value)) => Reply::Capacity {
            kind: capacity_kind(value.kind),
            required_at_least: value.required_at_least,
            allowed: value.allowed,
        },
        Ok(ConversationUsageResult::SourceChanged) => Reply::SourceChanged,
        Ok(ConversationUsageResult::Unavailable) | Err(_) => Reply::Unavailable,
    };
    serde_json::to_value(reply)
        .map_err(|_| JsonRpcError::internal("Usage response could not be encoded"))
}
