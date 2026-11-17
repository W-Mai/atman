#![cfg(windows)]

use std::{
    fs,
    io::{self, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize};
use windows_sys::Win32::{
    Foundation::{GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{
            CTRL_BREAK_EVENT, CTRL_C_EVENT, GenerateConsoleCtrlEvent, GetStdHandle,
            STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::{
            CREATE_NEW_CONSOLE, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
        },
    },
};

#[path = "../src/host_process/platform.rs"]
mod platform;

const ROOT: &str = "_ATMAN_WINDOWS_PLATFORM_TEST_ROOT";
const ROLE: &str = "_ATMAN_WINDOWS_PLATFORM_TEST_ROLE";
const DEADLINE: Duration = Duration::from_secs(15);

fn publish(root: &Path, name: &str, value: &str) {
    let temporary = root.join(format!(".{name}.tmp"));
    fs::write(&temporary, value).unwrap();
    fs::rename(temporary, root.join(name)).unwrap();
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing test receipt: {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn helper_command(root: &Path, role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "windows_host_platform_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, root)
        .env(ROLE, role)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

struct Helper(Child);

impl Helper {
    fn start(root: &Path, role: &str) -> Self {
        let mut command = helper_command(root, role);
        command.creation_flags(CREATE_NEW_CONSOLE);
        Self(command.spawn().unwrap())
    }

    fn wait_success(&mut self, root: &Path) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "platform helper failed: {status}; receipt={:?}",
                    fs::read_to_string(root.join("error"))
                );
                return;
            }
            assert!(Instant::now() < deadline, "platform helper did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Process(OwnedHandle);

impl Process {
    fn open(process: u32) -> Self {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                process,
            )
        };
        assert!(
            !handle.is_null(),
            "cannot observe owned helper {process}: {}",
            std::io::Error::last_os_error()
        );
        Self(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn alive(&self) -> bool {
        match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => true,
            WAIT_OBJECT_0 => false,
            other => panic!("owned process wait failed: {other}"),
        }
    }

    fn wait_exit(&self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), DEADLINE.as_millis() as u32) },
            WAIT_OBJECT_0,
            "owned worker did not terminate"
        );
    }

    fn exit_code(&self) -> u32 {
        let mut code = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(self.0.as_raw_handle(), &mut code) },
            0
        );
        code
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.alive() {
            unsafe {
                TerminateProcess(self.0.as_raw_handle(), 99);
                WaitForSingleObject(self.0.as_raw_handle(), 2000);
            }
        }
    }
}

fn create_job(limits: u32) -> OwnedHandle {
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!handle.is_null(), "cannot create the native test Job");
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut handle_flags = 0;
    assert_ne!(
        unsafe { GetHandleInformation(job.as_raw_handle(), &mut handle_flags) },
        0
    );
    assert_eq!(
        handle_flags & HANDLE_FLAG_INHERIT,
        0,
        "test Job handle must remain parent-owned"
    );
    let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    information.BasicLimitInformation.LimitFlags = limits;
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&information).cast(),
                std::mem::size_of_val(&information) as u32,
            )
        },
        0,
        "cannot configure the native test Job: {}",
        std::io::Error::last_os_error()
    );
    job
}

fn assign_job(job: &OwnedHandle, helper: &Helper) {
    assert_ne!(
        unsafe { AssignProcessToJobObject(job.as_raw_handle(), helper.0.as_raw_handle()) },
        0,
        "cannot assign the isolated helper to its test Job; enclosing Job policy: {}",
        std::io::Error::last_os_error()
    );
}

fn in_job(process: HANDLE, job: HANDLE) -> bool {
    let mut member = 0;
    assert_ne!(unsafe { IsProcessInJob(process, job, &mut member) }, 0);
    member != 0
}

fn check_job_survival(limits: u32, expected_membership: bool) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let job = create_job(limits);
    let mut helper = Helper::start(root, "job-source");
    assign_job(&job, &helper);
    fs::write(root.join("start"), "go").unwrap();
    wait_for(&root.join("worker"));
    let process: u32 = fs::read_to_string(root.join("worker"))
        .unwrap_or_else(|error| {
            panic!(
                "original platform launch did not produce a worker: {error}; {:?}",
                fs::read_to_string(root.join("error"))
            )
        })
        .parse()
        .unwrap();
    let worker = Process::open(process);
    publish(root, "observed", "verified");
    helper.wait_success(root);
    assert_eq!(
        in_job(worker.0.as_raw_handle(), job.as_raw_handle()),
        expected_membership,
        "worker membership does not reflect the allowed breakaway policy"
    );
    drop(job);
    assert!(
        worker.alive(),
        "closing the source Job terminated its worker"
    );
    fs::write(root.join("release"), "go").unwrap();
    worker.wait_exit();
    assert!(root.join("complete").exists());
}

#[test]
fn nonrestrictive_job_preserves_native_worker_lifetime() {
    check_job_survival(0, true);
}

#[test]
fn inherited_kill_on_close_job_preserves_worker_until_its_last_handle_closes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let job = create_job(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE);
    let mut helper = Helper::start(root, "job-held-source");
    assign_job(&job, &helper);
    fs::write(root.join("start"), "go").unwrap();
    wait_for(&root.join("worker"));
    let worker = Process::open(
        fs::read_to_string(root.join("worker"))
            .unwrap()
            .parse()
            .unwrap(),
    );
    publish(root, "observed", "verified");
    helper.wait_success(root);
    assert!(in_job(worker.0.as_raw_handle(), job.as_raw_handle()));
    assert_eq!(
        unsafe { WaitForSingleObject(worker.0.as_raw_handle(), 200) },
        WAIT_TIMEOUT,
        "wrapper exit terminated a worker while the actual Job handle remained open"
    );
    assert!(!root.join("error").exists());
    drop(job);
    worker.wait_exit();
    assert_ne!(
        worker.exit_code(),
        101,
        "harness failure cannot establish final Job-handle termination"
    );
    assert!(
        !root.join("error").exists(),
        "helper error cannot establish final Job-handle termination"
    );
    assert!(!root.join("complete").exists());
}

#[test]
fn explicit_and_silent_breakaway_survive_the_original_job_closing() {
    for permission in [
        JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    ] {
        check_job_survival(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | permission, false);
    }
}

#[test]
fn native_control_c_and_break_request_retirement_without_terminating_the_helper() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut helper = Helper::start(root, "console-controls");
    fs::write(root.join("start"), "go").unwrap();
    helper.wait_success(root);
    assert_eq!(
        fs::read_to_string(root.join("controls")).unwrap(),
        "verified"
    );
}

struct ConPty {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: mpsc::Receiver<()>,
    capture: Arc<Mutex<ConPtyCapture>>,
}

#[derive(Default)]
struct ConPtyCapture {
    bytes: Vec<u8>,
    cursor_replies: usize,
    error: Option<String>,
}

fn reply_cursor_queries(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    writer: &mut dyn Write,
) -> io::Result<usize> {
    const QUERY: &[u8] = b"\x1b[6n";
    pending.extend_from_slice(bytes);
    let mut consumed = 0;
    let mut replies = 0;
    while let Some(offset) = pending[consumed..]
        .windows(QUERY.len())
        .position(|window| window == QUERY)
    {
        writer.write_all(b"\x1b[1;1R")?;
        consumed += offset + QUERY.len();
        replies += 1;
    }
    let retired = consumed.max(pending.len().saturating_sub(QUERY.len() - 1));
    pending.drain(..retired);
    Ok(replies)
}

impl ConPty {
    fn start(root: &Path, role: &str) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (send, output) = mpsc::channel();
        let capture = Arc::new(Mutex::new(ConPtyCapture::default()));
        let observed = Arc::clone(&capture);
        // INHERIT_CURSOR requires a live terminal responder before client startup.
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            let mut pending = Vec::new();
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(length) => {
                        observed
                            .lock()
                            .unwrap()
                            .bytes
                            .extend_from_slice(&buffer[..length]);
                        match reply_cursor_queries(&mut pending, &buffer[..length], &mut writer) {
                            Ok(replies) => observed.lock().unwrap().cursor_replies += replies,
                            Err(error) => {
                                observed.lock().unwrap().error = Some(error.to_string());
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        observed.lock().unwrap().error = Some(error.to_string());
                        break;
                    }
                }
            }
            drop(reader);
            drop(writer);
            let _ = send.send(());
        });
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "windows_host_platform_child",
            "--nocapture",
            "--test-threads=1",
        ]);
        command.env(ROOT, root);
        command.env(ROLE, role);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        Self {
            master: Some(pair.master),
            child,
            output,
            capture,
        }
    }

    fn wait_ready(&mut self, root: &Path, role: &str) {
        let deadline = Instant::now() + DEADLINE;
        while !root.join("worker").exists() {
            let status = self.child.try_wait().unwrap();
            if status.is_some() || root.join("error").exists() || Instant::now() >= deadline {
                let message = {
                    let capture = self.capture.lock().unwrap();
                    format!(
                        "ConPTY helper not ready: role={role}, status={status:?}, error={:?}, cursor_replies={}, terminal_error={:?}, output={:?}",
                        fs::read_to_string(root.join("error")),
                        capture.cursor_replies,
                        capture.error,
                        String::from_utf8_lossy(&capture.bytes)
                    )
                };
                panic!("{message}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn close(&mut self) -> Vec<u8> {
        let master = self.master.take().unwrap();
        let (send, closed) = mpsc::channel();
        // ClosePseudoConsole can wait for output drainage; keep its reader active.
        std::thread::spawn(move || {
            drop(master);
            let _ = send.send(());
        });
        closed
            .recv_timeout(DEADLINE)
            .expect("ConPTY close did not finish");
        self.output
            .recv_timeout(DEADLINE)
            .expect("ConPTY output did not close");
        std::mem::take(&mut self.capture.lock().unwrap().bytes)
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(master) = self.master.take() {
            std::thread::spawn(move || drop(master));
        }
    }
}

#[test]
fn closing_conpty_terminates_attached_but_preserves_retired_platform_worker() {
    for role in [
        "conpty-attached",
        "conpty-retired",
        "conpty-retired-missing-stderr",
    ] {
        let retired = role != "conpty-attached";
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut terminal = ConPty::start(root, role);
        fs::write(root.join("start"), "go").unwrap();
        terminal.wait_ready(root, role);
        let worker = Process::open(
            fs::read_to_string(root.join("worker"))
                .unwrap()
                .parse()
                .unwrap(),
        );
        let output = terminal.close();
        if retired {
            assert!(
                worker.alive(),
                "retired worker died with its original ConPTY"
            );
            let output = String::from_utf8_lossy(&output);
            for marker in [
                "retired rust stdout",
                "retired rust stderr",
                "retired CRT stdout",
                "retired CRT stderr",
            ] {
                assert!(
                    !output.contains(marker),
                    "post-retirement output escaped: {marker}"
                );
                assert!(
                    fs::read_to_string(root.join("diagnostics.log"))
                        .unwrap()
                        .contains(marker)
                );
            }
            fs::write(root.join("release"), "go").unwrap();
            worker.wait_exit();
            assert!(root.join("complete").exists());
        } else {
            worker.wait_exit();
            let status = terminal
                .child
                .try_wait()
                .unwrap()
                .expect("terminated helper has no original process status");
            assert_ne!(
                status.exit_code(),
                101,
                "Rust harness failure cannot establish console destruction"
            );
            assert!(
                !root.join("error").exists(),
                "helper error cannot establish console destruction: {:?}",
                fs::read_to_string(root.join("error"))
            );
            assert!(
                !root.join("complete").exists(),
                "console destruction fabricated normal completion"
            );
        }
    }
}

#[test]
fn windows_host_platform_child() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    wait_for(&root.join("start"));
    let result = std::panic::catch_unwind(|| run_child(&root, role.to_str().unwrap()));
    if let Err(error) = result {
        let message = error
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "platform helper assertion failed".into());
        let _ = fs::write(root.join("error"), &message);
        std::panic::resume_unwind(error);
    }
}

fn run_child(root: &Path, role: &str) {
    match role {
        "job-source" | "job-held-source" => {
            let worker_role = if role == "job-held-source" {
                "job-held-worker"
            } else {
                "job-worker"
            };
            let mut command = helper_command(root, worker_role);
            platform::configure_native_worker(&mut command).unwrap();
            let mut worker = command.spawn().unwrap();
            let deadline = Instant::now() + DEADLINE;
            while !root.join("worker").exists() {
                assert!(
                    worker.try_wait().unwrap().is_none(),
                    "worker exited before its original platform readiness receipt"
                );
                assert!(
                    Instant::now() < deadline,
                    "worker never published its original platform readiness receipt"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            // Retain the original Child handle until the parent has its own handle.
            wait_for(&root.join("observed"));
        }
        "job-worker" | "job-held-worker" => {
            publish(root, "worker", &std::process::id().to_string());
            if role == "job-held-worker" {
                // Only the actual final Job-handle close or parent teardown may end it.
                loop {
                    std::thread::park();
                }
            }
            wait_for(&root.join("release"));
            publish(root, "complete", "verified");
        }
        "console-controls" => {
            let _modes = platform::ShellModes::capture();
            let signals = platform::ConsoleSignals::install().unwrap();
            assert!(!signals.resize());
            signals.forward_resize(std::process::id());
            for event in [CTRL_C_EVENT, CTRL_BREAK_EVENT] {
                assert!(!signals.retire());
                assert_ne!(unsafe { GenerateConsoleCtrlEvent(event, 0) }, 0);
                let deadline = Instant::now() + DEADLINE;
                while !signals.retire() {
                    assert!(
                        Instant::now() < deadline,
                        "console event {event} was not observed"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            publish(root, "controls", "verified");
        }
        "conpty-attached" | "conpty-retired" | "conpty-retired-missing-stderr" => {
            let _signals = platform::ConsoleSignals::install().unwrap();
            if role != "conpty-attached" {
                unsafe extern "C" {
                    fn _write(fd: i32, bytes: *const u8, length: u32) -> i32;
                    fn _close(fd: i32) -> i32;
                    fn _get_osfhandle(fd: i32) -> isize;
                    fn _get_errno(value: *mut i32) -> i32;
                    fn _get_doserrno(value: *mut u32) -> i32;
                }
                let crt_before = [0, 1, 2].map(|descriptor| unsafe { _get_osfhandle(descriptor) });
                let standard_before = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
                    .map(|standard| unsafe { GetStdHandle(standard) } as isize);
                if role == "conpty-retired-missing-stderr" {
                    assert_eq!(unsafe { _close(2) }, 0);
                }
                let log = fs::OpenOptions::new()
                    .create_new(true)
                    .append(true)
                    .open(root.join("diagnostics.log"))
                    .unwrap();
                platform::detach_terminal_stdio(&log).unwrap();
                let crt_after = [0, 1, 2].map(|descriptor| unsafe { _get_osfhandle(descriptor) });
                let standard_after = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
                    .map(|standard| unsafe { GetStdHandle(standard) } as isize);
                println!("retired rust stdout");
                eprintln!("retired rust stderr");
                for (descriptor, marker) in
                    [(1, b"retired CRT stdout\n"), (2, b"retired CRT stderr\n")]
                {
                    let written =
                        unsafe { _write(descriptor, marker.as_ptr(), marker.len() as u32) };
                    let mut errno = 0;
                    let mut dos_errno = 0;
                    unsafe {
                        _get_errno(&mut errno);
                        _get_doserrno(&mut dos_errno);
                    }
                    assert_eq!(
                        written,
                        marker.len() as i32,
                        "CRT write failed: fd={descriptor}, errno={errno}, dos_errno={dos_errno}, crt_before={crt_before:x?}, crt_after={crt_after:x?}, standard_before={standard_before:x?}, standard_after={standard_after:x?}"
                    );
                }
                std::io::stdout().flush().unwrap();
                std::io::stderr().flush().unwrap();
            }
            publish(root, "worker", &std::process::id().to_string());
            if role == "conpty-attached" {
                // Only the parent's close/deadline/teardown can end this process.
                // A helper timeout must never masquerade as OS console destruction.
                loop {
                    std::thread::park();
                }
            }
            wait_for(&root.join("release"));
            publish(root, "complete", "verified");
        }
        _ => panic!("unknown isolated platform helper role"),
    }
}
