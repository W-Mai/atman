//! Bounded, authorized traversal of independently committed Flow records.

use super::{SessionCatalog, require_visible};
use crate::flow_store::{FlowInstanceId, FlowRef, invalid};
use serde::Serialize;
use std::{io, sync::Arc};
use uuid::Uuid;

/// Resume within this Session, excluding Flow IDs at or above `before`. Newly created Sessions and Flows become visible on a fresh scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FlowCatalogCursor {
    pub session_id: Uuid,
    pub before: Option<FlowInstanceId>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowCatalogEntry<T> {
    Flow {
        owner: FlowRef,
        session_title: Option<String>,
        snapshot: Result<T, String>,
    },
    SessionUnavailable {
        session_id: Uuid,
        error: String,
    },
}

/// Each Flow retains its own journal receipt; this is not a global snapshot.
#[derive(Debug, Serialize)]
pub struct FlowCatalogPage<T> {
    pub entries: Vec<FlowCatalogEntry<T>>,
    pub next: Option<FlowCatalogCursor>,
    pub total_sessions: usize,
    /// Flow inspections plus empty or unavailable Sessions, bounded by the requested limit.
    pub inspected: usize,
}

impl SessionCatalog {
    /// Shared bounded traversal; project only the fields requested by the directory.
    pub(super) async fn scan_page<T: Send + 'static>(
        &self,
        cursor: Option<FlowCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
        project: fn(
            crate::flow_store::FlowInspection,
            &crate::flow_store::SessionProjection,
        ) -> io::Result<T>,
    ) -> io::Result<FlowCatalogPage<T>> {
        self.scan_page_observed(cursor, limit, visible, authorize, project, |_, snapshot| {
            std::future::ready(Ok(snapshot))
        })
        .await
    }

    /// Additional host observations finish before the same authorization checks as recorded reads.
    pub(super) async fn scan_page_observed<
        T: Send + 'static,
        F: std::future::Future<Output = io::Result<T>>,
    >(
        &self,
        cursor: Option<FlowCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
        project: fn(
            crate::flow_store::FlowInspection,
            &crate::flow_store::SessionProjection,
        ) -> io::Result<T>,
        observe: impl Fn(FlowRef, T) -> F,
    ) -> io::Result<FlowCatalogPage<T>> {
        if !(1..=50).contains(&limit) {
            return Err(invalid("flow catalog page limit must be between 1 and 50"));
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
        let mut page = FlowCatalogPage {
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
                    page.entries.push(FlowCatalogEntry::SessionUnavailable {
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
                                project(flow, &read.projection)
                            })
                            .await
                            .map_err(io::Error::other)?
                        };
                        let snapshot = match snapshot {
                            Ok(snapshot) => observe(owner.clone(), snapshot).await,
                            Err(error) => Err(error),
                        };
                        require_visible(session_id, &visible)?;
                        authorize(&owner)?;
                        page.entries.push(FlowCatalogEntry::Flow {
                            owner,
                            session_title: read.projection.title.clone(),
                            snapshot: snapshot.map_err(|error| error.to_string()),
                        });
                        page.inspected += 1;
                        if page.inspected == limit {
                            if index + 1 < flows.len() {
                                page.next = Some(FlowCatalogCursor {
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
                        .map(|&session_id| FlowCatalogCursor {
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
            if let FlowCatalogEntry::Flow { owner, .. } = entry {
                authorize(owner)?;
            }
        }
        Ok(page)
    }
}
