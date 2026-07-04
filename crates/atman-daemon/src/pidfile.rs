use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub fn default_pid_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DAEMON_PID_PATH") {
        return Ok(PathBuf::from(p));
    }
    let base = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir")?
        .data_dir()
        .to_path_buf();
    Ok(base.join("run").join("atman-daemon.pid"))
}

pub fn write_pid(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, pid.to_string())?;
    Ok(())
}

pub fn read_pid(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text.trim().parse().ok())
}

pub fn remove_pid(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn is_alive(pid: u32) -> bool {
    // kill(pid, 0) — signal 0 probes existence without side effects.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
