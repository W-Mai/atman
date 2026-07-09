use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::session_meta::fingerprint_from_root;

// Config lives with the user's other tool configs (`~/.config/atman` on
// unix, `%APPDATA%\atman\config` on Windows). Data — sessions, indexes,
// project stores — lives in the platform's data location so it doesn't
// pollute dotfile repos.
pub fn data_dir() -> Result<PathBuf> {
    resolve_data_dir(EnvOs::current())
}

pub fn config_dir() -> Result<PathBuf> {
    resolve_config_dir(EnvOs::current())
}

// Env + target OS bundled so path resolution is testable without touching
// the process environment.
struct EnvOs {
    atman_config_dir: Option<String>,
    atman_data_dir: Option<String>,
    xdg_config_home: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<String>,
    appdata: Option<String>,
    local_appdata: Option<String>,
    target_os: TargetOs,
}

// Only one variant is ever constructed on a given host, so at least two
// look "dead" to clippy. Tests exercise all three cross-platform paths, so
// the variants have to exist unconditionally.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetOs {
    Macos,
    Linux,
    Windows,
}

impl EnvOs {
    fn current() -> Self {
        Self {
            atman_config_dir: std::env::var("ATMAN_CONFIG_DIR").ok(),
            atman_data_dir: std::env::var("ATMAN_DATA_DIR").ok(),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok(),
            xdg_data_home: std::env::var("XDG_DATA_HOME").ok(),
            home: std::env::var("HOME").ok(),
            appdata: std::env::var("APPDATA").ok(),
            local_appdata: std::env::var("LOCALAPPDATA").ok(),
            target_os: TargetOs::current(),
        }
    }
}

impl TargetOs {
    fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Linux
        }
    }
}

fn resolve_config_dir(env: EnvOs) -> Result<PathBuf> {
    if let Some(p) = env.atman_config_dir {
        return Ok(PathBuf::from(p));
    }
    if let Some(x) = env.xdg_config_home {
        return Ok(PathBuf::from(x).join("atman"));
    }
    match env.target_os {
        TargetOs::Windows => {
            let base = env
                .appdata
                .context("APPDATA not set; set ATMAN_CONFIG_DIR to override")?;
            Ok(PathBuf::from(base).join("atman").join("config"))
        }
        TargetOs::Macos | TargetOs::Linux => {
            let home = env
                .home
                .context("HOME not set; set ATMAN_CONFIG_DIR to override")?;
            Ok(PathBuf::from(home).join(".config").join("atman"))
        }
    }
}

fn resolve_data_dir(env: EnvOs) -> Result<PathBuf> {
    if let Some(p) = env.atman_data_dir {
        return Ok(PathBuf::from(p));
    }
    if let Some(x) = env.xdg_data_home {
        return Ok(PathBuf::from(x).join("atman"));
    }
    match env.target_os {
        TargetOs::Windows => {
            let base = env
                .local_appdata
                .context("LOCALAPPDATA not set; set ATMAN_DATA_DIR to override")?;
            Ok(PathBuf::from(base).join("atman"))
        }
        TargetOs::Macos => {
            let home = env
                .home
                .context("HOME not set; set ATMAN_DATA_DIR to override")?;
            Ok(PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("atman"))
        }
        TargetOs::Linux => {
            let home = env
                .home
                .context("HOME not set; set ATMAN_DATA_DIR to override")?;
            Ok(PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("atman"))
        }
    }
}

pub fn load_storage_config(project_root: Option<&Path>) -> StorageConfig {
    let global = config_dir()
        .ok()
        .map(|d| StorageConfig::load_from(&d.join("config.toml")).unwrap_or_default())
        .unwrap_or_default();
    let project = project_root
        .map(|r| StorageConfig::load_from(&r.join(".atman/config.toml")).unwrap_or_default())
        .unwrap_or_default();
    StorageConfig::merge(global, project)
}

pub fn resolve_project_scope_for(project_root: &Path) -> Result<PathBuf> {
    let cfg = load_storage_config(Some(project_root));
    let data = data_dir()?;
    resolve_project_storage_root(project_root, &cfg, &data)
}

pub fn resolve_current_project_scope() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let project_root = crate::session_meta::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());
    resolve_project_scope_for(&project_root)
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

    fn env_with(target: TargetOs) -> EnvOs {
        EnvOs {
            atman_config_dir: None,
            atman_data_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            home: None,
            appdata: None,
            local_appdata: None,
            target_os: target,
        }
    }

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

    #[test]
    fn atman_config_dir_env_wins() {
        let mut env = env_with(TargetOs::Linux);
        env.atman_config_dir = Some("/somewhere/else".into());
        env.xdg_config_home = Some("/xdg".into());
        env.home = Some("/home/u".into());
        assert_eq!(
            resolve_config_dir(env).unwrap(),
            PathBuf::from("/somewhere/else")
        );
    }

    #[test]
    fn xdg_config_home_beats_home_fallback() {
        let mut env = env_with(TargetOs::Linux);
        env.xdg_config_home = Some("/xdg".into());
        env.home = Some("/home/u".into());
        assert_eq!(
            resolve_config_dir(env).unwrap(),
            PathBuf::from("/xdg/atman")
        );
    }

    #[test]
    fn macos_config_dir_is_dot_config_not_library() {
        let mut env = env_with(TargetOs::Macos);
        env.home = Some("/Users/u".into());
        assert_eq!(
            resolve_config_dir(env).unwrap(),
            PathBuf::from("/Users/u/.config/atman"),
        );
    }

    #[test]
    fn linux_config_dir_defaults_to_dot_config() {
        let mut env = env_with(TargetOs::Linux);
        env.home = Some("/home/u".into());
        assert_eq!(
            resolve_config_dir(env).unwrap(),
            PathBuf::from("/home/u/.config/atman"),
        );
    }

    #[test]
    fn windows_config_dir_uses_appdata_config_subdir() {
        let mut env = env_with(TargetOs::Windows);
        env.appdata = Some(r"C:\Users\u\AppData\Roaming".into());
        assert_eq!(
            resolve_config_dir(env).unwrap(),
            PathBuf::from(r"C:\Users\u\AppData\Roaming")
                .join("atman")
                .join("config"),
        );
    }

    #[test]
    fn atman_data_dir_env_wins() {
        let mut env = env_with(TargetOs::Macos);
        env.atman_data_dir = Some("/data/here".into());
        env.xdg_data_home = Some("/xdg".into());
        env.home = Some("/Users/u".into());
        assert_eq!(resolve_data_dir(env).unwrap(), PathBuf::from("/data/here"));
    }

    #[test]
    fn xdg_data_home_beats_platform_fallback() {
        let mut env = env_with(TargetOs::Macos);
        env.xdg_data_home = Some("/xdg".into());
        env.home = Some("/Users/u".into());
        assert_eq!(resolve_data_dir(env).unwrap(), PathBuf::from("/xdg/atman"));
    }

    #[test]
    fn macos_data_dir_is_application_support() {
        let mut env = env_with(TargetOs::Macos);
        env.home = Some("/Users/u".into());
        assert_eq!(
            resolve_data_dir(env).unwrap(),
            PathBuf::from("/Users/u/Library/Application Support/atman"),
        );
    }

    #[test]
    fn linux_data_dir_defaults_to_local_share() {
        let mut env = env_with(TargetOs::Linux);
        env.home = Some("/home/u".into());
        assert_eq!(
            resolve_data_dir(env).unwrap(),
            PathBuf::from("/home/u/.local/share/atman"),
        );
    }

    #[test]
    fn windows_data_dir_uses_local_appdata() {
        let mut env = env_with(TargetOs::Windows);
        env.local_appdata = Some(r"C:\Users\u\AppData\Local".into());
        assert_eq!(
            resolve_data_dir(env).unwrap(),
            PathBuf::from(r"C:\Users\u\AppData\Local").join("atman"),
        );
    }

    #[test]
    fn missing_home_on_unix_errors_out() {
        let env = env_with(TargetOs::Linux);
        let err = resolve_config_dir(env).unwrap_err();
        assert!(err.to_string().contains("HOME"));
    }

    #[test]
    fn missing_appdata_on_windows_errors_out() {
        let env = env_with(TargetOs::Windows);
        let err = resolve_config_dir(env).unwrap_err();
        assert!(err.to_string().contains("APPDATA"));
    }
}
