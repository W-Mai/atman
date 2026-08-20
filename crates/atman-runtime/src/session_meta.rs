use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const META_FILENAME: &str = "meta.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope<'a> {
    CurrentProject(&'a Path),
    AllProjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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
            let event_count = std::fs::read_to_string(path.join("events.jsonl"))
                .map(|s| s.lines().filter(|line| !line.trim().is_empty()).count())
                .unwrap_or(0);
            summaries.push(SessionSummary {
                id: entry.file_name().to_string_lossy().into_owned(),
                title: meta.title.unwrap_or_else(|| "Untitled session".into()),
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
        let project_root = start.and_then(find_project_root);
        let project_fingerprint = project_root.as_deref().map(fingerprint_from_root);
        Self {
            project_root,
            start_path: start.map(|p| p.to_path_buf()),
            project_fingerprint,
            created_at: Some(Utc::now()),
            title: None,
            tags: Vec::new(),
        }
    }

    /// Recompute `project_root`, `project_fingerprint`, and `start_path`
    /// from a new working directory.
    pub fn rebase(&mut self, new_cwd: &Path) {
        self.start_path = Some(new_cwd.to_path_buf());
        self.project_root = find_project_root(new_cwd);
        self.project_fingerprint = self.project_root.as_deref().map(fingerprint_from_root);
    }

    pub fn set_title(session_dir: &Path, title: Option<String>) -> std::io::Result<()> {
        let mut meta = Self::load(session_dir).unwrap_or_default();
        meta.title = title;
        meta.save(session_dir)
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
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let meta = SessionMeta {
            project_root: Some(PathBuf::from("/tmp/foo")),
            start_path: Some(PathBuf::from("/tmp/foo/sub")),
            project_fingerprint: Some("deadbeef".repeat(2)),
            created_at: Some(Utc::now()),
            title: Some("nice title".into()),
            tags: vec!["x".into()],
        };
        meta.save(tmp.path()).unwrap();
        let back = SessionMeta::load(tmp.path()).unwrap();
        assert_eq!(back.project_root, meta.project_root);
        assert_eq!(back.start_path, meta.start_path);
        assert_eq!(back.project_fingerprint, meta.project_fingerprint);
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
            tags: vec![],
        };
        meta.rebase(&sub);
        assert_eq!(meta.start_path, Some(sub.clone()));
        assert_eq!(
            meta.project_root.unwrap().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
        let expected_fp = fingerprint_from_root(tmp.path());
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
}
