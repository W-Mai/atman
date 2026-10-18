//! Host-supplied project metadata. Components never infer storage paths from a view.

use std::collections::HashMap;

use atman_runtime::project_catalog::ProjectRecord;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    Legacy(String),
    Persistent(Uuid),
}

#[derive(Clone, Debug)]
pub struct ProjectSession {
    pub target: SessionTarget,
    pub title: String,
    pub activity: String,
    pub updated_at: String,
    pub goal: Option<String>,
    pub is_current: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectDirectory {
    pub projects: Vec<ProjectRecord>,
    pub sessions: HashMap<String, Vec<ProjectSession>>,
    pub details: HashMap<String, ProjectDetails>,
}

#[derive(Clone, Debug)]
pub struct ProjectDetails {
    pub path_available: bool,
    pub scope: Option<atman_runtime::storage::StorageScope>,
}

#[derive(Clone, Debug)]
pub struct ProjectRequest {
    pub id: Uuid,
    pub mutation: Option<crate::ProjectMutation>,
}
