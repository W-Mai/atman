use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::session_meta::fingerprint_from_root;

pub fn data_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DATA_DIR") {
        return Ok(PathBuf::from(p));
    }
    let proj = ProjectDirs::from("", "", "atman")
        .context("could not determine XDG data dir; set ATMAN_DATA_DIR to override")?;
    Ok(proj.data_dir().to_path_buf())
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    let proj = ProjectDirs::from("", "", "atman")
        .context("could not determine XDG config dir; set ATMAN_CONFIG_DIR to override")?;
    Ok(proj.config_dir().to_path_buf())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageConfig {
    #[serde(default)]
    pub scope: Option<StorageScope>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageScope {
    #[default]
    Global,
    Local,
}

impl StorageConfig {
    pub fn load_from(path: &Path) -> Result<Self> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(Self::default());
        };
        #[derive(Deserialize, Default)]
        struct Wrapper {
            #[serde(default)]
            storage: StorageConfig,
        }
        let wrapper: Wrapper =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(wrapper.storage)
    }

    pub fn merge(base: Self, overlay: Self) -> Self {
        Self {
            scope: overlay.scope.or(base.scope),
        }
    }
}

pub fn resolve_project_storage_root(
    project_root: &Path,
    cfg: &StorageConfig,
    data_dir: &Path,
) -> Result<PathBuf> {
    let scope = cfg.scope.unwrap_or_default();
    let out = match scope {
        StorageScope::Local => project_root.join(".atman"),
        StorageScope::Global => {
            let fp = fingerprint_from_root(project_root);
            data_dir.join("projects").join(fp)
        }
    };
    std::fs::create_dir_all(&out).with_context(|| format!("mkdir {}", out.display()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scope_defaults_to_global() {
        let cfg = StorageConfig::default();
        assert_eq!(cfg.scope.unwrap_or_default(), StorageScope::Global);
    }

    #[test]
    fn local_scope_resolves_to_project_dot_atman() {
        let project = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let cfg = StorageConfig {
            scope: Some(StorageScope::Local),
        };
        let out = resolve_project_storage_root(project.path(), &cfg, data.path()).unwrap();
        assert_eq!(out, project.path().join(".atman"));
        assert!(out.is_dir());
    }

    #[test]
    fn global_scope_resolves_to_data_dir_projects_fingerprint() {
        let project = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let cfg = StorageConfig {
            scope: Some(StorageScope::Global),
        };
        let out = resolve_project_storage_root(project.path(), &cfg, data.path()).unwrap();
        let fp = fingerprint_from_root(project.path());
        assert_eq!(out, data.path().join("projects").join(fp));
        assert!(out.is_dir());
    }

    #[test]
    fn missing_scope_falls_back_to_global() {
        let project = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        let cfg = StorageConfig::default();
        let out = resolve_project_storage_root(project.path(), &cfg, data.path()).unwrap();
        assert!(out.starts_with(data.path().join("projects")));
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = StorageConfig::load_from(&tmp.path().join("nonexistent.toml")).unwrap();
        assert_eq!(cfg, StorageConfig::default());
    }

    #[test]
    fn load_from_parses_scope() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[storage]\nscope = \"local\"\n").unwrap();
        let cfg = StorageConfig::load_from(&path).unwrap();
        assert_eq!(cfg.scope, Some(StorageScope::Local));
    }

    #[test]
    fn load_from_tolerates_unrelated_sections() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[compaction]\nreview = \"manual-only\"\n\n[storage]\nscope = \"global\"\n",
        )
        .unwrap();
        let cfg = StorageConfig::load_from(&path).unwrap();
        assert_eq!(cfg.scope, Some(StorageScope::Global));
    }

    #[test]
    fn merge_overlay_overrides_base() {
        let base = StorageConfig {
            scope: Some(StorageScope::Global),
        };
        let overlay = StorageConfig {
            scope: Some(StorageScope::Local),
        };
        let merged = StorageConfig::merge(base, overlay);
        assert_eq!(merged.scope, Some(StorageScope::Local));
    }

    #[test]
    fn merge_empty_overlay_keeps_base() {
        let base = StorageConfig {
            scope: Some(StorageScope::Local),
        };
        let overlay = StorageConfig::default();
        let merged = StorageConfig::merge(base, overlay);
        assert_eq!(merged.scope, Some(StorageScope::Local));
    }
}
