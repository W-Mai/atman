use std::io;

#[cfg(unix)]
pub(super) struct ShellModes(Option<libc::termios>);

#[cfg(unix)]
impl ShellModes {
    pub fn capture() -> Self {
        let mut mode = unsafe { std::mem::zeroed() };
        Self((unsafe { libc::tcgetattr(0, &mut mode) } == 0).then_some(mode))
    }

    pub fn restore(&self) -> io::Result<()> {
        if let Some(mode) = &self.0
            && unsafe { libc::tcsetattr(0, libc::TCSANOW, mode) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ShellModes {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(windows)]
pub(super) struct ShellModes(Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>);
#[cfg(windows)]
impl ShellModes {
    pub fn capture() -> Self {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut modes = Vec::new();
        for standard in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
            let handle = unsafe { GetStdHandle(standard) };
            let mut mode = 0;
            if unsafe { GetConsoleMode(handle, &mut mode) } != 0 {
                modes.push((handle, mode));
            }
        }
        Self(modes)
    }
}

#[cfg(windows)]
impl Drop for ShellModes {
    fn drop(&mut self) {
        for (handle, mode) in &self.0 {
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(*handle, *mode);
            }
        }
    }
}

/// Start an interactive host independently of the shell wrapper's lifetime.
pub fn configure_native_worker(command: &mut std::process::Command) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only async-signal-safe process setup runs between spawn and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::{
            JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK},
            Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP},
        };
        let limits = current_job_limits()?;
        let mut flags = CREATE_NEW_PROCESS_GROUP;
        // Inherited Job membership does not make the wrapper its handle owner.
        // Prefer permitted breakaway; otherwise retain ordinary inheritance.
        if limits & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0
            && limits & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK == 0
        {
            flags |= CREATE_BREAKAWAY_FROM_JOB;
        }
        command.creation_flags(flags);
    }
    Ok(())
}

#[cfg(windows)]
fn current_job_limits() -> io::Result<u32> {
    use windows_sys::Win32::System::{
        JobObjects::{
            IsProcessInJob, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, QueryInformationJobObject,
        },
        Threading::GetCurrentProcess,
    };
    let mut in_job = 0;
    if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_job == 0 {
        return Ok(0);
    }
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    if unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut(&mut information).cast(),
            std::mem::size_of_val(&information) as u32,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(information.BasicLimitInformation.LimitFlags)
}

#[cfg(windows)]
pub(super) struct ConsoleSignals;

#[cfg(windows)]
static CONSOLE_RETIRE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn receive_console_control(signal: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    match signal {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            CONSOLE_RETIRE.store(true, std::sync::atomic::Ordering::Relaxed);
            1
        }
        // Physical console destruction terminates an attached process even if its
        // handler returns TRUE. It cannot acknowledge a successful UI retirement.
        _ => 0,
    }
}

#[cfg(windows)]
impl ConsoleSignals {
    pub fn install() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        CONSOLE_RETIRE.store(false, std::sync::atomic::Ordering::Relaxed);
        if unsafe { SetConsoleCtrlHandler(Some(receive_console_control), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let signals = Self;
        // CREATE_NEW_PROCESS_GROUP initially disables Ctrl+C for the worker.
        if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(signals)
    }

    pub fn retire(&self) -> bool {
        CONSOLE_RETIRE.swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn resize(&self) -> bool {
        false
    }

    pub fn forward_resize(&self, _: u32) {}
}

#[cfg(windows)]
impl Drop for ConsoleSignals {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(receive_console_control),
                0,
            );
        }
    }
}

/// Release the shell terminal after input/render retirement and summary output.
pub fn detach_terminal_stdio(log: &std::fs::File) -> io::Result<()> {
    use std::io::Write;
    // The original terminal may already be gone; descriptor replacement still must run.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let input = std::fs::File::open("/dev/null")?;
        for (source, destination) in [
            (input.as_raw_fd(), 0),
            (log.as_raw_fd(), 1),
            (log.as_raw_fd(), 2),
        ] {
            if unsafe { libc::dup2(source, destination) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(windows)]
    {
        detach_windows_stdio(log)?;
    }
    Ok(())
}

#[cfg(windows)]
fn detach_windows_stdio(log: &std::fs::File) -> io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        FreeConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    unsafe extern "C" {
        fn _open_osfhandle(handle: isize, flags: i32) -> i32;
        fn _dup2(source: i32, destination: i32) -> i32;
        fn _close(descriptor: i32) -> i32;
    }
    let null = std::fs::File::open("NUL")?;
    let files = [null, log.try_clone()?, log.try_clone()?];
    for (descriptor, (file, standard)) in files
        .into_iter()
        .zip([STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE])
        .enumerate()
    {
        // The CRT owns this duplicate; the process standard handle owns another.
        let crt_handle = file.try_clone()?.into_raw_handle();
        let crt = unsafe { _open_osfhandle(crt_handle as isize, 0x8000) };
        if crt == -1 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(crt_handle);
            }
            return Err(io::Error::last_os_error());
        }
        if crt != descriptor as i32 {
            let result = unsafe { _dup2(crt, descriptor as i32) };
            unsafe {
                _close(crt);
            }
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let handle = file.into_raw_handle();
        if unsafe { SetStdHandle(standard, handle) } == 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return Err(io::Error::last_os_error());
        }
    }
    // No crossterm state may be used after this presentation boundary.
    if unsafe { FreeConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
