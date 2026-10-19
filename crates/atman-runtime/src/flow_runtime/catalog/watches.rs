//! Bounded discovery of committed subscriptions across independently sampled flows.

use std::{io, sync::Arc};

use serde::Serialize;
use uuid::Uuid;

use super::{SessionCatalog, require_visible};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{FlowInstanceId, FlowRef, invalid, watch_view::WatchRecord},
};

/// Resume within this Session, excluding Flow IDs at or above `before`. Newly created Sessions and Flows become visible on a fresh scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WatchCatalogCursor {
    pub session_id: Uuid,
    pub before: Option<FlowInstanceId>,
}

#[derive(Debug, Serialize)]
pub struct WatchCatalogSnapshot {
    pub title: String,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub records: Vec<WatchRecord>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchCatalogEntry {
    Flow {
        owner: FlowRef,
        session_title: Option<String>,
        snapshot: Result<WatchCatalogSnapshot, String>,
    },
    SessionUnavailable {
        session_id: Uuid,
        error: String,
    },
}

/// Each Flow retains its own journal receipt; this is not a global snapshot.
#[derive(Debug, Serialize)]
pub struct WatchCatalogPage {
    pub entries: Vec<WatchCatalogEntry>,
    pub next: Option<WatchCatalogCursor>,
    pub total_sessions: usize,
    /// Flow inspections plus empty or unavailable Sessions, bounded by the requested limit.
    pub inspected: usize,
}

impl SessionCatalog {
    /// Inspect subscriptions without acquiring writers, recovering turns, or consuming input. Failed and empty records advance the cursor. Authorization failures reject the entire page.
    pub async fn watch_page(
        &self,
        cursor: Option<WatchCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<WatchCatalogPage> {
        if !(1..=50).contains(&limit) {
            return Err(invalid("watch catalog page limit must be between 1 and 50"));
        }
        let mut ids = Vec::new();
        for id in self.ids().await? {
            if visible(id)? {
                ids.push(id);
            }
        }
        let eligible: Vec<_> = ids
            .iter()
            .rev()
            .copied()
            .filter(|id| {
                cursor
                    .as_ref()
                    .is_none_or(|cursor| *id <= cursor.session_id)
            })
            .collect();
        let mut page = WatchCatalogPage {
            entries: Vec::new(),
            next: None,
            total_sessions: ids.len(),
            inspected: 0,
        };
        for (position, &session_id) in eligible.iter().enumerate() {
            require_visible(session_id, &visible)?;
            let read = self.recorded(session_id).await;
            require_visible(session_id, &visible)?;
            match read {
                Err(error) => {
                    page.entries.push(WatchCatalogEntry::SessionUnavailable {
                        session_id,
                        error: error.to_string(),
                    });
                    page.inspected += 1;
                }
                Ok(read) => {
                    let before = cursor
                        .as_ref()
                        .filter(|cursor| cursor.session_id == session_id)
                        .and_then(|cursor| cursor.before.as_ref());
                    let mut flows: Vec<_> = read
                        .projection
                        .flows
                        .iter()
                        .map(|flow| flow.id.clone())
                        .filter(|id| before.is_none_or(|before| id.0.0 < before.0.0))
                        .collect();
                    flows.sort_unstable_by_key(|id| std::cmp::Reverse(id.0.0));
                    let read = Arc::new(read);
                    if flows.is_empty() {
                        page.inspected += 1;
                    }
                    for (index, instance_id) in flows.iter().enumerate() {
                        let owner = FlowRef {
                            session_id: session_id.into(),
                            instance_id: instance_id.clone(),
                        };
                        require_visible(session_id, &visible)?;
                        authorize(&owner)?;
                        let snapshot = {
                            let read = read.clone();
                            let instance = instance_id.clone();
                            tokio::task::spawn_blocking(move || {
                                let flow = read.inspect_flow(&instance)?;
                                Ok::<_, io::Error>(WatchCatalogSnapshot {
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
                            .map_err(io::Error::other)?
                        };
                        require_visible(session_id, &visible)?;
                        authorize(&owner)?;
                        page.entries.push(WatchCatalogEntry::Flow {
                            owner,
                            session_title: read.projection.title.clone(),
                            snapshot: snapshot.map_err(|error| error.to_string()),
                        });
                        page.inspected += 1;
                        if page.inspected == limit {
                            if index + 1 < flows.len() {
                                page.next = Some(WatchCatalogCursor {
                                    session_id,
                                    before: Some(instance_id.clone()),
                                });
                            }
                            break;
                        }
                    }
                }
            }
            if page.inspected == limit {
                if page.next.is_none() {
                    page.next = eligible
                        .get(position + 1)
                        .map(|&session_id| WatchCatalogCursor {
                            session_id,
                            before: None,
                        });
                }
                break;
            }
        }
        // Recheck even earlier rows after later I/O; partial results must not leak revoked access.
        for id in ids {
            require_visible(id, &visible)?;
        }
        for entry in &page.entries {
            if let WatchCatalogEntry::Flow { owner, .. } = entry {
                authorize(owner)?;
            }
        }
        Ok(page)
    }
}
