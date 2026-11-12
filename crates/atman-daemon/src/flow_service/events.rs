use std::num::NonZeroUsize;

use atman_proto::flows::{FlowCursor, ReadFlowEventsRequest};
use atman_proto::responses::{FlowEventBatch, FlowEventPage};
use atman_runtime::event_log::journal::CommitReceipt;

use super::{SessionService, core_address};

pub(crate) fn receipt(cursor: FlowCursor) -> CommitReceipt {
    CommitReceipt {
        journal_id: cursor.journal_id,
        sequence: cursor.sequence,
        digest: cursor.digest,
    }
}

pub(crate) fn cursor(receipt: &CommitReceipt) -> FlowCursor {
    FlowCursor {
        journal_id: receipt.journal_id,
        sequence: receipt.sequence,
        digest: receipt.digest.clone(),
    }
}

impl SessionService {
    pub(super) async fn events(
        &self,
        request: ReadFlowEventsRequest,
    ) -> anyhow::Result<serde_json::Value> {
        let worker = self.host.flow(&core_address(request.flow)).await?;
        let page = worker
            .read_updates(
                request.after.map(|position| receipt(position).into()),
                NonZeroUsize::new(request.limit).expect("validated event page limit"),
            )
            .await?;
        let response = FlowEventPage {
            batches: page
                .batches
                .into_iter()
                .map(|batch| {
                    Ok::<_, anyhow::Error>(FlowEventBatch {
                        receipt: super::wire::cursor(&batch.receipt)?,
                        facts: batch
                            .facts
                            .into_iter()
                            .map(serde_json::to_value)
                            .collect::<Result<Vec<_>, _>>()?,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
            next: page.next.as_ref().map(super::wire::cursor).transpose()?,
            has_more: page.has_more,
        };
        Ok(serde_json::to_value(response)?)
    }
}
