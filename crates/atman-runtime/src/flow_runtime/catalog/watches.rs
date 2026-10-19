//! Bounded discovery of committed subscriptions across independently sampled flows.

use super::{FlowCatalogCursor, FlowCatalogEntry, FlowCatalogPage, SessionCatalog};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{FlowRef, invalid, watch_view::WatchRecord},
};
use serde::Serialize;
use std::io;
use uuid::Uuid;

pub type WatchCatalogEntry = FlowCatalogEntry<WatchCatalogSnapshot>;
pub type WatchCatalogPage = FlowCatalogPage<WatchCatalogSnapshot>;

#[derive(Debug, Serialize)]
pub struct WatchCatalogSnapshot {
    pub title: String,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub records: Vec<WatchRecord>,
}

impl SessionCatalog {
    /// Inspect subscriptions without acquiring writers, recovering turns, or consuming input. Failed and empty records advance the cursor. Authorization failures reject the entire page.
    pub async fn watch_page(
        &self,
        cursor: Option<FlowCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<WatchCatalogPage> {
        self.scan_page(cursor, limit, visible, authorize, |flow, _| {
            Ok(WatchCatalogSnapshot {
                title: flow.descriptor.content.title().into(),
                cursor: flow
                    .journal
                    .cursor
                    .ok_or_else(|| invalid("flow journal has no creation"))?,
                trailing_bytes: flow.journal.trailing_bytes,
                records: flow.watchers,
            })
        })
        .await
    }
}
