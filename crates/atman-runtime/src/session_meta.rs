use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const META_FILENAME: &str = "meta.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
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

    pub fn from_cwd() -> Self {
        let cwd = std::env::current_dir().ok();
        Self::from_start_path(cwd.as_deref())
    }

    pub fn from_start_path(start: Option<&Path>) -> Self {
        let project_root = start.and_then(find_project_root);
        let project_fingerprint = project_root.as_deref().map(fingerprint_from_root);
        Self {
            project_root,
            project_fingerprint,
            created_at: Some(Utc::now()),
            title: None,
            tags: Vec::new(),
        }
    }

    pub fn set_title(session_dir: &Path, title: Option<String>) -> std::io::Result<()> {
        let mut meta = Self::load(session_dir).unwrap_or_default();
        meta.title = title;
        meta.save(session_dir)
    }
}

pub fn fingerprint_from_root(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes());
    hex_prefix(digest.as_bytes(), 16)
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
            project_fingerprint: Some("deadbeef".repeat(2)),
            created_at: Some(Utc::now()),
            title: Some("nice title".into()),
            tags: vec!["x".into()],
        };
        meta.save(tmp.path()).unwrap();
        let back = SessionMeta::load(tmp.path()).unwrap();
        assert_eq!(back.project_root, meta.project_root);
        assert_eq!(back.project_fingerprint, meta.project_fingerprint);
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(SessionMeta::load(tmp.path()).is_none());
    }
}
