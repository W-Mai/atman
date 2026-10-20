//! Durable workspace bindings, independent of checkout availability and execution owners.

use std::{io, path::PathBuf};

use serde::Serialize;
use uuid::Uuid;

use super::{FlowCatalogCursor, FlowCatalogEntry, FlowCatalogPage, SessionCatalog};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{FlowRef, invalid, workspace::WorkspaceRequest},
};

pub type WorkspaceCatalogEntry = FlowCatalogEntry<WorkspaceCatalogSnapshot>;
pub type WorkspaceCatalogPage = FlowCatalogPage<WorkspaceCatalogSnapshot>;

/// The request describes recorded acquisition intent, not a currently held reference. Inspect the resource registry separately before displaying ownership or offering lifecycle controls.
#[derive(Debug, Serialize)]
pub struct WorkspaceCatalogSnapshot {
    pub title: String,
    pub project_root: Option<PathBuf>,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub workspace: Option<WorkspaceRequest>,
}

impl SessionCatalog {
    /// Discover committed workspace locations without opening writers or inspecting Git. Missing checkouts and external storage remain discoverable. Empty and failed records advance the shared bounded cursor; authorization failure rejects the entire page.
    pub async fn workspace_page(
        &self,
        cursor: Option<FlowCatalogCursor>,
        limit: usize,
        visible: impl Fn(Uuid) -> io::Result<bool>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<WorkspaceCatalogPage> {
        self.scan_page(cursor, limit, visible, authorize, |flow, _session| {
            Ok(WorkspaceCatalogSnapshot {
                title: flow.descriptor.content.title().into(),
                project_root: flow
                    .project
                    .as_ref()
                    .map(|project| project.root().to_owned()),
                cursor: flow
                    .journal
                    .cursor
                    .ok_or_else(|| invalid("flow journal has no creation"))?,
                trailing_bytes: flow.journal.trailing_bytes,
                workspace: flow.workspace,
            })
        })
        .await
    }
}
