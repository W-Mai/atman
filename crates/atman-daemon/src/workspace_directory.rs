//! Authenticated reads of the configured folders on the Profile host.

use atman_proto::{
    JsonRpcError,
    projects::{
        ListWorkspaceFoldersRequest, ListWorkspaceRootsRequest, WorkspaceFolderResponse,
        WorkspaceRootsResponse,
    },
};
use serde_json::Value;

use crate::DaemonState;

pub(crate) fn parse_roots(params: Value) -> Result<ListWorkspaceRootsRequest, JsonRpcError> {
    serde_json::from_value(params)
        .map_err(|_| JsonRpcError::invalid_params("Invalid workspace roots query"))
}

pub(crate) fn parse_folders(params: Value) -> Result<ListWorkspaceFoldersRequest, JsonRpcError> {
    let request: ListWorkspaceFoldersRequest = serde_json::from_value(params)
        .map_err(|_| JsonRpcError::invalid_params("Invalid workspace folder query"))?;
    if request
        .limit
        .is_some_and(|limit| !(1..=50).contains(&limit))
        || request.after.is_some_and(|id| id.is_nil())
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid workspace folder page",
        ));
    }
    Ok(request)
}

pub(crate) async fn roots(
    state: &DaemonState,
    params: Value,
    principal: &str,
) -> Result<WorkspaceRootsResponse, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    parse_roots(params)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Ok(local::roots(state).await)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = state;
        Ok(WorkspaceRootsResponse::Unavailable {
            reason: atman_proto::projects::WorkspaceBrowseFailure::Unsupported,
        })
    }
}

pub(crate) async fn folders(
    state: &DaemonState,
    params: Value,
    principal: &str,
) -> Result<WorkspaceFolderResponse, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let request = parse_folders(params)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        local::folders(state, request).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (state, request);
        Ok(WorkspaceFolderResponse::Unavailable {
            reason: atman_proto::projects::WorkspaceBrowseFailure::Unsupported,
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod local;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use local::FolderReceipts;
