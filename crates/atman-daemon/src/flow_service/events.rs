use std::num::NonZeroUsize;

use atman_proto::flows::{FlowCursor, ReadFlowEventsRequest};
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
        Ok(serde_json::to_value(
            worker
                .read_updates(
                    request.after.map(|position| receipt(position).into()),
                    NonZeroUsize::new(request.limit).expect("validated event page limit"),
                )
                .await?,
        )?)
    }
}
