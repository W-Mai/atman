use std::io::BufRead;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

const META_FILENAME: &str = "meta.json";
const STATS_FILENAME: &str = "stats.json";
const STATS_LOCK_FILENAME: &str = ".stats.lock";
const STATS_SCHEMA_VERSION: u8 = 1;

fn is_auto_name(source: &NameSource) -> bool {
    matches!(source, NameSource::Auto)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope<'a> {
    CurrentProject(&'a Path),
    AllProjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryScope {
    CurrentProject {
        project_root: PathBuf,
        project_fingerprint: String,
    },
    AllProjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiscoveryQuery {
    pub scope: DiscoveryScope,
    pub include_legacy: bool,
    pub text: Option<String>,
    pub limit: Option<usize>,
}

impl SessionDiscoveryQuery {
    pub fn all_projects() -> Self {
        Self {
            scope: DiscoveryScope::AllProjects,
            include_legacy: false,
            text: None,
            limit: None,
        }
    }

    pub fn current_project(project_root: &Path) -> Self {
        Self {
            scope: DiscoveryScope::CurrentProject {
                project_root: canonical_root(project_root),
                project_fingerprint: fingerprint_from_root(project_root),
            },
            include_legacy: false,
            text: None,
            limit: None,
        }
    }

    pub fn with_legacy(mut self, include_legacy: bool) -> Self {
        self.include_legacy = include_legacy;
        self
    }

    pub fn matches_meta(&self, meta: Option<&SessionMeta>) -> bool {
        let Some(meta) = meta else {
            return self.include_legacy;
        };
        match &self.scope {
            DiscoveryScope::AllProjects => {
                self.include_legacy || meta.project_fingerprint.is_some()
            }
            DiscoveryScope::CurrentProject {
                project_root,
                project_fingerprint,
            } => {
                if meta.project_fingerprint.is_none() {
                    return self.include_legacy;
                }
                meta.project_fingerprint.as_deref() == Some(project_fingerprint)
                    || meta.project_root.as_deref().map(canonical_root).as_ref()
                        == Some(project_root)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    #[default]
    Auto,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub name_source: NameSource,
    pub project_root: Option<PathBuf>,
    pub created_at: Option<DateTime<Utc>>,
    pub event_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "is_auto_name")]
    pub name_source: NameSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStats {
    #[serde(default)]
    schema_version: u8,
    #[serde(default)]
    pub event_bytes: u64,
    #[serde(default)]
    pub event_count: u64,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub user_message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<DateTime<Utc>>,
}

impl Default for SessionStats {
    fn default() -> Self {
        Self {
            schema_version: STATS_SCHEMA_VERSION,
            event_bytes: 0,
            event_count: 0,
            message_count: 0,
            user_message_count: 0,
            first_ts: None,
        }
    }
}

impl SessionStats {
    pub fn load_or_rebuild(session_dir: &Path) -> std::io::Result<Self> {
        let events_path = session_dir.join("events.jsonl");
        let mut event_bytes = std::fs::metadata(&events_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if let Some(stats) = Self::load_unchecked(session_dir)
            && stats.event_bytes == event_bytes
        {
            return Ok(stats);
        }
        let _lock = match lock_stats(session_dir) {
            Ok(lock) => lock,
            Err(_) => return Self::scan(&events_path),
        };
        event_bytes = std::fs::metadata(&events_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if let Some(stats) = Self::load_unchecked(session_dir)
            && stats.event_bytes == event_bytes
        {
            return Ok(stats);
        }
        let stats = Self::scan(&events_path)?;
        let _ = stats.save_unlocked(session_dir);
        Ok(stats)
    }

    pub(crate) fn record_persisted_event(
        session_dir: &Path,
        start: u64,
        end: u64,
        envelope: &crate::event::EventEnvelope,
    ) -> std::io::Result<()> {
        let _lock = lock_stats(session_dir)?;
        let mut stats = match Self::load_unchecked(session_dir) {
            Some(stats) if stats.event_bytes == end => return Ok(()),
            Some(stats) if stats.event_bytes == start => stats,
            None if start == 0 => Self::default(),
            _ => {
                let stats = Self::scan(&session_dir.join("events.jsonl"))?;
                stats.save_unlocked(session_dir)?;
                return Ok(());
            }
        };
        stats.event_bytes = end;
        stats.event_count = stats.event_count.saturating_add(1);
        if stats.first_ts.is_none() {
            stats.first_ts = Some(envelope.ts);
        }
        match &envelope.event {
            crate::event::Event::UserMsg { .. } => {
                stats.user_message_count = stats.user_message_count.saturating_add(1);
                stats.message_count = stats.message_count.saturating_add(1);
            }
            crate::event::Event::AssistantMsg { .. }
            | crate::event::Event::ToolResultMsg { .. } => {
                stats.message_count = stats.message_count.saturating_add(1);
            }
            _ => {}
        }
        stats.save_unlocked(session_dir)
    }

    fn load_unchecked(session_dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(session_dir.join(STATS_FILENAME)).ok()?;
        let stats: Self = serde_json::from_slice(&bytes).ok()?;
        (stats.schema_version == STATS_SCHEMA_VERSION).then_some(stats)
    }

    fn scan(events_path: &Path) -> std::io::Result<Self> {
        #[cfg(test)]
        STATS_SCANS.with(|count| count.set(count.get().saturating_add(1)));

        let file = match std::fs::File::open(events_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        let mut reader = std::io::BufReader::new(file);
        let mut stats = Self::default();
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            stats.event_bytes = stats.event_bytes.saturating_add(bytes as u64);
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            stats.event_count = stats.event_count.saturating_add(1);
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                continue;
            };
            if stats.first_ts.is_none() {
                stats.first_ts = value
                    .get("ts")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                    .map(|ts| ts.with_timezone(&Utc));
            }
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("user_msg") => {
                    stats.user_message_count = stats.user_message_count.saturating_add(1);
                    stats.message_count = stats.message_count.saturating_add(1);
                }
                Some("assistant_msg" | "tool_result_msg") => {
                    stats.message_count = stats.message_count.saturating_add(1);
                }
                _ => {}
            }
        }
        Ok(stats)
    }

    fn save_unlocked(&self, session_dir: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temp = session_dir.join(".stats.json.tmp");
        std::fs::write(&temp, bytes)?;
        if let Err(error) = std::fs::rename(&temp, session_dir.join(STATS_FILENAME)) {
            let _ = std::fs::remove_file(temp);
            return Err(error);
        }
        Ok(())
    }
}

fn lock_stats(session_dir: &Path) -> std::io::Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(session_dir.join(STATS_LOCK_FILENAME))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

#[cfg(test)]
thread_local! {
    static STATS_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl SessionMeta {
    pub fn load(session_dir: &Path) -> Option<Self> {
        let path = session_dir.join(META_FILENAME);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, session_dir: &Path) -> std::io::Result<()> {
        let path = session_dir.join(META_FILENAME);
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, bytes)
    }

    pub fn set_auto_title_if_unclaimed(
        session_dir: &Path,
        title: impl Into<String>,
    ) -> std::io::Result<Option<Self>> {
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "title cannot be empty",
            ));
        }
        let mut meta = Self::load(session_dir).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session metadata not found")
        })?;
        if matches!(meta.name_source, NameSource::User) {
            return Ok(None);
        }
        meta.title = Some(title);
        meta.name_source = NameSource::Auto;
        meta.save(session_dir)?;
        Ok(Some(meta))
    }

    pub fn rename(session_dir: &Path, title: impl Into<String>) -> std::io::Result<Self> {
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "title cannot be empty",
            ));
        }
        let mut meta = Self::load(session_dir).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "session metadata not found")
        })?;
        meta.title = Some(title);
        meta.name_source = NameSource::User;
        meta.save(session_dir)?;
        Ok(meta)
    }

    pub fn discover(root: &Path, scope: SessionScope<'_>) -> std::io::Result<Vec<SessionSummary>> {
        let sessions = root.join("sessions");
        let mut summaries = Vec::new();
        if !sessions.exists() {
            return Ok(summaries);
        }
        for entry in std::fs::read_dir(sessions)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(meta) = Self::load(&path) else {
                continue;
            };
            if let SessionScope::CurrentProject(project) = scope
                && meta.project_root.as_deref() != Some(project)
            {
                continue;
            }
            let event_count = SessionStats::load_or_rebuild(&path)
                .map(|stats| stats.event_count as usize)
                .unwrap_or(0);
            summaries.push(SessionSummary {
                id: entry.file_name().to_string_lossy().into_owned(),
                title: meta.title.unwrap_or_else(|| "Untitled session".into()),
                name_source: meta.name_source,
                project_root: meta.project_root,
                created_at: meta.created_at,
                event_count,
            });
        }
        summaries.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(summaries)
    }

    pub fn from_cwd() -> Self {
        let cwd = std::env::current_dir().ok();
        Self::from_start_path(cwd.as_deref())
    }

    pub fn from_start_path(start: Option<&Path>) -> Self {
        let project_root = start.map(canonical_root);
        let project_fingerprint = project_root.as_deref().map(fingerprint_from_root);
        Self {
            start_path: project_root.clone(),
            project_root,
            project_fingerprint,
            created_at: Some(Utc::now()),
            title: None,
            name_source: NameSource::Auto,
            tags: Vec::new(),
        }
    }

    pub fn rebase(&mut self, new_cwd: &Path) {
        let project_root = canonical_root(new_cwd);
        self.start_path = Some(project_root.clone());
        self.project_fingerprint = Some(fingerprint_from_root(&project_root));
        self.project_root = Some(project_root);
    }

    pub fn set_title(session_dir: &Path, title: Option<String>) -> std::io::Result<()> {
        let mut meta = Self::load(session_dir).unwrap_or_default();
        meta.title = title;
        meta.name_source = NameSource::User;
        meta.save(session_dir)
    }

    pub fn set_auto_title(session_dir: &Path, title: impl Into<String>) -> std::io::Result<bool> {
        Self::set_auto_title_with_force(session_dir, title, false)
    }

    pub fn set_auto_title_with_force(
        session_dir: &Path,
        title: impl Into<String>,
        force: bool,
    ) -> std::io::Result<bool> {
        let mut meta = Self::load(session_dir).unwrap_or_default();
        if !force && meta.name_source == NameSource::User {
            return Ok(false);
        }
        let title = title.into().trim().replace(['\n', '\r'], " ");
        let title: String = title.chars().take(60).collect();
        if title.is_empty() {
            return Ok(false);
        }
        meta.title = Some(title);
        meta.name_source = NameSource::Auto;
        meta.save(session_dir)?;
        Ok(true)
    }
}

pub fn fingerprint_from_root(root: &Path) -> String {
    let stable = root
        .canonicalize()
        .or_else(|_| {
            if root.is_absolute() {
                Ok(root.to_path_buf())
            } else {
                std::env::current_dir().map(|cwd| cwd.join(root))
            }
        })
        .unwrap_or_else(|_| root.to_path_buf());
    let digest = blake3::hash(stable.to_string_lossy().as_bytes());
    hex_prefix(digest.as_bytes(), 16)
}

/// Return the canonical form of the project root, falling back to the raw path.
pub fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn hex_prefix(bytes: &[u8], hex_chars: usize) -> String {
    let mut out = String::with_capacity(hex_chars);
    for byte in bytes {
        if out.len() >= hex_chars {
            break;
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out.truncate(hex_chars);
    out
}

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut cursor: Option<&Path> = Some(start);
    while let Some(dir) = cursor {
        if dir.join(".atman").is_dir() || dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discovery_query_matches_project_identity_and_legacy_policy() {
        let root = PathBuf::from("/tmp/project");
        let query = SessionDiscoveryQuery::current_project(&root).with_legacy(false);
        let matching = SessionMeta {
            project_root: Some(root.clone()),
            project_fingerprint: Some(fingerprint_from_root(&root)),
            ..SessionMeta::default()
        };
        let other = SessionMeta {
            project_root: Some(PathBuf::from("/tmp/other")),
            project_fingerprint: Some(fingerprint_from_root(Path::new("/tmp/other"))),
            ..SessionMeta::default()
        };
        assert!(query.matches_meta(Some(&matching)));
        assert!(!query.matches_meta(Some(&other)));
        assert!(!query.matches_meta(Some(&SessionMeta::default())));
        assert!(!SessionDiscoveryQuery::all_projects().matches_meta(None));
        assert!(
            SessionDiscoveryQuery::all_projects()
                .with_legacy(true)
                .matches_meta(None)
        );
    }

    #[test]
    fn fingerprint_is_stable_16_hex_chars() {
        let tmp = TempDir::new().unwrap();
        let fp = fingerprint_from_root(tmp.path());
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp, fingerprint_from_root(tmp.path()));
    }

    #[test]
    fn find_project_root_locates_git_ancestor() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("nested/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(
            find_project_root(&sub).unwrap().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn find_project_root_prefers_atman_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".atman")).unwrap();
        let root = find_project_root(tmp.path()).unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn find_project_root_returns_none_when_nothing_matches() {
        let tmp = TempDir::new().unwrap();
        assert!(find_project_root(tmp.path()).is_none());
    }

    #[test]
    fn start_path_is_the_project_without_repository_markers() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("plain").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let meta = SessionMeta::from_start_path(Some(&nested));
        let canonical = nested.canonicalize().unwrap();
        assert_eq!(meta.project_root.as_deref(), Some(canonical.as_path()));
        assert_eq!(meta.start_path.as_deref(), Some(canonical.as_path()));
        let fingerprint = fingerprint_from_root(&canonical);
        assert_eq!(
            meta.project_fingerprint.as_deref(),
            Some(fingerprint.as_str())
        );
    }

    #[test]
    fn auto_title_does_not_overwrite_user_title() {
        let tmp = TempDir::new().unwrap();
        SessionMeta::from_start_path(Some(tmp.path()))
            .save(tmp.path())
            .unwrap();
        assert!(SessionMeta::set_auto_title(tmp.path(), "Generated name").unwrap());
        SessionMeta::set_title(tmp.path(), Some("User name".into())).unwrap();
        assert!(!SessionMeta::set_auto_title(tmp.path(), "Replacement").unwrap());
        let meta = SessionMeta::load(tmp.path()).unwrap();
        assert_eq!(meta.title.as_deref(), Some("User name"));
        assert_eq!(meta.name_source, NameSource::User);
    }

    #[test]
    fn built_in_session_name_flow_parses() {
        let parsed = atman_dsl::parse::parse_file(crate::templates::SESSION_NAME_AT).unwrap();
        assert_eq!(parsed.flows[0].name.name, "session_name");
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let meta = SessionMeta {
            project_root: Some(PathBuf::from("/tmp/foo")),
            start_path: Some(PathBuf::from("/tmp/foo/sub")),
            project_fingerprint: Some("deadbeef".repeat(2)),
            created_at: Some(Utc::now()),
            title: Some("nice title".into()),
            name_source: NameSource::User,
            tags: vec!["x".into()],
        };
        meta.save(tmp.path()).unwrap();
        let back = SessionMeta::load(tmp.path()).unwrap();
        assert_eq!(back.project_root, meta.project_root);
        assert_eq!(back.start_path, meta.start_path);
        assert_eq!(back.project_fingerprint, meta.project_fingerprint);
    }

    #[test]
    fn auto_title_does_not_overwrite_manual_rename() {
        let tmp = TempDir::new().unwrap();
        SessionMeta::default().save(tmp.path()).unwrap();
        let auto = SessionMeta::set_auto_title_if_unclaimed(tmp.path(), "Generated title")
            .unwrap()
            .unwrap();
        assert_eq!(auto.name_source, NameSource::Auto);
        let manual = SessionMeta::rename(tmp.path(), "Manual title").unwrap();
        assert_eq!(manual.name_source, NameSource::User);
        assert!(
            SessionMeta::set_auto_title_if_unclaimed(tmp.path(), "Late generated title")
                .unwrap()
                .is_none()
        );
        let loaded = SessionMeta::load(tmp.path()).unwrap();
        assert_eq!(loaded.title.as_deref(), Some("Manual title"));
        assert_eq!(loaded.name_source, NameSource::User);
    }

    #[test]
    fn discovery_filters_and_renames_sessions() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let first = tmp.path().join("sessions/first");
        let second = tmp.path().join("sessions/second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        SessionMeta {
            project_root: Some(project.clone()),
            created_at: Some(Utc::now()),
            ..SessionMeta::default()
        }
        .save(&first)
        .unwrap();
        SessionMeta {
            project_root: Some(tmp.path().join("other")),
            created_at: Some(Utc::now()),
            ..SessionMeta::default()
        }
        .save(&second)
        .unwrap();
        std::fs::write(first.join("events.jsonl"), "{}\n{}\n").unwrap();
        assert_eq!(
            SessionMeta::discover(tmp.path(), SessionScope::CurrentProject(&project))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            SessionMeta::discover(tmp.path(), SessionScope::AllProjects)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            SessionMeta::rename(&first, "  Login fix  ")
                .unwrap()
                .title
                .as_deref(),
            Some("Login fix")
        );
        assert!(SessionMeta::rename(&first, " ").is_err());
        assert_eq!(
            SessionMeta::discover(tmp.path(), SessionScope::CurrentProject(&project)).unwrap()[0]
                .event_count,
            2
        );
    }

    #[test]
    fn rebase_updates_project_root_and_fingerprint() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let mut meta = SessionMeta {
            project_root: Some(PathBuf::from("/old")),
            start_path: Some(PathBuf::from("/old")),
            project_fingerprint: Some("0000000000000000".into()),
            created_at: None,
            title: None,
            name_source: NameSource::Auto,
            tags: vec![],
        };
        meta.rebase(&sub);
        let canonical = sub.canonicalize().unwrap();
        assert_eq!(meta.start_path.as_deref(), Some(canonical.as_path()));
        assert_eq!(meta.project_root.as_deref(), Some(canonical.as_path()));
        let expected_fp = fingerprint_from_root(&canonical);
        assert_eq!(meta.project_fingerprint, Some(expected_fp));
    }

    #[test]
    fn session_meta_serde_backward_compat_no_start_path() {
        // Old meta.json without start_path should deserialize with start_path = None.
        let json = r#"{"project_root":"/tmp/foo","project_fingerprint":"deadbeefdeadbeef","created_at":"2025-01-01T00:00:00Z"}"#;
        let meta: SessionMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.project_root, Some(PathBuf::from("/tmp/foo")));
        assert_eq!(meta.start_path, None);
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(SessionMeta::load(tmp.path()).is_none());
    }

    #[test]
    fn session_stats_backfill_is_reused_until_the_log_changes() {
        let tmp = TempDir::new().unwrap();
        let events = tmp.path().join("events.jsonl");
        let first = concat!(
            "{\"type\":\"user_msg\",\"ts\":\"2026-09-01T00:00:00Z\"}\n",
            "not-json\n",
            "{\"type\":\"assistant_msg\",\"ts\":\"2026-09-01T00:00:01Z\"}\n"
        );
        std::fs::write(&events, first).unwrap();
        STATS_SCANS.with(|count| count.set(0));

        let stats = SessionStats::load_or_rebuild(tmp.path()).unwrap();
        assert_eq!(stats.event_bytes, first.len() as u64);
        assert_eq!(stats.event_count, 3);
        assert_eq!(stats.user_message_count, 1);
        assert_eq!(stats.message_count, 2);
        assert_eq!(
            stats.first_ts,
            Some("2026-09-01T00:00:00Z".parse().unwrap())
        );
        assert!(tmp.path().join(STATS_FILENAME).exists());

        assert_eq!(SessionStats::load_or_rebuild(tmp.path()).unwrap(), stats);
        assert_eq!(STATS_SCANS.with(std::cell::Cell::get), 1);

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&events)
            .unwrap();
        writeln!(file, "{{\"type\":\"tool_result_msg\"}}").unwrap();
        let updated = SessionStats::load_or_rebuild(tmp.path()).unwrap();
        assert_eq!(updated.event_count, 4);
        assert_eq!(updated.message_count, 3);
        assert_eq!(STATS_SCANS.with(std::cell::Cell::get), 2);
    }
}
