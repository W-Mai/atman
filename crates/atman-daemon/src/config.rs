use std::path::PathBuf;

use anyhow::{Context, Result};
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(&cfg)?;
        std::fs::write(path, text)?;
        set_config_perms(path)?;
        Ok(cfg)
    }
}

fn generate_token() -> String {
    // Two v4 UUIDs concatenated give 32 bytes of entropy encoded as hex.
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
    let base = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir")?
        .config_dir()
        .to_path_buf();
    Ok(base.join("daemon.toml"))
}
