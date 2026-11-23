use atman_proto::conversation::{
    ConversationDiffLine, ConversationDiffLineKind as Kind, ConversationDiffRecord,
    ConversationDiffSource, ConversationWorkDiffs, ReadConversationDiffRequest,
    ReadConversationDiffResponse,
};
use atman_runtime::{
    flow_runtime::catalog::{ConversationDiffRequest, ConversationDiffResult},
    flow_store::branches::{
        ConversationDiffLineKind as OriginalKind, ConversationDiffSource as OriginalSource,
    },
};

use super::*;

pub(in crate::flow_service) fn validate(
    request: &ReadConversationDiffRequest,
) -> Result<(), JsonRpcError> {
    let locator = &request.locator;
    let receipt = &locator.receipt;
    if !(1..=64).contains(&request.limit)
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
            "diff requires an observed execution record and a bounded page",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) async fn read(
    state: &DaemonState,
    request: ReadConversationDiffRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    use ReadConversationDiffResponse as Reply;

    let locator = request.locator;
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let result = catalog
        .read_conversation_diff(
            ConversationDiffRequest {
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
            },
            state.conversation_artifacts.clone(),
            |_| Ok(()),
        )
        .await;
    let reply = match result {
        Ok(ConversationDiffResult::Ready(page)) => Reply::Ready {
            locator: Box::new(locator),
            offset: page.offset,
            next_offset: page.next_offset,
            total_lines: page.total_lines,
            lines: page
                .lines
                .into_iter()
                .map(|line| ConversationDiffLine {
                    kind: match line.kind {
                        OriginalKind::Header => Kind::Header,
                        OriginalKind::Hunk => Kind::Hunk,
                        OriginalKind::Added => Kind::Added,
                        OriginalKind::Removed => Kind::Removed,
                        OriginalKind::Context => Kind::Context,
                        OriginalKind::Meta => Kind::Meta,
                    },
                    before: line.before,
                    after: line.after,
                    text: line.text,
                    truncated: line.truncated,
                })
                .collect(),
        },
        Ok(ConversationDiffResult::Capacity(value)) => Reply::Capacity {
            kind: capacity_kind(value.kind),
            required_at_least: value.required_at_least,
            allowed: value.allowed,
        },
        Ok(ConversationDiffResult::SourceChanged) => Reply::SourceChanged,
        Ok(ConversationDiffResult::InvalidRange) => Reply::InvalidRange,
        Ok(ConversationDiffResult::Unavailable) | Err(_) => Reply::Unavailable,
    };
    serde_json::to_value(reply)
        .map_err(|_| JsonRpcError::internal("Diff response could not be encoded"))
}

pub(super) fn project(sources: Vec<OriginalSource>, truncated: bool) -> ConversationWorkDiffs {
    let entries = sources
        .into_iter()
        .map(|source| ConversationDiffSource {
            receipt: events::cursor(&source.receipt),
            record: ConversationDiffRecord {
                commit: source.record.commit,
                fact: source.record.fact,
                item: source.record.item,
            },
            lease: super::super::live::turn(source.lease),
            title: source.title,
        })
        .collect();
    ConversationWorkDiffs { entries, truncated }
}
