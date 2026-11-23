//! Lightweight paging for registered Projects and recorded Session sources.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use atman_proto::{JsonRpcError, SessionId, projects::*};
use atman_runtime::project_catalog::{
    ProjectCatalogStore,
    sessions::{DirectorySession, DirectorySessionKind, SessionDirectoryCache},
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::DaemonState;

use super::{PAGE_ROW_BYTES, PAGE_ROWS, entry, validate_page};

const SNAPSHOT_REUSE: Duration = Duration::from_secs(2);
const PROJECT_PREVIEW: usize = 5;
const TITLE_CHARS: usize = 512;

pub(crate) struct SessionDirectory {
    storage: SessionDirectoryCache,
    published: Mutex<Option<Published>>,
    refresh: tokio::sync::Mutex<()>,
}

struct Published {
    checked: Instant,
    snapshot: Arc<Snapshot>,
}

struct Snapshot {
    edition: Uuid,
    catalog_revision: String,
    groups: BTreeMap<Option<String>, Group>,
}

#[derive(PartialEq, Eq)]
struct Group {
    label: String,
    record: Option<ProjectListEntry>,
    sessions: Vec<ProjectSessionEntry>,
}

impl SessionDirectory {
    pub(crate) fn new(data: PathBuf) -> Self {
        Self {
            storage: SessionDirectoryCache::new(data),
            published: Default::default(),
            refresh: Default::default(),
        }
    }

    fn cached(&self, check: impl FnOnce(Instant) -> bool) -> Option<Arc<Snapshot>> {
        self.published
            .lock()
            .unwrap()
            .as_ref()
            .filter(|entry| check(entry.checked))
            .map(|entry| entry.snapshot.clone())
    }

    async fn snapshot(
        &self,
        state: &DaemonState,
        request: &ReadProjectDirectoryRequest,
    ) -> io::Result<Arc<Snapshot>> {
        let started = Instant::now();
        let accepts = |checked: Instant| {
            checked >= started || (!request.refresh && checked.elapsed() < SNAPSHOT_REUSE)
        };
        if let Some(snapshot) = self.cached(accepts) {
            return Ok(snapshot);
        }
        let _refresh = self.refresh.lock().await;
        if let Some(snapshot) = self.cached(accepts) {
            return Ok(snapshot);
        }
        let catalog = ProjectCatalogStore::new(state.data_dir()).observe().await?;
        let rows = self.storage.observe().await?;
        let groups = group(catalog.catalog.projects.into_iter().map(entry), rows);
        let mut published = self.published.lock().unwrap();
        let snapshot = published
            .as_ref()
            .filter(|last| {
                last.snapshot.catalog_revision == catalog.revision && last.snapshot.groups == groups
            })
            .map(|last| last.snapshot.clone())
            .unwrap_or_else(|| {
                Arc::new(Snapshot {
                    edition: Uuid::now_v7(),
                    catalog_revision: catalog.revision,
                    groups,
                })
            });
        *published = Some(Published {
            checked: Instant::now(),
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }
}

pub(crate) async fn read_directory(
    state: &DaemonState,
    params: Value,
    principal: &str,
) -> Result<ReadProjectDirectoryResponse, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let request: ReadProjectDirectoryRequest = serde_json::from_value(params)
        .map_err(|_| JsonRpcError::invalid_params("Invalid Project Session directory query"))?;
    validate_page(request.limit, request.after.as_ref())?;
    if request.after.is_some() && request.refresh
        || request
            .search
            .as_ref()
            .is_some_and(|text| text.chars().count() > 256)
        || matches!(&request.target, ProjectDirectoryTarget::Sessions { project_root: Some(path) } if path.is_empty() || path.len() > 128 * 1024)
    {
        return Err(JsonRpcError::invalid_params(
            "Invalid Project directory selection or search",
        ));
    }
    let search = request
        .search
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let snapshot = if request.after.is_some() {
        let Some(snapshot) = state.project_sessions.cached(|_| true) else {
            return Ok(ReadProjectDirectoryResponse::Changed);
        };
        snapshot
    } else {
        state
            .project_sessions
            .snapshot(state, &request)
            .await
            .map_err(read_error)?
    };
    let query = serde_json::to_vec(&(snapshot.edition, &request.target, request.sort, &search))
        .map_err(encode_error)?;
    let revision = blake3::hash(&query).to_hex().to_string();
    let offset = match request.after {
        Some(cursor) if cursor.revision == revision => cursor.offset,
        Some(_) => return Ok(ReadProjectDirectoryResponse::Changed),
        None => 0,
    } as usize;
    let limit = request.limit.unwrap_or(PAGE_ROWS) as usize;
    let (rows, total, count) = match request.target {
        ProjectDirectoryTarget::Projects => {
            let mut groups: Vec<_> = snapshot
                .groups
                .iter()
                .filter_map(|(root, group)| {
                    let rows = group.matching(root.as_deref(), &search);
                    (matches_group(root.as_deref(), group, &search) || !rows.is_empty())
                        .then_some((root, group, rows))
                })
                .collect();
            groups.sort_by(|(left_root, left, _), (right_root, right, _)| {
                pinned(right)
                    .cmp(&pinned(left))
                    .then_with(|| left_root.is_none().cmp(&right_root.is_none()))
                    .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
                    .then_with(|| left_root.cmp(right_root))
            });
            let total = groups.len();
            check_offset(offset, total)?;
            let projects = pack(groups.into_iter().skip(offset).take(limit).map(
                |(root, group, mut rows)| {
                    sort(&mut rows, request.sort);
                    ProjectDirectoryGroup {
                        project_root: root.clone(),
                        label: group.label.clone(),
                        record: group.record.clone(),
                        session_count: rows.len() as u32,
                        preview: rows.into_iter().take(PROJECT_PREVIEW).cloned().collect(),
                    }
                },
            ))?;
            let count = projects.len();
            (ProjectDirectoryRows::Projects { projects }, total, count)
        }
        ProjectDirectoryTarget::Sessions { project_root } => {
            let Some(group) = snapshot.groups.get(&project_root) else {
                return Ok(ReadProjectDirectoryResponse::UnknownProject);
            };
            let mut rows = group.matching(project_root.as_deref(), &search);
            sort(&mut rows, request.sort);
            let total = rows.len();
            check_offset(offset, total)?;
            let sessions = pack(rows.into_iter().skip(offset).take(limit).cloned())?;
            let count = sessions.len();
            (
                ProjectDirectoryRows::Sessions {
                    project_root,
                    sessions,
                },
                total,
                count,
            )
        }
    };
    let next = offset + count;
    Ok(ReadProjectDirectoryResponse::Ready {
        page: ProjectDirectoryPage {
            catalog_revision: snapshot.catalog_revision.clone(),
            sort: request.sort,
            total: total as u32,
            rows,
            after: (next < total).then(|| ProjectListCursor {
                revision: revision.clone(),
                offset: next as u32,
            }),
            revision,
        },
    })
}

fn group(
    projects: impl Iterator<Item = ProjectListEntry>,
    rows: Vec<DirectorySession>,
) -> BTreeMap<Option<String>, Group> {
    let mut fingerprints = HashMap::new();
    let mut groups = BTreeMap::new();
    for project in projects {
        fingerprints.insert(project.fingerprint.clone(), project.project_root.clone());
        groups.insert(
            Some(project.project_root.clone()),
            Group {
                label: project.display_name.clone(),
                record: Some(project),
                sessions: Vec::new(),
            },
        );
    }
    for row in rows {
        let root = row
            .project_fingerprint
            .as_ref()
            .and_then(|id| fingerprints.get(id))
            .cloned()
            .or_else(|| {
                row.project_root
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .filter(|path| !path.is_empty());
        let group = groups.entry(root.clone()).or_insert_with(|| Group {
            label: root
                .as_ref()
                .and_then(|root| root.split(['/', '\\']).rfind(|part| !part.is_empty()))
                .unwrap_or_else(|| root.as_deref().unwrap_or("No project"))
                .to_owned(),
            record: None,
            sessions: Vec::new(),
        });
        group.sessions.push(ProjectSessionEntry {
            id: SessionId(row.id),
            title: row.title.chars().take(TITLE_CHARS).collect(),
            kind: match row.kind {
                DirectorySessionKind::Native => ProjectSessionKind::Native,
                DirectorySessionKind::Legacy => ProjectSessionKind::Legacy,
            },
            updated_at: row.updated_at,
            available: row.available,
        });
    }
    groups
}

fn matches_group(root: Option<&str>, group: &Group, search: &str) -> bool {
    search.is_empty()
        || group.label.to_lowercase().contains(search)
        || root.is_some_and(|root| root.to_lowercase().contains(search))
}

impl Group {
    fn matching(&self, root: Option<&str>, search: &str) -> Vec<&ProjectSessionEntry> {
        let all = matches_group(root, self, search);
        self.sessions
            .iter()
            .filter(|row| {
                all || row.title.to_lowercase().contains(search)
                    || row.id.to_string().contains(search)
            })
            .collect()
    }
}

fn pinned(group: &Group) -> bool {
    group.record.as_ref().is_some_and(|record| record.pinned)
}

fn sort(rows: &mut [&ProjectSessionEntry], mode: ProjectSessionSort) {
    rows.sort_unstable_by(|left, right| {
        let order = match mode {
            ProjectSessionSort::NewestFirst => right.updated_at.cmp(&left.updated_at),
            ProjectSessionSort::OldestFirst => match (left.updated_at, right.updated_at) {
                (Some(left), Some(right)) => left.cmp(&right),
                _ => right.updated_at.is_some().cmp(&left.updated_at.is_some()),
            },
            ProjectSessionSort::NameAscending => {
                left.title.to_lowercase().cmp(&right.title.to_lowercase())
            }
        };
        order
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.0.cmp(&right.id.0))
            .then_with(|| {
                (left.kind == ProjectSessionKind::Legacy)
                    .cmp(&(right.kind == ProjectSessionKind::Legacy))
            })
    });
}

fn check_offset(offset: usize, total: usize) -> Result<(), JsonRpcError> {
    if offset != 0 && offset >= total {
        return Err(JsonRpcError::invalid_params(
            "Session directory cursor is outside the snapshot",
        ));
    }
    Ok(())
}

fn pack<T: Serialize>(entries: impl Iterator<Item = T>) -> Result<Vec<T>, JsonRpcError> {
    let mut available = PAGE_ROW_BYTES;
    let mut page = Vec::new();
    for entry in entries {
        let bytes = serde_json::to_vec(&entry).map_err(encode_error)?.len() + 1;
        if bytes > available {
            if page.is_empty() {
                return Err(JsonRpcError::application(
                    "Directory entry exceeds its page byte budget",
                ));
            }
            break;
        }
        available -= bytes;
        page.push(entry);
    }
    Ok(page)
}

fn encode_error(_: serde_json::Error) -> JsonRpcError {
    JsonRpcError::internal("Session directory could not be encoded")
}

fn read_error(error: io::Error) -> JsonRpcError {
    JsonRpcError::application(match error.kind() {
        io::ErrorKind::WouldBlock => "Session directory reader is busy; retry",
        io::ErrorKind::InvalidData => {
            "Session directory exceeds its budget or uses an unsupported format"
        }
        _ => "Session directory is unavailable",
    })
}
