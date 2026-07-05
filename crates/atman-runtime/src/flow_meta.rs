use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const HASH_PREFIX_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowMeta {
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub last_modified: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip)]
    pub source: FlowMetaSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowMetaSource {
    Sidecar,
    #[default]
    HashFallback,
}

impl FlowMeta {
    pub fn load(at_path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(at_path)
            .with_context(|| format!("read flow source {}", at_path.display()))?;
        Self::from_source(at_path, &src)
    }

    pub fn from_source(at_path: &Path, at_source: &str) -> Result<Self> {
        let sidecar = sidecar_path(at_path);
        if sidecar.exists() {
            return load_sidecar(&sidecar).map(|mut m| {
                m.source = FlowMetaSource::Sidecar;
                m
            });
        }
        Ok(hash_meta(at_source))
    }

    pub fn is_sidecar(&self) -> bool {
        matches!(self.source, FlowMetaSource::Sidecar)
    }

    pub fn short_hash(at_source: &str) -> String {
        let hash = blake3::hash(at_source.as_bytes()).to_hex().to_string();
        hash.chars().take(HASH_PREFIX_LEN).collect()
    }
}

pub fn sidecar_path(at_path: &Path) -> PathBuf {
    let name = at_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let with_meta = format!("{name}.meta.toml");
    at_path.with_file_name(with_meta)
}

fn load_sidecar(path: &Path) -> Result<FlowMeta> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut meta: FlowMeta =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if meta.version.trim().is_empty() {
        anyhow::bail!("{}: `version` must be non-empty", path.display());
    }
    meta.source = FlowMetaSource::Sidecar;
    Ok(meta)
}

fn hash_meta(at_source: &str) -> FlowMeta {
    FlowMeta {
        version: format!("hash:{}", FlowMeta::short_hash(at_source)),
        description: None,
        last_modified: None,
        author: None,
        tags: Vec::new(),
        source: FlowMetaSource::HashFallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_fallback_when_no_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("greet.at");
        std::fs::write(&at, "flow greet() { return 1 }").unwrap();
        let meta = FlowMeta::load(&at).unwrap();
        assert!(!meta.is_sidecar());
        assert!(meta.version.starts_with("hash:"));
        assert_eq!(meta.version.len(), "hash:".len() + HASH_PREFIX_LEN);
    }

    #[test]
    fn hash_is_stable_and_content_addressed() {
        let a = FlowMeta::short_hash("flow x() { return 1 }");
        let b = FlowMeta::short_hash("flow x() { return 1 }");
        let c = FlowMeta::short_hash("flow x() { return 2 }");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn sidecar_wins_over_hash() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("greet.at");
        std::fs::write(&at, "flow greet() { return 1 }").unwrap();
        std::fs::write(
            dir.path().join("greet.at.meta.toml"),
            r#"version = "0.3.1"
description = "greet the user"
author = "w-mai"
tags = ["hello", "demo"]
"#,
        )
        .unwrap();
        let meta = FlowMeta::load(&at).unwrap();
        assert!(meta.is_sidecar());
        assert_eq!(meta.version, "0.3.1");
        assert_eq!(meta.description.as_deref(), Some("greet the user"));
        assert_eq!(meta.author.as_deref(), Some("w-mai"));
        assert_eq!(meta.tags, vec!["hello", "demo"]);
    }

    #[test]
    fn sidecar_missing_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("foo.at");
        std::fs::write(&at, "flow foo() { return 1 }").unwrap();
        std::fs::write(
            dir.path().join("foo.at.meta.toml"),
            r#"version = ""
description = "bad"
"#,
        )
        .unwrap();
        let err = FlowMeta::load(&at).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn sidecar_path_beside_at_file() {
        let p = sidecar_path(Path::new("/tmp/agents/review.at"));
        assert_eq!(p, PathBuf::from("/tmp/agents/review.at.meta.toml"));
    }

    #[test]
    fn from_source_reads_sidecar_without_reading_at_file() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().join("v.at");
        std::fs::write(dir.path().join("v.at.meta.toml"), "version = \"1.2.3\"\n").unwrap();
        let meta = FlowMeta::from_source(&at, "flow v() { return 1 }").unwrap();
        assert_eq!(meta.version, "1.2.3");
        assert!(meta.is_sidecar());
    }
}
