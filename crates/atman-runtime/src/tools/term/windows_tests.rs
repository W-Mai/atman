use super::*;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

const ROOT: &str = "ATMAN_TEST_TERM_LIFECYCLE_ROOT";
const CHILD: &str = "tools::term::windows_tests::native_terminal_lifecycle_child";
const MARKER: &str = "native_terminal_lifecycle_finished";
const DEADLINE: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Capture {
    output: Vec<u8>,
    input: Vec<u8>,
    input_chunks: Vec<Vec<u8>>,
    reader_closed: bool,
    reader_eof: bool,
    reader_error: Option<String>,
    writer_closed: bool,
    writer_error: Option<String>,
    close_started: bool,
    close_complete: bool,
}

struct Reader {
    inner: Option<Box<dyn Read + Send>>,
    capture: Arc<Mutex<Capture>>,
}

impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let result = self.inner.as_mut().unwrap().read(bytes);
        let mut capture = self.capture.lock().unwrap();
        match &result {
            Ok(0) => capture.reader_eof = true,
            Ok(length) => capture.output.extend_from_slice(&bytes[..*length]),
            Err(error) => capture.reader_error = Some(error.to_string()),
        }
        result
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.capture.lock().unwrap().reader_closed = true;
    }
}

struct Writer {
    inner: Option<Box<dyn Write + Send>>,
    capture: Arc<Mutex<Capture>>,
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let result = self.inner.as_mut().unwrap().write(bytes);
        let mut capture = self.capture.lock().unwrap();
        match &result {
            Ok(length) => {
                capture.input.extend_from_slice(&bytes[..*length]);
                capture.input_chunks.push(bytes[..*length].to_vec());
            }
            Err(error) => capture.writer_error = Some(error.to_string()),
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let result = self.inner.as_mut().unwrap().flush();
        if let Err(error) = &result {
            self.capture.lock().unwrap().writer_error = Some(error.to_string());
        }
        result
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.capture.lock().unwrap().writer_closed = true;
    }
}

struct Master {
    inner: Option<Box<dyn portable_pty::MasterPty + Send>>,
    capture: Arc<Mutex<Capture>>,
}

impl portable_pty::MasterPty for Master {
    fn resize(&self, size: portable_pty::PtySize) -> anyhow::Result<()> {
        self.inner.as_ref().unwrap().resize(size)
    }

    fn get_size(&self) -> anyhow::Result<portable_pty::PtySize> {
        self.inner.as_ref().unwrap().get_size()
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
        self.inner.as_ref().unwrap().try_clone_reader()
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
        self.inner.as_ref().unwrap().take_writer()
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.capture.lock().unwrap().close_started = true;
        drop(self.inner.take());
        self.capture.lock().unwrap().close_complete = true;
    }
}

fn diagnostic(
    entry: &TermEntry,
    tasks: &crate::task_registry::TaskRegistry,
    sink: &crate::event::EventSink,
    capture: &Mutex<Capture>,
) -> serde_json::Value {
    let process = entry.child.try_lock().map(|mut child| {
        child.as_mut().map(|child| {
            // WinChild.try_wait queries its retained process handle without reaping a PID.
            serde_json::json!({
                "pid": child.process_id(),
                "wait": format!("{:?}", child.try_wait()),
            })
        })
    });
    let finals: Vec<_> = sink
        .snapshot()
        .into_iter()
        .filter(|event| matches!(event, crate::event::Event::TerminalFinalState { .. }))
        .collect();
    let capture = capture.lock().unwrap();
    serde_json::json!({
        "handle": entry.handle.to_string(),
        "state": format!("{:?}", entry.current_state()),
        "child": format!("{process:?}"),
        "master": entry.master.try_lock().map(|master| master.is_some()).ok(),
        "writer": entry.writer.try_lock().map(|writer| writer.is_some()).ok(),
        "reader_task_finished": entry.reader_task.try_lock().ok()
            .and_then(|task| task.as_ref().map(JoinHandle::is_finished)),
        "task": tasks.lookup_by_handle(&entry.handle.to_string())
            .map(|task| format!("{:?}", task.status)),
        "output": String::from_utf8_lossy(&capture.output).escape_debug().to_string(),
        "input": String::from_utf8_lossy(&capture.input).escape_debug().to_string(),
        "input_chunks": capture.input_chunks.iter().map(|chunk| String::from_utf8_lossy(chunk).escape_debug().to_string()).collect::<Vec<_>>(),
        "reader_closed": capture.reader_closed,
        "reader_eof": capture.reader_eof,
        "reader_error": capture.reader_error,
        "writer_closed": capture.writer_closed,
        "writer_error": capture.writer_error,
        "close_started": capture.close_started,
        "close_complete": capture.close_complete,
        "finals": finals,
    })
}

async fn probe(root: &Path) -> bool {
    std::fs::write(root.join("stage.txt"), "opening native ConPTY").unwrap();
    let pair = crate::platform::with_process_creation(|| {
        portable_pty::native_pty_system().openpty(portable_pty::PtySize {
            rows: 5,
            cols: 40,
            pixel_width: 0,
            pixel_height: 0,
        })
    })
    .unwrap();
    std::fs::write(root.join("stage.txt"), "spawning original native child").unwrap();
    let shell = crate::shell::shell_command(Some(&format!("echo {MARKER}"))).unwrap();
    let parts = shell.string_parts();
    let mut command = portable_pty::CommandBuilder::new(&parts[0]);
    command.args(&parts[1..]);
    command.cwd(root);
    let child =
        crate::platform::with_process_creation(|| pair.slave.spawn_command(command)).unwrap();
    drop(pair.slave);
    std::fs::write(root.join("stage.txt"), "capturing original terminal pipes").unwrap();
    let mut pty = crate::sandbox::complete_pty_spawn(child, pair.master, None).unwrap();
    let capture = Arc::new(Mutex::new(Capture::default()));
    pty.reader = Box::new(Reader {
        inner: Some(pty.reader),
        capture: capture.clone(),
    });
    pty.writer = Box::new(Writer {
        inner: Some(pty.writer),
        capture: capture.clone(),
    });
    pty.master = Box::new(Master {
        inner: Some(pty.master),
        capture: capture.clone(),
    });
    let tasks = crate::task_registry::TaskRegistry::new();
    let registry = Arc::new(TermRegistry::new().with_task_registry(tasks.clone()));
    let sink = crate::event::EventSink::new();
    std::fs::write(
        root.join("stage.txt"),
        "installing original terminal registry",
    )
    .unwrap();
    let (handle, entry) = registry
        .spawn_entry(
            5,
            40,
            "native-terminal-probe".into(),
            root.to_path_buf(),
            pty,
            None,
            "native terminal lifecycle".into(),
            format!("echo {MARKER}"),
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
            Some(sink.clone()),
            None,
        )
        .unwrap();
    std::fs::write(
        root.join("stage.txt"),
        "observing original child and pipe retirement",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = serde_json::Value::Null;
    loop {
        let report = diagnostic(&entry, &tasks, &sink, &capture);
        if report != previous {
            std::fs::write(
                root.join("state.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            previous = report;
        }
        let completed = sink.snapshot().iter().any(|event| {
            matches!(event, crate::event::Event::TerminalFinalState {
                handle: original,
                screen,
                state: TermStateSnapshot::Exited { exit_code: Some(0) },
            } if original == &handle.to_string()
                && screen.cells.iter().map(|cell| cell.chars.as_str()).collect::<String>().contains(MARKER))
        });
        if completed
            && tasks.lookup_by_handle(&handle.to_string()).unwrap().status
                == crate::task_registry::TaskStatus::Ok
            && entry
                .reader_task
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished()
        {
            let capture = capture.lock().unwrap();
            assert!(capture.reader_closed && capture.writer_closed && capture.close_complete);
            assert!(
                capture
                    .output
                    .windows(MARKER.len())
                    .any(|bytes| bytes == MARKER.as_bytes())
            );
            if capture.output.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                assert!(capture.input.windows(6).any(|bytes| bytes == b"\x1b[1;1R"));
            }
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("native terminal did not complete: {previous}");
            registry.close();
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
fn native_terminal_lifecycle_child() {
    let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let completed = runtime.block_on(probe(&root));
    // A failed probe remains a failure even if its owned blocking worker cannot retire.
    runtime.shutdown_timeout(Duration::from_millis(250));
    assert!(completed, "native terminal lifecycle probe failed");
}

struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn native_terminal_lifecycle_preserves_original_exit_and_pipe_completion() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let log = std::fs::File::create(root.join("helper.log")).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--nocapture", "--test-threads=1"])
        .env(ROOT, root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut helper = Helper(crate::platform::with_process_creation(|| command.spawn()).unwrap());
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = helper.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let state = std::fs::read_to_string(root.join("state.json")).unwrap_or_default();
    let stage = std::fs::read_to_string(root.join("stage.txt")).unwrap_or_default();
    let log = std::fs::read_to_string(root.join("helper.log")).unwrap();
    eprintln!("native terminal OS evidence: stage={stage}\n{state}");
    assert!(
        status.is_some_and(|status| status.success()),
        "native terminal helper status={status:?}; stage={stage}\n{state}\n{log}"
    );
}
