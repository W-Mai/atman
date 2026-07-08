use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub auth_token: String,
}

impl DaemonConfig {
    pub fn load_or_init(path: &PathBuf) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            let cfg: DaemonConfig =
                toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            return Ok(cfg);
        }
        let cfg = DaemonConfig {
            auth_token: generate_token(),
        };
        write_config(path, &cfg)?;
        Ok(cfg)
    }

    pub fn rotate(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Err(anyhow!(
                "no daemon config at {} — nothing to rotate. Run `atman daemon start` once to generate one.",
                path.display()
            ));
        }
        let cfg = DaemonConfig {
            auth_token: generate_token(),
        };
        write_config(path, &cfg)?;
        Ok(cfg)
    }
}

fn write_config(path: &PathBuf, cfg: &DaemonConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(cfg)?;
    std::fs::write(path, text)?;
    set_config_perms(path)?;
    Ok(())
}

fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

fn set_config_perms(path: &PathBuf) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

pub fn default_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DAEMON_CONFIG_PATH") {
        return Ok(PathBuf::from(p));
    }
    Ok(atman_runtime::storage::config_dir()?.join("daemon.toml"))
}
