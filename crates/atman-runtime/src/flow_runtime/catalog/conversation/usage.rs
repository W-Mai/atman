use std::{io, num::NonZeroUsize, sync::Arc};

use crate::{
    event::AtmanUuid,
    flow_store::{
        FlowRef,
        branches::{
            ConversationCapacity, ConversationCapacityKind, ConversationItemId, HistoryHead,
            WorkUsage,
        },
        source_capture::SourceSelection,
    },
};

use super::{
    Observation, RecentConversationRequest, SessionCatalog,
    artifact::{MAX_READERS, READERS},
    read_step,
};

pub struct ConversationUsageRequest {
    pub source: SourceSelection,
    pub head: HistoryHead,
    pub item: ConversationItemId,
    pub lease: atman_rt::TurnLease<AtmanUuid>,
}

pub enum ConversationUsageResult {
    Ready(WorkUsage),
    Capacity(ConversationCapacity),
    SourceChanged,
    Unavailable,
}

impl SessionCatalog {
    pub async fn read_conversation_usage(
        &self,
        request: ConversationUsageRequest,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<ConversationUsageResult> {
        use ConversationUsageResult as Reply;

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
        let observation = Observation::default();
        let prepared = self
            .prepare_conversation(
                RecentConversationRequest {
                    flow: flow.clone(),
                    expected: Some(request.source),
                    expected_head: Some(request.head),
                    before: None,
                    limit: NonZeroUsize::MIN,
                    include_work_logs: false,
                    inherited_logs_after: None,
                },
                &permit,
                &observation.0,
                Some(request.lease),
            )
            .await;
        let mut source = match prepared {
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
        let reply = read_step(&permit, move || {
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            let value = match source.read_work_usage(request.item) {
                Ok(Ok(usage)) => Reply::Ready(usage),
                Ok(Err(capacity)) => Reply::Capacity(capacity),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Reply::Unavailable,
                Err(error) => return Err(error),
            };
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            Ok(value)
        })
        .await?;
        authorize(&flow)?;
        Ok(reply)
    }
}
