//! Query the original committed execution patch without an execution owner.

use crate::{
    event_log::journal::CommitReceipt, flow_store::branches::ConversationDiffPage,
    projection::flow::FlowRecordId,
};

use super::*;

pub struct ConversationDiffRequest {
    pub source: SourceSelection,
    pub head: HistoryHead,
    pub record: FlowRecordId,
    pub receipt: CommitReceipt,
    pub offset: usize,
    pub limit: usize,
}

pub enum ConversationDiffResult {
    Ready(ConversationDiffPage),
    Capacity(ConversationCapacity),
    SourceChanged,
    InvalidRange,
    Unavailable,
}

impl SessionCatalog {
    pub async fn read_conversation_diff(
        &self,
        request: ConversationDiffRequest,
        cache: Arc<ConversationArtifactSources>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<ConversationDiffResult> {
        use ConversationDiffResult as Reply;

        let flow = request.source.flow.clone();
        authorize(&flow)?;
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(Reply::Capacity(ConversationCapacity {
                kind: ConversationCapacityKind::ReadersBusy,
                required_at_least: MAX_READERS + 1,
                allowed: MAX_READERS,
                restart_required: false,
            }));
        };
        let permit = Arc::new(permit);
        let root = self.recorded_root()?;
        let observation = Observation::default();
        let mut source = match self
            .prepare_record_source(
                &cache,
                &root,
                &request.source,
                &request.head,
                &permit,
                &observation,
            )
            .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(capacity)) => return Ok(Reply::Capacity(capacity)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(Reply::SourceChanged);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = authorize(&flow) {
            crate::read_work::retire((source, Arc::clone(&permit)));
            return Err(error);
        }
        let result = read_step(&permit, move || {
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            let result = match source.read_diff_record(
                request.record,
                &request.receipt,
                request.offset,
                request.limit,
            ) {
                Ok(Ok(page)) => Reply::Ready(page),
                Ok(Err(capacity)) => Reply::Capacity(capacity),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Reply::Unavailable,
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => Reply::InvalidRange,
                Err(error) => return Err(error),
            };
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            cache.retain(root, source);
            Ok(result)
        })
        .await?;
        authorize(&flow)?;
        Ok(result)
    }
}
