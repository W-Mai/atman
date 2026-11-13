use std::path::{Path, PathBuf};

use anyhow::Result;

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
