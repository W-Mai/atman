use std::path::{Path, PathBuf};

use anyhow::Result;

/// Retains the daemon's startup fence independently of its advisory PID file.
pub struct PidLease {
    path: PathBuf,
    pid: u32,
    _lease: atman_runtime::platform::ExclusiveFileLease,
}

impl PidLease {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(PathBuf::from(lock_name))?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "daemon lease is not a regular file"
        );
        let lease = atman_runtime::platform::ExclusiveFileLease::try_acquire(file)?;
        let pid = std::process::id();
        if let Some(existing) = read_pid(path)?
            && existing != pid
            && is_alive(existing)
        {
            anyhow::bail!("another atman-daemon already running (pid={existing})");
        }
        atman_runtime::platform::atomic_replace(path, pid.to_string().as_bytes())?;
        Ok(Self {
            path: path.to_owned(),
            pid,
            _lease: lease,
        })
    }
}

impl Drop for PidLease {
    fn drop(&mut self) {
        // Keep the lock inode; a waiter must never acquire a different lock file.
        if matches!(read_pid(&self.path), Ok(Some(pid)) if pid == self.pid) {
            remove_pid(&self.path);
        }
    }
}

pub fn default_pid_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DAEMON_PID_PATH") {
        return Ok(PathBuf::from(p));
    }
    Ok(atman_runtime::storage::data_dir()?
        .join("run")
        .join("atman-daemon.pid"))
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
    platform_is_alive(pid)
}

#[cfg(unix)]
fn platform_is_alive(pid: u32) -> bool {
    // kill(pid, 0) — signal 0 probes existence without side effects.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn platform_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0;
    let alive = unsafe {
        GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code as i32 == STILL_ACTIVE
    };
    unsafe { CloseHandle(handle) };
    alive
}

#[cfg(unix)]
pub fn request_process_stop(pid: u32) -> std::io::Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub fn request_process_stop(_pid: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "process signals are unavailable on Windows",
    ))
}
