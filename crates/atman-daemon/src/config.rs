use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub auth_token: String,
}

impl DaemonConfig {
    pub fn load_or_init(path: &PathBuf) -> Result<Self> {
        let config = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(path)
            .load_or_init_daemon_config()?;
        Ok(Self {
            auth_token: config.auth_token,
        })
    }

    pub fn rotate(path: &PathBuf) -> Result<Self> {
        let config = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(path)
            .rotate_daemon_config()?;
        Ok(Self {
            auth_token: config.auth_token,
        })
    }
}

pub fn default_config_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DAEMON_CONFIG_PATH") {
        return Ok(PathBuf::from(p));
    }
    Ok(atman_runtime::storage::config_dir()?.join("daemon.toml"))
}
