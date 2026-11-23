use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use atman_proto::{
    JsonRpcError,
    projects::{
        ListWorkspaceFoldersRequest, WorkspaceBrowseFailure, WorkspaceFolderPage,
        WorkspaceFolderResponse, WorkspaceRoot, WorkspaceRoots, WorkspaceRootsResponse,
    },
};
use atman_runtime::project_catalog::workspace::{
    self, BrowseError, DirectoryBoundary, WorkspaceReader,
};
use uuid::Uuid;

use crate::DaemonState;

mod open;
pub(super) use open::{open, status};

const RECEIPT_LIFETIME: Duration = Duration::from_secs(180);
const RECEIPTS: usize = 512;

#[derive(Default)]
pub(crate) struct FolderReceipts(Mutex<VecDeque<Receipt>>);

#[derive(PartialEq, Eq)]
struct Observation {
    root: String,
    components: Vec<String>,
    boundary: DirectoryBoundary,
}

#[derive(Clone)]
struct Receipt {
    id: Uuid,
    observed: Arc<Observation>,
    offset: usize,
    expires: Instant,
}

impl FolderReceipts {
    fn selection(
        &self,
        id: Uuid,
    ) -> Option<atman_runtime::flow_store::profile::workspace_open::WorkspaceOpenCandidate> {
        let now = Instant::now();
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|receipt| receipt.id == id && receipt.offset == 0 && receipt.expires > now)
            .map(|receipt| {
                atman_runtime::flow_store::profile::workspace_open::WorkspaceOpenCandidate {
                    expires: receipt.expires,
                    selection: workspace::WorkspaceSelection {
                        root_id: receipt.observed.root.clone(),
                        components: receipt.observed.components.clone(),
                        boundary: receipt.observed.boundary.clone(),
                    },
                }
            })
    }

    fn continuation(&self, id: Uuid, root: &str, components: &[String]) -> Option<Receipt> {
        let now = Instant::now();
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|receipt| {
                receipt.id == id
                    && receipt.expires > now
                    && receipt.offset > 0
                    && receipt.observed.root == root
                    && receipt.observed.components == components
            })
            .cloned()
    }

    fn page(&self, observed: Arc<Observation>, next: Option<usize>) -> (Uuid, Option<Uuid>) {
        let mut receipts = self.0.lock().unwrap();
        let expires = Instant::now() + RECEIPT_LIFETIME;
        let mut issue = |offset| {
            if receipts.len() == RECEIPTS {
                receipts.pop_front();
            }
            let id = Uuid::new_v4();
            receipts.push_back(Receipt {
                id,
                observed: observed.clone(),
                offset,
                expires,
            });
            id
        };
        (issue(0), next.map(issue))
    }
}

fn reader(state: &DaemonState) -> Result<WorkspaceReader, WorkspaceBrowseFailure> {
    let launcher = state.launcher();
    let config_dir = launcher
        .as_ref()
        .and_then(|launcher| launcher.config_dir.as_deref());
    let config = crate::bootstrap::resolve_config_hub(config_dir)
        .map_err(|_| WorkspaceBrowseFailure::InvalidPolicy)?;
    Ok(WorkspaceReader::new(
        config.config_dir().to_owned(),
        state.data_dir().to_owned(),
    ))
}

fn failure(error: BrowseError) -> WorkspaceBrowseFailure {
    match error {
        BrowseError::Busy => WorkspaceBrowseFailure::Busy,
        BrowseError::InvalidPolicy => WorkspaceBrowseFailure::InvalidPolicy,
        BrowseError::UnknownRoot => WorkspaceBrowseFailure::UnknownRoot,
        BrowseError::Unavailable | BrowseError::ReadOnlyRoot => {
            WorkspaceBrowseFailure::DirectoryUnavailable
        }
        BrowseError::Changed => WorkspaceBrowseFailure::DirectoryChanged,
        BrowseError::TooLarge => WorkspaceBrowseFailure::DirectoryTooLarge,
        BrowseError::Unsupported => WorkspaceBrowseFailure::Unsupported,
    }
}

pub(super) async fn roots(state: &DaemonState) -> WorkspaceRootsResponse {
    let result = match reader(state) {
        Ok(reader) => {
            let shutdown = state.shutdown_signal();
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => Err(WorkspaceBrowseFailure::DirectoryUnavailable),
                roots = reader.roots() => roots.map_err(failure),
            }
        }
        Err(reason) => Err(reason),
    };
    match result {
        Ok(roots) => WorkspaceRootsResponse::Ready {
            directory: WorkspaceRoots {
                roots: roots
                    .into_iter()
                    .map(|(id, label)| WorkspaceRoot { id, label })
                    .collect(),
            },
        },
        Err(reason) => WorkspaceRootsResponse::Unavailable { reason },
    }
}

pub(super) async fn folders(
    state: &DaemonState,
    request: ListWorkspaceFoldersRequest,
) -> Result<WorkspaceFolderResponse, JsonRpcError> {
    if !workspace::root_id(&request.root_id) || !workspace::valid_path(&request.components) {
        return Err(JsonRpcError::invalid_params(
            "Invalid workspace root or folder components",
        ));
    }
    Ok(match read_page(state, request).await {
        Ok(page) => WorkspaceFolderResponse::Ready { page },
        Err(reason) => WorkspaceFolderResponse::Unavailable { reason },
    })
}

async fn read_page(
    state: &DaemonState,
    request: ListWorkspaceFoldersRequest,
) -> Result<WorkspaceFolderPage, WorkspaceBrowseFailure> {
    let changed = WorkspaceBrowseFailure::DirectoryChanged;
    let scope = &state.workspace_folders;
    let previous = request
        .after
        .map(|id| {
            scope
                .continuation(id, &request.root_id, &request.components)
                .ok_or(changed)
        })
        .transpose()?;
    let shutdown = state.shutdown_signal();
    let observed = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Err(WorkspaceBrowseFailure::DirectoryUnavailable),
        observed = reader(state)?.folders(request.root_id.clone(), request.components.clone()) => observed.map_err(|error| {
            if previous.is_some() && matches!(error, BrowseError::UnknownRoot | BrowseError::Unavailable) {
                changed
            } else {
                failure(error)
            }
        })?,
    };
    let observation = Arc::new(Observation {
        root: request.root_id.clone(),
        components: request.components.clone(),
        boundary: observed.boundary,
    });
    let offset = if let Some(previous) = previous {
        if previous.observed != observation
            || previous.expires <= Instant::now()
            || previous.offset >= observed.names.len()
        {
            return Err(changed);
        }
        previous.offset
    } else {
        0
    };
    let limit = request.limit.unwrap_or(50) as usize;
    let next = (offset + limit < observed.names.len()).then_some(offset + limit);
    let (selection, after) = scope.page(observation, next);
    Ok(WorkspaceFolderPage {
        root_id: request.root_id,
        components: request.components,
        folders: observed
            .names
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect(),
        selection,
        after,
    })
}
