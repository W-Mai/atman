//! Browse Project records through the authenticated operator's current storage profile.

use atman_proto::{
    JsonRpcError,
    projects::{
        ListProjectsRequest, ProjectListCursor, ProjectListEntry, ProjectListPage,
        SetProjectPinnedRequest, SetProjectPinnedResponse,
    },
};
use atman_runtime::project_catalog::{ProjectCatalogStore, ProjectRecord};
use serde_json::Value;

use crate::DaemonState;

const PAGE_ROWS: u32 = 50;
// Includes headroom for response metadata and array punctuation.
const PAGE_ROW_BYTES: usize = 128 * 1024 - 1024;

pub(crate) fn parse(params: Value) -> Result<ListProjectsRequest, JsonRpcError> {
    let request: ListProjectsRequest = serde_json::from_value(params)
        .map_err(|_| JsonRpcError::invalid_params("Invalid Project directory query"))?;
    if request
        .limit
        .is_some_and(|limit| !(1..=PAGE_ROWS).contains(&limit))
    {
        return Err(JsonRpcError::invalid_params(
            "Project page limit must be from 1 to 50",
        ));
    }
    if request
        .after
        .as_ref()
        .is_some_and(|cursor| cursor.offset == 0 || !lower_hex(&cursor.revision, 64))
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid Project directory cursor",
        ));
    }
    Ok(request)
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn parse_pin(params: Value) -> Result<SetProjectPinnedRequest, JsonRpcError> {
    let request: SetProjectPinnedRequest = serde_json::from_value(params)
        .map_err(|_| JsonRpcError::invalid_params("Invalid Project Pin command"))?;
    if !lower_hex(&request.fingerprint, 16) || !lower_hex(&request.expected_revision, 64) {
        return Err(JsonRpcError::invalid_params(
            "Invalid Project fingerprint or directory revision",
        ));
    }
    Ok(request)
}

pub(crate) async fn set_pinned(
    state: &DaemonState,
    params: Value,
    principal: &str,
) -> Result<SetProjectPinnedResponse, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let request = parse_pin(params)?;
    let host = state.runtime_host();
    let operation = host
        .retain_host_operation()
        .map_err(|error| JsonRpcError::application(error.to_string()))?;
    let owner = match host.profile_control_port() {
        Some(owner) => owner,
        None => host
            .open_profile(state.data_dir())
            .await
            .map_err(|_| JsonRpcError::application("Project Profile owner is unavailable"))?,
    };
    let (record, revision) = owner
        .set_project_pinned(
            request.fingerprint,
            request.expected_revision,
            request.pinned,
            operation,
        )
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::NotConnected => JsonRpcError::application(error.to_string()),
            _ => JsonRpcError::application(
                "Project Pin outcome is unavailable; refresh the directory",
            ),
        })?;
    Ok(SetProjectPinnedResponse {
        revision,
        project: entry(record),
    })
}

fn entry(record: ProjectRecord) -> ProjectListEntry {
    ProjectListEntry {
        fingerprint: record.fingerprint,
        display_name: record.display_name,
        project_root: record.root.to_string_lossy().into_owned(),
        pinned: record.pinned,
        archived: record.archived,
        last_opened: record.last_opened,
    }
}

pub(crate) async fn list(
    state: &DaemonState,
    params: Value,
    principal: &str,
) -> Result<ProjectListPage, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let request = parse(params)?;
    let observation = ProjectCatalogStore::new(state.data_dir())
        .observe()
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock => {
                JsonRpcError::application("Project directory reader is busy; retry")
            }
            std::io::ErrorKind::InvalidData => JsonRpcError::application(
                "Project directory exceeds the read budget or has an unsupported version",
            ),
            _ => JsonRpcError::application("Project directory is unavailable"),
        })?;
    let revision = observation.revision;
    let offset = match request.after {
        Some(cursor) if cursor.revision == revision => cursor.offset as usize,
        Some(_) => {
            return Err(JsonRpcError::application(
                "Project directory changed; restart paging",
            ));
        }
        None => 0,
    };
    let total = observation.catalog.projects.len();
    if offset != 0 && offset >= total {
        return Err(JsonRpcError::invalid_params(
            "Project directory cursor is outside the snapshot",
        ));
    }
    let limit = request.limit.unwrap_or(PAGE_ROWS) as usize;
    let mut projects = Vec::with_capacity(limit);
    let mut available = PAGE_ROW_BYTES;
    for record in observation
        .catalog
        .projects
        .into_iter()
        .skip(offset)
        .take(limit)
    {
        let entry = entry(record);
        let bytes = serde_json::to_vec(&entry)
            .map_err(|_| JsonRpcError::internal("Project entry could not be encoded"))?
            .len();
        if bytes > available {
            if projects.is_empty() {
                return Err(JsonRpcError::application(
                    "Project entry exceeds the page byte budget",
                ));
            }
            break;
        }
        available -= bytes;
        projects.push(entry);
    }
    let next = offset + projects.len();
    let after = (next < total).then(|| ProjectListCursor {
        revision: revision.clone(),
        offset: u32::try_from(next).expect("row count is bounded by the catalog byte budget"),
    });
    Ok(ProjectListPage {
        revision,
        projects,
        after,
    })
}
