//! Read-only public projection of the registered Project directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
