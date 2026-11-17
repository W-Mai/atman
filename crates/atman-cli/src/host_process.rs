//! Shell supervision for an original native UI/runtime worker.

use std::{
    io::{self, IsTerminal, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;

pub(crate) mod diagnostics;
mod platform;

const BOOTSTRAP: &str = "_ATMAN_HOST_LAUNCH";
const VERSION: u32 = 1;
const MAX_FRAME: usize = 4096;
static WORKER: OnceLock<Worker> = OnceLock::new();

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    address: SocketAddr,
    launch: Uuid,
    secret: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hello {
    version: u32,
    launch: Uuid,
    secret: Uuid,
    process: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerNotice {
    Retired {
        retirement: Uuid,
        session: Option<Uuid>,
        summary_printed: bool,
        exit_code: i32,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WrapperNotice {
    Accepted { launch: Uuid, process: u32 },
    Retire,
    Observed { retirement: Uuid },
}

pub(crate) struct Worker {
    output: Mutex<TcpStream>,
    retire: watch::Sender<bool>,
}

pub(crate) fn worker() -> Option<&'static Worker> {
    WORKER.get()
}

/// Called before Runtime construction, config migration, theme probing or threads.
pub(crate) fn launch(cli: &crate::Cli) -> Result<Option<i32>> {
    if let Some(value) = std::env::var_os(BOOTSTRAP) {
        // main has not constructed any thread or runtime; children must not inherit it.
        unsafe {
            std::env::remove_var(BOOTSTRAP);
        }
        let bootstrap: Bootstrap = serde_json::from_str(&value.to_string_lossy())?;
        ensure!(
            bootstrap.address.ip().is_loopback() && bootstrap.address.port() != 0,
            "invalid host launch channel"
        );
        let mut stream = TcpStream::connect_timeout(&bootstrap.address, Duration::from_secs(5))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_frame(
            &mut stream,
            &Hello {
                version: VERSION,
                launch: bootstrap.launch,
                secret: bootstrap.secret,
                process: std::process::id(),
            },
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let accepted = Frames::default().read::<WrapperNotice>(&mut stream)?;
        ensure!(
            matches!(accepted, Some(WrapperNotice::Accepted { launch, process }) if launch == bootstrap.launch && process == std::process::id()),
            "host launch was not acknowledged"
        );
        stream.set_read_timeout(None)?;
        let (retire, _) = watch::channel(false);
        let mut input = stream.try_clone()?;
        #[cfg(windows)]
        let signals = platform::ConsoleSignals::install()?;
        #[cfg(windows)]
        input.set_read_timeout(Some(Duration::from_millis(50)))?;
        let requested = retire.clone();
        std::thread::Builder::new()
            .name("host-launch".into())
            .spawn(move || {
                let mut frames = Frames::default();
                loop {
                    #[cfg(windows)]
                    if signals.retire() {
                        requested.send_replace(true);
                    }
                    match frames.read::<WrapperNotice>(&mut input) {
                        Ok(Some(WrapperNotice::Retire)) => {
                            requested.send_replace(true);
                        }
                        Ok(Some(WrapperNotice::Observed { .. })) => {}
                        Ok(Some(WrapperNotice::Accepted { .. })) => {
                            requested.send_replace(true);
                            return;
                        }
                        Ok(None) => continue,
                        #[cfg(windows)]
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                            ) => {}
                        Err(_) => {
                            requested.send_replace(true);
                            return;
                        }
                    }
                }
            })?;
        WORKER
            .set(Worker {
                output: Mutex::new(stream),
                retire,
            })
            .map_err(|_| anyhow::anyhow!("duplicate host launch"))?;
        return Ok(None);
    }
    if cli.cmd.is_some()
        || !crate::tui_mode_requested()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
    {
        return Ok(None);
    }
    let _shell_modes = platform::ShellModes::capture();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let bootstrap = Bootstrap {
        address: listener.local_addr()?,
        launch: Uuid::new_v4(),
        secret: Uuid::new_v4(),
    };
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .env(BOOTSTRAP, serde_json::to_string(&bootstrap)?);
    platform::configure_native_worker(&mut command)?;
    let mut child = command.spawn().context("start native runtime worker")?;
    supervise(listener, bootstrap, &mut child, &_shell_modes).map(Some)
}

fn supervise(
    listener: TcpListener,
    bootstrap: Bootstrap,
    child: &mut Child,
    _shell_modes: &platform::ShellModes,
) -> Result<i32> {
    let signals = Signals::install()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    #[cfg(unix)]
    let mut deadline = deadline;
    let mut candidate: Option<(TcpStream, Frames)> = None;
    let mut authenticated = false;
    let mut channel_lost = false;
    loop {
        #[cfg(unix)]
        {
            deadline += signals.suspend(child.id(), _shell_modes)?;
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let _ = atman_tui::terminal_guard::ensure_shell_mode();
            }
            return Ok(status.code().unwrap_or(1));
        }
        if !authenticated && Instant::now() >= deadline {
            // A malformed launch is never a successful retirement.
            let _ = child.kill();
            let _ = child.wait();
            let _ = atman_tui::terminal_guard::ensure_shell_mode();
            anyhow::bail!("native runtime worker did not authenticate its launch");
        }
        if candidate.is_none() && !channel_lost {
            match listener.accept() {
                Ok((stream, address)) if address.ip().is_loopback() => {
                    stream.set_nonblocking(true)?;
                    stream.set_nodelay(true)?;
                    candidate = Some((stream, Frames::default()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some((stream, frames)) = &mut candidate {
            if !authenticated {
                match frames.read::<Hello>(stream) {
                    Ok(Some(hello))
                        if hello.version == VERSION
                            && hello.launch == bootstrap.launch
                            && hello.secret == bootstrap.secret
                            && hello.process == child.id() =>
                    {
                        write_frame(
                            stream,
                            &WrapperNotice::Accepted {
                                launch: bootstrap.launch,
                                process: child.id(),
                            },
                        )?;
                        authenticated = true;
                    }
                    Ok(None) => {}
                    _ => {
                        candidate = None;
                        continue;
                    }
                }
            } else {
                match frames.read::<WorkerNotice>(stream) {
                    Ok(Some(WorkerNotice::Retired {
                        retirement,
                        exit_code,
                        ..
                    })) if matches!(exit_code, 0 | 1) => {
                        let _ = write_frame(stream, &WrapperNotice::Observed { retirement });
                        return Ok(exit_code);
                    }
                    Ok(Some(_)) => anyhow::bail!("invalid native UI retirement status"),
                    Ok(None) => {}
                    Err(error) => {
                        // EOF before retirement still waits for the actual worker result.
                        candidate = None;
                        channel_lost = true;
                        if error.kind() != io::ErrorKind::UnexpectedEof {
                            return Err(error.into());
                        }
                    }
                }
            }
        }
        if signals.resize() {
            signals.forward_resize(child.id());
        }
        if authenticated
            && signals.retire()
            && let Some((stream, _)) = &mut candidate
        {
            write_frame(stream, &WrapperNotice::Retire)?;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

impl Worker {
    pub fn retirement(&self) -> watch::Receiver<bool> {
        self.retire.subscribe()
    }

    /// The caller has already restored the terminal and printed the full applicable card.
    pub fn retire(
        &self,
        log: &std::fs::File,
        session: Option<Uuid>,
        summary_printed: bool,
        exit_code: i32,
    ) -> Result<()> {
        platform::detach_terminal_stdio(log)?;
        if let Err(error) = write_frame(
            &mut self.output.lock().unwrap(),
            &WorkerNotice::Retired {
                retirement: Uuid::new_v4(),
                session,
                summary_printed,
                exit_code,
            },
        ) {
            // Presentation is fully detached even if its wrapper has already disappeared.
            atman_runtime::notify!(
                warn,
                location = Log,
                "UI retirement acknowledgement unavailable: {error}"
            );
        }
        Ok(())
    }
}

fn write_frame(stream: &mut TcpStream, value: &impl Serialize) -> io::Result<()> {
    let mut frame = serde_json::to_vec(value).map_err(io::Error::other)?;
    if frame.len() >= MAX_FRAME {
        return Err(io::Error::other("host launch frame is too large"));
    }
    frame.push(b'\n');
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut remaining = frame.as_slice();
    while !remaining.is_empty() {
        match stream.write(remaining) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(length) => remaining = &remaining[length..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_frames_keep_partial_identity_and_reject_oversized_input() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let mut frames = Frames::default();
        sender.write_all(b"{\"kind\":").unwrap();
        assert!(
            frames
                .read::<WrapperNotice>(&mut receiver)
                .unwrap()
                .is_none()
        );
        receiver.set_nonblocking(false).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        sender
            .write_all(b"\"retire\"}\n{\"kind\":\"retire\"}\n")
            .unwrap();
        assert!(matches!(
            frames.read(&mut receiver).unwrap(),
            Some(WrapperNotice::Retire)
        ));
        assert!(matches!(
            frames.read(&mut receiver).unwrap(),
            Some(WrapperNotice::Retire)
        ));
        let retirement = Uuid::new_v4();
        write_frame(
            &mut sender,
            &WorkerNotice::Retired {
                retirement,
                session: None,
                summary_printed: false,
                exit_code: 1,
            },
        )
        .unwrap();
        assert!(matches!(
            frames.read::<WorkerNotice>(&mut receiver).unwrap(),
            Some(WorkerNotice::Retired { retirement: observed, exit_code: 1, .. }) if observed == retirement
        ));
        sender.write_all(&vec![b' '; MAX_FRAME]).unwrap();
        assert!(frames.read::<WrapperNotice>(&mut receiver).is_err());
    }
}

#[derive(Default)]
struct Frames {
    bytes: Vec<u8>,
}

impl Frames {
    fn read<T: for<'a> Deserialize<'a>>(
        &mut self,
        stream: &mut TcpStream,
    ) -> io::Result<Option<T>> {
        loop {
            if let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') {
                let value = serde_json::from_slice(&self.bytes[..end]).map_err(io::Error::other)?;
                self.bytes.drain(..=end);
                return Ok(Some(value));
            }
            if self.bytes.len() >= MAX_FRAME {
                return Err(io::Error::other("host launch frame is too large"));
            }
            let mut buffer = [0; 512];
            let available = buffer.len().min(MAX_FRAME - self.bytes.len());
            match stream.read(&mut buffer[..available]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(length) => self.bytes.extend_from_slice(&buffer[..length]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(windows)]
use platform::ConsoleSignals as Signals;
#[cfg(unix)]
use signals::Signals;
#[cfg(not(any(unix, windows)))]
struct Signals;
#[cfg(not(any(unix, windows)))]
impl Signals {
    fn install() -> io::Result<Self> {
        Ok(Self)
    }
    fn resize(&self) -> bool {
        false
    }
    fn retire(&self) -> bool {
        false
    }
    fn forward_resize(&self, _: u32) {}
}

#[cfg(unix)]
mod signals {
    use std::{
        io,
        sync::atomic::{AtomicU32, Ordering},
        time::{Duration, Instant},
    };
    static RECEIVED: AtomicU32 = AtomicU32::new(0);
    extern "C" fn receive(signal: i32) {
        RECEIVED.fetch_or(
            match signal {
                libc::SIGWINCH => 1,
                libc::SIGTSTP => 4,
                libc::SIGCONT => 8,
                _ => 2,
            },
            Ordering::Relaxed,
        );
    }
    pub(super) struct Signals(Vec<(i32, libc::sigaction)>);
    impl Signals {
        pub fn install() -> io::Result<Self> {
            let mut installed = Self(Vec::new());
            for signal in [
                libc::SIGWINCH,
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGTSTP,
                libc::SIGCONT,
            ] {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                let mut previous = unsafe { std::mem::zeroed() };
                action.sa_sigaction = receive as *const () as usize;
                unsafe {
                    libc::sigemptyset(&mut action.sa_mask);
                }
                if unsafe { libc::sigaction(signal, &action, &mut previous) } == -1 {
                    return Err(io::Error::last_os_error());
                }
                installed.0.push((signal, previous));
            }
            Ok(installed)
        }
        pub fn resize(&self) -> bool {
            RECEIVED.fetch_and(!1, Ordering::Relaxed) & 1 != 0
        }
        pub fn retire(&self) -> bool {
            RECEIVED.fetch_and(!2, Ordering::Relaxed) & 2 != 0
        }
        pub fn forward_resize(&self, process: u32) {
            unsafe {
                libc::kill(process as i32, libc::SIGWINCH);
            }
        }

        pub fn suspend(
            &self,
            process: u32,
            shell: &super::platform::ShellModes,
        ) -> io::Result<Duration> {
            if RECEIVED.fetch_and(!4, Ordering::Relaxed) & 4 == 0 {
                return Ok(Duration::ZERO);
            }
            RECEIVED.fetch_and(!8, Ordering::Relaxed);
            let started = Instant::now();
            if unsafe { libc::kill(-(process as i32), libc::SIGSTOP) } == -1 {
                return Err(io::Error::last_os_error());
            }
            struct Resume(u32);
            impl Drop for Resume {
                fn drop(&mut self) {
                    unsafe {
                        libc::kill(-(self.0 as i32), libc::SIGCONT);
                    }
                }
            }
            let resume = Resume(process);
            let mut state: libc::siginfo_t = unsafe { std::mem::zeroed() };
            loop {
                if unsafe {
                    libc::waitid(
                        libc::P_PID,
                        process,
                        &mut state,
                        libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT,
                    )
                } == 0
                {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            // Observe without reaping: Child retains the original exit receipt.
            if state.si_code != libc::CLD_STOPPED {
                return Ok(started.elapsed());
            }
            let ui = super::platform::ShellModes::capture();
            shell.restore()?;
            // The independent worker group must already be stopped before returning the tty to its shell.
            if RECEIVED.fetch_and(!8, Ordering::Relaxed) & 8 == 0
                && unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            ui.restore()?;
            drop(resume);
            self.forward_resize(process);
            Ok(started.elapsed())
        }
    }
    impl Drop for Signals {
        fn drop(&mut self) {
            for (signal, action) in &self.0 {
                unsafe {
                    libc::sigaction(*signal, action, std::ptr::null_mut());
                }
            }
        }
    }
}
