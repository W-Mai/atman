//! Host-supplied project metadata. Components never infer storage paths from a view.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use atman_runtime::project_catalog::ProjectRecord;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    Legacy(String),
    Indexed(atman_runtime::history_index::IndexedSource),
    Persistent(Uuid),
}

/// Optional conversation content, prepared after directory metadata is usable.
#[derive(Clone, Debug)]
pub struct SessionBrowseDetails {
    pub message_count: usize,
    pub goal: Option<String>,
    pub preview: Option<String>,
}

pub type IndexedSessionsResult = Result<Vec<atman_runtime::history_index::IndexedSession>, String>;

#[derive(Clone, Debug)]
pub struct ProjectSession {
    pub target: SessionTarget,
    pub title: String,
    pub activity: String,
    pub preview: Option<String>,
    pub updated_at: String,
    pub updated_time: Option<SystemTime>,
    pub goal: Option<String>,
    pub is_current: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectSort {
    #[default]
    NewestFirst,
    OldestFirst,
    NameAscending,
}

impl ProjectSort {
    pub const ALL: [Self; 3] = [Self::NewestFirst, Self::OldestFirst, Self::NameAscending];

    pub fn label(self) -> &'static str {
        match self {
            Self::NewestFirst => "Newest first",
            Self::OldestFirst => "Oldest first",
            Self::NameAscending => "Name A–Z",
        }
    }

    pub fn compare_sessions(self, a: &ProjectSession, b: &ProjectSession) -> Ordering {
        let order = match self {
            Self::NewestFirst => b.updated_time.cmp(&a.updated_time),
            Self::OldestFirst => match (a.updated_time, b.updated_time) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => b.updated_time.is_some().cmp(&a.updated_time.is_some()),
            },
            Self::NameAscending => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
        };
        order.then_with(|| a.title.cmp(&b.title))
    }

    pub fn compare_projects(self, a: &ProjectRecord, b: &ProjectRecord) -> Ordering {
        (match self {
            Self::NewestFirst => b.last_opened.cmp(&a.last_opened),
            Self::OldestFirst => a.last_opened.cmp(&b.last_opened),
            Self::NameAscending => a
                .display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase()),
        })
        .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSortRequest {
    pub id: Uuid,
    pub project: Option<String>,
    pub sort: ProjectSort,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectDirectory {
    pub projects: Vec<ProjectRecord>,
    pub sessions: HashMap<String, Vec<ProjectSession>>,
    pub details: HashMap<String, ProjectDetails>,
}

impl ProjectDirectory {
    pub(crate) fn details_request(&self, source: Uuid) -> crate::TuiControl {
        let mut sessions: Vec<_> = self
            .sessions
            .values()
            .flatten()
            .filter_map(|session| {
                if let SessionTarget::Persistent(id) = session.target {
                    Some((session.updated_time, id))
                } else {
                    None
                }
            })
            .collect();
        sessions.sort_unstable_by(|a, b| b.cmp(a));
        sessions.dedup_by_key(|session| session.1);
        crate::TuiControl::LoadSessionDetails {
            source,
            sessions: sessions.into_iter().map(|session| session.1).collect(),
        }
    }

    pub(crate) fn apply_details(&mut self, session: Uuid, details: &SessionBrowseDetails) {
        apply_details(&mut self.sessions, session, details);
    }
}

pub(crate) fn apply_details(
    sessions: &mut HashMap<String, Vec<ProjectSession>>,
    session: Uuid,
    details: &SessionBrowseDetails,
) {
    for row in sessions
        .values_mut()
        .flatten()
        .filter(|row| row.target == SessionTarget::Persistent(session))
    {
        row.preview = details.preview.clone();
        row.goal = details.goal.clone();
    }
}

pub(crate) fn retain_details(
    sessions: &mut HashMap<String, Vec<ProjectSession>>,
    previous: &HashMap<String, Vec<ProjectSession>>,
) {
    for (project, rows) in sessions.iter_mut() {
        if let Some(old) = previous.get(project) {
            for row in rows {
                if let Some(saved) = old.iter().find(|saved| saved.target == row.target) {
                    if row.preview.is_none() {
                        row.preview = saved.preview.clone();
                    }
                    if row.goal.is_none() {
                        row.goal = saved.goal.clone();
                    }
                }
            }
        }
    }
    for (project, saved) in previous {
        let rows = sessions.entry(project.clone()).or_default();
        let mut indexed: HashSet<_> = rows
            .iter()
            .filter_map(|row| {
                if let SessionTarget::Indexed(source) = &row.target {
                    Some((source.index.clone(), source.session))
                } else {
                    None
                }
            })
            .collect();
        for row in saved
            .iter()
            .filter(|row| matches!(row.target, SessionTarget::Indexed(_)))
        {
            let SessionTarget::Indexed(source) = &row.target else {
                unreachable!()
            };
            if indexed.insert((source.index.clone(), source.session)) {
                rows.push(row.clone());
            }
        }
        rows.sort_by(|a, b| ProjectSort::NewestFirst.compare_sessions(a, b));
    }
}

pub(crate) fn apply_indexed(
    sessions: &mut HashMap<String, Vec<ProjectSession>>,
    indexed: &[atman_runtime::history_index::IndexedSession],
) {
    for rows in sessions.values_mut() {
        rows.retain(|row| !matches!(row.target, SessionTarget::Indexed(_)));
    }
    for session in indexed {
        if let Some(project) = &session.project {
            sessions
                .entry(atman_runtime::session_meta::fingerprint_from_root(project))
                .or_default()
                .push(ProjectSession {
                    target: SessionTarget::Indexed(session.source.clone()),
                    title: session.title(),
                    activity: format!(
                        "{} indexed user messages · Review recovery",
                        session.user_messages
                    ),
                    preview: None,
                    updated_time: chrono::DateTime::parse_from_rfc3339(&session.updated_at)
                        .ok()
                        .map(SystemTime::from),
                    updated_at: session.updated_at.clone(),
                    goal: None,
                    is_current: false,
                });
        }
    }
    for rows in sessions.values_mut() {
        rows.sort_by(|a, b| ProjectSort::NewestFirst.compare_sessions(a, b));
    }
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
