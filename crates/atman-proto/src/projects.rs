//! Public projection and Pin command for the registered Project directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectListCursor {
    /// Revision of the directory contents returned with the previous page.
    pub revision: String,
    pub offset: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ListProjectsRequest {
    /// Maximum number of entries, from 1 to 50. Defaults to 50.
    #[schema(minimum = 1, maximum = 50)]
    pub limit: Option<u32>,
    pub after: Option<ProjectListCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ProjectListEntry {
    pub fingerprint: String,
    pub display_name: String,
    /// Display path and join key for existing Session summaries. It grants no filesystem access.
    pub project_root: String,
    pub pinned: bool,
    pub archived: bool,
    pub last_opened: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ProjectListPage {
    pub revision: String,
    pub projects: Vec<ProjectListEntry>,
    pub after: Option<ProjectListCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetProjectPinnedRequest {
    pub fingerprint: String,
    /// Directory revision observed with the target Project.
    pub expected_revision: String,
    pub pinned: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SetProjectPinnedResponse {
    pub revision: String,
    pub project: ProjectListEntry,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ListWorkspaceRootsRequest {}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRoot {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRoots {
    pub roots: Vec<WorkspaceRoot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ListWorkspaceFoldersRequest {
    pub root_id: String,
    pub components: Vec<String>,
    #[schema(minimum = 1, maximum = 50)]
    pub limit: Option<u32>,
    pub after: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFolderPage {
    pub root_id: String,
    pub components: Vec<String>,
    pub folders: Vec<String>,
    /// A short-lived receipt for the observed directory and current authorization.
    pub selection: Uuid,
    pub after: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBrowseFailure {
    Busy,
    InvalidPolicy,
    UnknownRoot,
    DirectoryUnavailable,
    DirectoryChanged,
    DirectoryTooLarge,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceRootsResponse {
    Ready { directory: WorkspaceRoots },
    Unavailable { reason: WorkspaceBrowseFailure },
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceFolderResponse {
    Ready { page: WorkspaceFolderPage },
    Unavailable { reason: WorkspaceBrowseFailure },
}
