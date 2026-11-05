//! Bounded preparation retained by the view's original source.

use std::{
    io,
    sync::{Arc, OnceLock},
    time::Instant,
};

use tokio::{
    sync::{Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use atman_runtime::{Session, tools::tool_output::OutputStore};

use super::{FlowViewContent, FlowViewSource};
use crate::{
    flow_view::FlowViewModel,
    loading::{ViewLoading, ViewPhase},
};

static PREPARATION_LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();

struct PreparedSource {
    source: FlowViewSource,
    result: io::Result<(Option<Box<FlowViewModel>>, Vec<String>)>,
}

/// Background history preparation owned by one retained view source.
pub struct ViewPreparation {
    pub(super) address: crate::control::ViewAddress,
    task: Option<JoinHandle<io::Result<PreparedSource>>>,
    phase: watch::Receiver<ViewPhase>,
    started_at: Instant,
    cancellation: CancellationToken,
    offset: usize,
    failure: Option<String>,
}

impl Drop for ViewPreparation {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl FlowViewSource {
    pub(crate) fn prepare_initial(
        self,
        session: Option<Arc<Session>>,
        output: Option<Arc<OutputStore>>,
        build_document: bool,
    ) -> Self {
        if matches!(self, Self::Preparing(_)) {
            return self;
        }
        let limit = PREPARATION_LIMIT
            .get_or_init(|| Arc::new(Semaphore::new(2)))
            .clone();
        self.prepare_with_limit(session, output, build_document, limit)
    }

    pub(crate) fn prepare_with_limit(
        self,
        session: Option<Arc<Session>>,
        output: Option<Arc<OutputStore>>,
        build_document: bool,
        limit: Arc<Semaphore>,
    ) -> Self {
        let address = self.address();
        let (phase, receiver) = watch::channel(ViewPhase::Waiting);
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::spawn(async move {
            let permit = tokio::select! {
                _ = cancelled.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "view preparation cancelled")),
                permit = limit.acquire_owned() => permit.map_err(io::Error::other)?,
            };
            tokio::task::spawn_blocking(move || {
                // The permit remains held until the blocking work actually exits.
                let _permit = permit;
                let mut source = self;
                let result = runtime.block_on(async {
                    phase.send_replace(ViewPhase::Reading);
                    let content = tokio::select! {
                        _ = cancelled.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "view preparation cancelled")),
                        content = source.refresh_content() => content?,
                    };
                    phase.send_replace(ViewPhase::Building);
                    let document = if build_document {
                        let mut document = FlowViewModel::default();
                        document.sync_content(content, output.as_deref())?;
                        Some(Box::new(document))
                    } else { None };
                    if cancelled.is_cancelled() { return Err(io::Error::new(io::ErrorKind::Interrupted, "view preparation cancelled")); }
                    let history = if build_document {
                        session.map(|session| session.messages_full().iter()
                            .filter(|message| message.role == atman_runtime::message::MessageRole::User
                                && message.origin == atman_runtime::message::MessageOrigin::User)
                            .map(|message| message.text_concat())
                            .filter(|text| !text.trim().is_empty()).collect()).unwrap_or_default()
                    } else { Vec::new() };
                    Ok((document, history))
                });
                Ok(PreparedSource { source, result })
            }).await.map_err(io::Error::other)?
        });
        Self::Preparing(Box::new(ViewPreparation {
            address,
            task: Some(task),
            phase: receiver,
            started_at: Instant::now(),
            cancellation,
            offset: 0,
            failure: None,
        }))
    }

    pub(crate) fn loading(&self) -> Option<ViewLoading> {
        match self {
            Self::Preparing(preparation) if preparation.failure.is_none() => Some(ViewLoading {
                phase: *preparation.phase.borrow(),
                started_at: preparation.started_at,
            }),
            _ => None,
        }
    }

    pub(super) async fn finish_initial(&mut self) -> io::Result<FlowViewContent<'_>> {
        let Self::Preparing(preparation) = self else {
            return self.current_content();
        };
        let task = preparation.task.as_mut().ok_or_else(|| {
            io::Error::other(
                preparation
                    .failure
                    .as_deref()
                    .unwrap_or("view preparation has no worker"),
            )
        })?;
        let prepared = task
            .await
            .map_err(io::Error::other)
            .and_then(|result| result);
        preparation.task = None;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                preparation.failure = Some(error.to_string());
                return Err(error);
            }
        };
        let offset = preparation.offset;
        *self = prepared.source;
        self.set_pending_offset(offset);
        let (document, history) = prepared.result?;
        if let Some(document) = document {
            Ok(FlowViewContent::Initial { document, history })
        } else {
            self.current_content()
        }
    }
}

impl ViewPreparation {
    pub(super) fn set_pending_offset(&mut self, offset: usize) {
        self.offset = offset;
    }
}
