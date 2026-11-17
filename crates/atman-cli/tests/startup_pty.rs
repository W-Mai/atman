use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

use atman_runtime::flow_store::RuntimeStore;
use portable_pty::{CommandBuilder, PtySize};

fn open_terminal(root: &Path, arguments: &[String], title: &str) -> uuid::Uuid {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 36,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_atman"));
    command.args(arguments);
    command.cwd(root);
    command.env("ATMAN_DATA_DIR", root.join("data"));
    command.env("ATMAN_CONFIG_DIR", root.join("config"));
    command.env("ATMAN_DISABLE_MIGRATION", "1");
    command.env("ATMAN_TUI", "1");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for key in ["ATMAN_REPL_NON_INTERACTIVE", "ATMAN_NO_TUI", "NO_COLOR"] {
        command.env_remove(key);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(length) = reader.read(&mut buffer) {
            if length == 0 || send.send(buffer[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut parser = vt100::Parser::new(36, 120, 0);
    let queries: &[(&[u8], &[u8])] = &[
        (b"\x1b[6n", b"\x1b[1;1R"),
        (b"\x1b[c", b"\x1b[?1;2c"),
        (b"\x1b[?u", b"\x1b[?0u"),
        (b"\x1b]10;?\x07", b"\x1b]10;rgb:dddd/dddd/dddd\x07"),
        (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x07"),
    ];
    let mut answered = vec![0; queries.len()];
    let mut output = Vec::new();
    let mut selected = None;
    let mut onboarding_skipped = false;
    let mut submitted = false;
    let mut exiting = false;
    let mut rpc_verified = false;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let project_root = root.canonicalize().unwrap();
    let outcome = (|| -> Result<uuid::Uuid, String> {
        loop {
            if let Ok(bytes) = receive.recv_timeout(Duration::from_millis(25)) {
                parser.process(&bytes);
                output.extend(bytes);
                for (index, (query, reply)) in queries.iter().enumerate() {
                    while let Some(position) = output[answered[index]..]
                        .windows(query.len())
                        .position(|window| window == *query)
                    {
                        writer.write_all(reply).map_err(|error| error.to_string())?;
                        answered[index] += position + query.len();
                    }
                }
            }
            let screen = parser.screen().contents();
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return if status.success() && exiting {
                    selected.ok_or_else(|| "startup never installed a live Session".into())
                } else {
                    Err(format!(
                        "startup exited before acknowledgement: {status:?}\n{screen}"
                    ))
                };
            }
            if Instant::now() >= deadline || output.len() > 2_000_000 {
                return Err(format!(
                    "startup did not acknowledge terminal input\n{screen}"
                ));
            }
            if screen.contains("os error 33") || screen.contains("View disconnected") {
                return Err(screen);
            }
            if screen.contains("Welcome to atman") {
                if !onboarding_skipped {
                    writer.write_all(b"q").map_err(|error| error.to_string())?;
                    onboarding_skipped = true;
                }
                continue;
            }
            if selected.is_none() {
                for id in RuntimeStore::session_ids(&root.join("data"))
                    .map_err(|error| error.to_string())?
                {
                    let observation = RuntimeStore::inspect_session(&root.join("data"), id)
                        .map_err(|error| format!("startup snapshot failed: {error}\n{screen}"))?;
                    if observation
                        .projection
                        .project
                        .as_ref()
                        .is_some_and(|project| project.root() == project_root)
                    {
                        selected = Some(id);
                        break;
                    }
                }
            }
            let Some(id) = selected else { continue };
            // The boot animation uses a placeholder identity and cannot accept commands.
            if !submitted
                && (onboarding_skipped || !arguments.is_empty())
                && screen.contains("/ Flow ")
                && screen.contains(&id.to_string()[..8])
            {
                if !rpc_verified {
                    rpc_verified = runtime.block_on(check_native_rpc(root, id))?;
                    if !rpc_verified {
                        continue;
                    }
                }
                writer
                    .write_all(format!(":rename {title}\r").as_bytes())
                    .map_err(|error| error.to_string())?;
                submitted = true;
            }
            if submitted && !exiting {
                let observation = RuntimeStore::inspect_session(&root.join("data"), id)
                    .map_err(|error| format!("live snapshot failed: {error}\n{screen}"))?;
                if observation.projection.title.as_deref() == Some(title) {
                    writer
                        .write_all(b"\x04")
                        .map_err(|error| error.to_string())?;
                    exiting = true;
                }
            }
        }
    })();
    let _ = child.kill();
    drop(writer);
    drop(pair.master);
    let _ = child.wait();
    reader.join().unwrap();
    outcome.unwrap_or_else(|error| panic!("{error}"))
}

async fn check_native_rpc(root: &Path, id: uuid::Uuid) -> Result<bool, String> {
    use atman_proto::{
        capabilities::CapabilitiesRequest,
        flows::{ControlFlowRequest, FlowAddress, FlowControl, FlowRequest},
    };
    let records = atman_daemon::host_directory::discover(&root.join("data"))
        .map_err(|error| error.to_string())?;
    let Some(record) = records.into_iter().find(|record| {
        record.role == atman_proto::hosts::HostRole::Tui && record.sessions.contains(&id)
    }) else {
        return Ok(false);
    };
    let config = std::fs::read_to_string(root.join("config/daemon.toml"))
        .map_err(|error| error.to_string())?;
    let config: atman_daemon::config::DaemonConfig =
        toml::from_str(&config).map_err(|error| error.to_string())?;
    let endpoint = format!("http://{}", record.endpoint);
    let client = atman_client::HttpClient::new(&endpoint, config.auth_token)
        .map_err(|error| error.to_string())?;
    let capabilities = client
        .capabilities(CapabilitiesRequest::default())
        .await
        .map_err(|error| error.to_string())?;
    if capabilities.host_generation != record.generation.to_string() {
        return Err("native RPC generation differs from discovery".into());
    }
    let selected = RuntimeStore::inspect_session(&root.join("data"), id)
        .map_err(|error| error.to_string())?
        .projection
        .default_flow
        .ok_or_else(|| "native Session has no default Flow".to_owned())?;
    let flow = FlowAddress {
        session_id: atman_proto::SessionId(id),
        instance_id: selected.0.0,
    };
    let view = client
        .get_flow(FlowRequest { flow: flow.clone() })
        .await
        .map_err(|error| error.to_string())?;
    if !view.execution.can_start_turn {
        return Ok(false);
    }
    let request = uuid::Uuid::now_v7();
    let pause = client
        .control(ControlFlowRequest {
            flow: flow.clone(),
            request_id: request,
            expected_revision: view.execution.revision,
            action: FlowControl::Pause,
            pause_key: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    let retry = client
        .control(ControlFlowRequest {
            flow: flow.clone(),
            request_id: request,
            expected_revision: view.execution.revision,
            action: FlowControl::Pause,
            pause_key: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    if pause != retry {
        return Err("native RPC did not retain its original control receipt".into());
    }
    let view = client
        .get_flow(FlowRequest { flow: flow.clone() })
        .await
        .map_err(|error| error.to_string())?;
    let inspected = RuntimeStore::inspect_session(&root.join("data"), id)
        .map_err(|error| error.to_string())?
        .inspect_flow(&selected)
        .map_err(|error| error.to_string())?;
    let user_pause = uuid::Uuid::from_bytes(*b"atman-user-pause");
    if !inspected
        .state
        .pauses()
        .iter()
        .any(|key| key.0 == user_pause)
        || !view
            .execution
            .pause_keys
            .iter()
            .any(|key| key.0 == user_pause)
    {
        return Err("RPC control did not reach the native writer".into());
    }
    let competing = RuntimeStore::open(&root.join("data")).map_err(|error| error.to_string())?;
    if competing.open_session(id).is_ok() {
        return Err("RPC acquired a second Session writer".into());
    }
    client
        .control(ControlFlowRequest {
            flow,
            request_id: uuid::Uuid::now_v7(),
            expected_revision: view.execution.revision,
            action: FlowControl::Resume,
            pause_key: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    Ok(true)
}

#[test]
fn fresh_profile_accepts_terminal_input_and_reopens_the_same_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let id = open_terminal(root, &[], "Startup first");
    let restored = open_terminal(
        root,
        &["--continue".into(), id.to_string()],
        "Startup restored",
    );
    assert_eq!(restored, id);
    let observation = RuntimeStore::inspect_session(&root.join("data"), id).unwrap();
    assert_eq!(
        observation.projection.title.as_deref(),
        Some("Startup restored")
    );
}

struct NativeTerminal {
    root: std::path::PathBuf,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    receive: mpsc::Receiver<Option<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    #[cfg(unix)]
    reader_stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    parser: vt100::Parser,
    output: Vec<u8>,
    answered: [usize; 5],
    eof: bool,
}

impl NativeTerminal {
    fn start(root: &Path) -> Self {
        Self::start_args(root, &[])
    }

    fn start_args(root: &Path, arguments: &[String]) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 36,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_atman"));
        command.args(arguments);
        command.cwd(root);
        for (key, value) in [
            ("ATMAN_DATA_DIR", root.join("data")),
            ("ATMAN_CONFIG_DIR", root.join("config")),
        ] {
            command.env(key, value);
        }
        for (key, value) in [
            ("ATMAN_DISABLE_MIGRATION", "1"),
            ("ATMAN_TUI", "1"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ] {
            command.env(key, value);
        }
        for key in [
            "ATMAN_REPL_NON_INTERACTIVE",
            "ATMAN_NO_TUI",
            "NO_COLOR",
            "_ATMAN_HOST_LAUNCH",
        ] {
            command.env_remove(key);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        #[cfg(unix)]
        let writer: Box<dyn Write + Send> = {
            use std::os::fd::FromRawFd;
            // portable-pty's writer destructor injects VEOF; a real terminal loss must not.
            let descriptor =
                unsafe { libc::fcntl(pair.master.as_raw_fd().unwrap(), libc::F_DUPFD_CLOEXEC, 0) };
            assert!(
                descriptor >= 0,
                "duplicate native PTY writer: {}",
                std::io::Error::last_os_error()
            );
            Box::new(unsafe { std::fs::File::from_raw_fd(descriptor) })
        };
        #[cfg(not(unix))]
        let writer = pair.master.take_writer().unwrap();
        let mut source = pair.master.try_clone_reader().unwrap();
        #[cfg(unix)]
        let descriptor = pair.master.as_raw_fd().unwrap();
        #[cfg(unix)]
        let reader_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(unix)]
        let stopped = reader_stop.clone();
        let (send, receive) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                #[cfg(unix)]
                {
                    if stopped.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let mut readiness = libc::pollfd {
                        fd: descriptor,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut readiness, 1, 50) };
                    if ready == 0 {
                        continue;
                    }
                    if ready < 0 {
                        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        let _ = send.send(None);
                        break;
                    }
                }
                match source.read(&mut bytes) {
                    Ok(0) | Err(_) => {
                        let _ = send.send(None);
                        break;
                    }
                    Ok(length) => {
                        if send.send(Some(bytes[..length].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            root: root.to_owned(),
            master: Some(pair.master),
            child,
            writer,
            receive,
            reader: Some(reader),
            #[cfg(unix)]
            reader_stop,
            parser: vt100::Parser::new(36, 120, 1000),
            output: Vec::new(),
            answered: [0; 5],
            eof: false,
        }
    }

    fn pump(&mut self) {
        if self.eof {
            std::thread::sleep(Duration::from_millis(25));
            return;
        }
        if let Ok(event) = self.receive.recv_timeout(Duration::from_millis(25)) {
            match event {
                None => self.eof = true,
                Some(bytes) => {
                    self.parser.process(&bytes);
                    self.output.extend(bytes);
                }
            }
        }
        let queries: &[(&[u8], &[u8])] = &[
            (b"\x1b[6n", b"\x1b[1;1R"),
            (b"\x1b[c", b"\x1b[?1;2c"),
            (b"\x1b[?u", b"\x1b[?0u"),
            (b"\x1b]10;?\x07", b"\x1b]10;rgb:dddd/dddd/dddd\x07"),
            (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x07"),
        ];
        for (index, (query, reply)) in queries.iter().enumerate() {
            while let Some(position) = self.output[self.answered[index]..]
                .windows(query.len())
                .position(|window| window == *query)
            {
                self.writer.write_all(reply).unwrap();
                self.answered[index] += position + query.len();
            }
        }
        assert!(
            self.output.len() < 4_000_000,
            "unexpected terminal output volume"
        );
    }

    fn ready(&mut self, expect_onboarding: bool) -> uuid::Uuid {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut onboarding_seen = false;
        loop {
            self.pump();
            let screen = self.parser.screen().contents();
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "native launcher exited during startup:\n{screen}"
            );
            assert!(
                Instant::now() < deadline,
                "native UI did not become ready:\n{screen}\n{}",
                self.terminal_diagnostics()
            );
            if screen.contains("Welcome to atman") {
                assert!(
                    expect_onboarding,
                    "configured native fixture unexpectedly requires onboarding:\n{screen}"
                );
                if !onboarding_seen {
                    self.writer.write_all(b"q").unwrap();
                    onboarding_seen = true;
                }
                continue;
            }
            if expect_onboarding && !onboarding_seen {
                continue;
            }
            for id in RuntimeStore::session_ids(&self.root.join("data")).unwrap_or_default() {
                let Ok(session) = RuntimeStore::inspect_session(&self.root.join("data"), id) else {
                    continue;
                };
                if session
                    .projection
                    .project
                    .as_ref()
                    .is_some_and(|project| project.root() == self.root.canonicalize().unwrap())
                    && screen.contains("/ Flow ")
                    && screen.contains(&id.to_string()[..8])
                    && self.synchronized_frame_complete()
                {
                    return id;
                }
            }
        }
    }

    fn synchronized_frame_complete(&self) -> bool {
        let begin = self
            .output
            .windows(b"\x1b[?2026h".len())
            .rposition(|bytes| bytes == b"\x1b[?2026h");
        let end = self
            .output
            .windows(b"\x1b[?2026l".len())
            .rposition(|bytes| bytes == b"\x1b[?2026l");
        matches!((begin, end), (Some(begin), Some(end)) if end > begin)
    }

    fn terminal_diagnostics(&self) -> String {
        let count = |sequence: &[u8]| {
            self.output
                .windows(sequence.len())
                .filter(|bytes| *bytes == sequence)
                .count()
        };
        let screen = self.parser.screen();
        let tail = &self.output[self.output.len().saturating_sub(1024)..];
        format!(
            "VT output: bytes={}, synchronized_begin={}, synchronized_end={}, cursor={:?}, hidden={}\nVT tail: {}",
            self.output.len(),
            count(b"\x1b[?2026h"),
            count(b"\x1b[?2026l"),
            screen.cursor_position(),
            screen.hide_cursor(),
            tail.escape_ascii(),
        )
    }

    fn submit_visible_draft(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.pump();
            let screen = self.parser.screen().contents();
            if screen.contains(text)
                && self.synchronized_frame_complete()
                && ![
                    "Waiting",
                    "Reading history",
                    "Preparing view",
                    "Loading display",
                    "Opening conversation",
                ]
                .iter()
                .any(|phase| screen.contains(phase))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "original composer did not display its single authored draft:\n{screen}\n{}",
                self.terminal_diagnostics()
            );
        }
        self.writer.write_all(b"\r").unwrap();
    }

    fn retire(&mut self, session: uuid::Uuid) {
        self.writer.write_all(b"\x04").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "launcher retirement failed:\n{}",
                    self.parser.screen().contents()
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "UI waited for active execution instead of retiring:\n{}",
                self.parser.screen().contents()
            );
        }
        while !self.eof && Instant::now() < deadline {
            self.pump();
        }
        assert!(
            self.eof,
            "retired worker retained a PTY descriptor; the shell stream never closed"
        );
        let card = self.parser.screen().contents();
        for label in ["USAGE", "WORK", "atman --continue", &session.to_string()] {
            assert!(card.contains(label), "exit card lost {label}:\n{card}");
        }
        assert_eq!(
            card.matches("atman --continue").count(),
            1,
            "exit card was printed more than once:\n{card}"
        );
    }

    #[cfg(unix)]
    fn lose_wrapper(&mut self) {
        let process = self.child.process_id().unwrap();
        assert_eq!(unsafe { libc::kill(process as i32, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.signal().is_some(),
                    "wrapper was not killed: {status:?}"
                );
                break;
            }
            assert!(Instant::now() < deadline, "wrapper death was not observed");
        }
        while !self.eof && Instant::now() < deadline {
            self.pump();
        }
        assert!(
            self.eof,
            "original worker kept reading or holding the retired PTY after wrapper death"
        );
    }

    #[cfg(unix)]
    fn lose_terminal(&mut self) {
        // Close during an incomplete original parser sequence, without native exit input.
        self.writer.write_all(b"\x1b[").unwrap();
        self.reader_stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.reader.take().unwrap().join().unwrap();
        drop(std::mem::replace(
            &mut self.writer,
            Box::new(std::io::sink()),
        ));
        self.master.take();
        self.eof = true;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.signal().is_none(),
                    "terminal loss killed the wrapper before presentation retirement: {status:?}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "closed PTY did not retire its original presentation"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    fn terminal_flags(&self) -> libc::tcflag_t {
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        let descriptor = self.master.as_ref().unwrap().as_raw_fd().unwrap();
        assert_eq!(unsafe { libc::tcgetattr(descriptor, &mut mode) }, 0);
        mode.c_lflag
    }

    #[cfg(unix)]
    fn suspend_and_resume(&mut self, worker: u32) {
        use std::os::unix::fs::OpenOptionsExt;
        let wrapper = self.child.process_id().unwrap();
        let shell_flags = libc::ECHO | libc::ICANON;
        assert_eq!(
            self.terminal_flags() & shell_flags,
            0,
            "native UI was not in raw mode"
        );
        assert_eq!(unsafe { libc::kill(wrapper as i32, libc::SIGTSTP) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_stopped(wrapper) || !process_stopped(worker) {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "suspending the wrapper did not stop its original worker group"
            );
        }
        assert_eq!(
            self.terminal_flags() & shell_flags,
            shell_flags,
            "suspended native UI did not restore the shell terminal mode"
        );
        let mut shell = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(self.master.as_ref().unwrap().tty_name().unwrap())
            .unwrap();
        let marker = b"shell owns suspended tty\n";
        self.writer.write_all(marker).unwrap();
        let mut shell_input = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !shell_input.ends_with(b"\n") {
            let mut bytes = [0; 128];
            match shell.read(&mut bytes) {
                Ok(0) => panic!("suspended terminal lost its shell input"),
                Ok(length) => shell_input.extend_from_slice(&bytes[..length]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read suspended shell input: {error}"),
            }
            self.pump();
            assert!(
                Instant::now() < deadline,
                "background native reader consumed shell input while suspended"
            );
        }
        assert_eq!(&shell_input[..], &marker[..]);
        drop(shell);
        assert_eq!(unsafe { libc::kill(wrapper as i32, libc::SIGCONT) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_stopped(wrapper)
            || process_stopped(worker)
            || self.terminal_flags() & shell_flags != 0
        {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "CONT did not restore the original native UI and terminal mode"
            );
        }
        let marker = b"resumed-input";
        self.writer.write_all(marker).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.parser.screen().contents().contains("resumed-input") {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "resumed native UI did not read its original PTY"
            );
        }
        self.writer.write_all(&vec![0x7f; marker.len()]).unwrap();
        while self.parser.screen().contents().contains("resumed-input") {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "resumed native composer did not clear its unsent draft"
            );
        }
    }
}

#[cfg(unix)]
fn process_stopped(process: u32) -> bool {
    let state = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &process.to_string()])
        .output()
        .unwrap();
    assert!(
        state.status.success(),
        "tracked process disappeared: {process}"
    );
    String::from_utf8_lossy(&state.stdout)
        .trim_start()
        .starts_with('T')
}

#[test]
fn unused_automatic_session_is_discarded_after_two_control_c_presses() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut terminal = NativeTerminal::start(root);
    let automatic = terminal.ready(true);
    let data = root.join("data");
    let sessions = data.join("runtime-v1/sessions");
    let automatic_directory = sessions.join(automatic.to_string());
    assert!(automatic_directory.is_dir());
    let retained: Vec<_> = RuntimeStore::session_ids(&data)
        .unwrap()
        .into_iter()
        .filter(|id| *id != automatic)
        .collect();
    terminal.writer.write_all(b"\x03\x03").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        terminal.pump();
        if let Some(status) = terminal.child.try_wait().unwrap() {
            assert!(
                status.success(),
                "unused-session launcher failed:\n{}",
                terminal.parser.screen().contents()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unused-session launcher did not exit:\n{}",
            terminal.parser.screen().contents()
        );
    }
    loop {
        terminal.pump();
        let registered = atman_daemon::host_directory::discover(&data)
            .unwrap()
            .iter()
            .any(|record| record.role == atman_proto::hosts::HostRole::Tui);
        if terminal.eof && !automatic_directory.exists() && !registered {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unused-session exit retained terminal or ownership: eof={}, directory={}, registered={}\n{}",
            terminal.eof,
            automatic_directory.exists(),
            registered,
            terminal.parser.screen().contents()
        );
    }
    let screen = terminal.parser.screen().contents();
    assert!(
        !screen.contains("atman --continue"),
        "discarded session printed a resume command:\n{screen}"
    );
    assert!(
        !screen.contains("USAGE") && !screen.contains("WORK"),
        "discarded session printed an exit summary:\n{screen}"
    );
    assert!(
        !RuntimeStore::session_ids(&data)
            .unwrap()
            .contains(&automatic)
    );
    for id in retained {
        assert!(
            sessions.join(id.to_string()).is_dir(),
            "unused-session cleanup removed another Session: {id}"
        );
        RuntimeStore::inspect_session(&data, id).unwrap();
    }
}

#[test]
fn historical_native_view_starts_rpc_only_when_continued() {
    use atman_runtime::{
        event_log::legacy::LegacyFlowKey,
        flow_store::{history::LegacyArchive, view::FlowUpdate},
        message::{Message, MessageOrigin},
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let original = uuid::Uuid::now_v7();
    let source = root.join("data/sessions").join(original.to_string());
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(root.join("config/commands")).unwrap();
    std::fs::write(root.join("config/config.toml"), "").unwrap();
    std::fs::write(
        root.join("config/commands/echo.at"),
        "flow agent(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }",
    )
    .unwrap();
    std::fs::write(
        root.join("config/routes.at"),
        "default_route { flow: echo }",
    )
    .unwrap();
    let turn = atman_rt::TurnId::now();
    let user = Message::user_text(turn.clone(), "retained historical request");
    let mut answer = Message::assistant_text(turn.clone(), "retained historical answer");
    answer.origin = MessageOrigin::FinalAnswer;
    let raw = [
        serde_json::json!({"seq":1,"type":"user_msg","turn_id":turn,"message":user}),
        serde_json::json!({"seq":2,"type":"assistant_msg","turn_id":turn,"message":answer}),
    ]
    .map(|event| format!("{event}\n"))
    .concat();
    std::fs::write(source.join("events.jsonl"), &raw).unwrap();
    let mut terminal =
        NativeTerminal::start_args(root, &["--continue".into(), original.to_string()]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        terminal.pump();
        let screen = terminal.parser.screen().contents();
        if screen.contains("retained historical answer") {
            break;
        }
        assert!(
            terminal.child.try_wait().unwrap().is_none(),
            "history UI exited:\n{screen}"
        );
        assert!(
            Instant::now() < deadline,
            "history UI did not become ready:\n{screen}"
        );
    }
    // Opening immutable history must not create a Profile writer or an RPC credential.
    assert!(!root.join("data/runtime-v1").exists());
    assert!(!root.join("config/daemon.toml").exists());
    assert!(!root.join("data/run/peer/credential").exists());
    let continued = LegacyArchive::continuation_id(original, &LegacyFlowKey::Default).unwrap();
    terminal.writer.write_all(b"live continuation\r").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        terminal.pump();
        if let Ok(session) = RuntimeStore::inspect_session(&root.join("data"), continued)
            && let Some(flow) = &session.projection.default_flow
        {
            let mut completed = false;
            session
                .inspect_flow_updates(flow, |_, batch| {
                    completed |= batch
                        .iter()
                        .any(|update| matches!(update, FlowUpdate::TurnCompleted(_)));
                    Ok(())
                })
                .unwrap();
            if completed && runtime.block_on(check_native_rpc(root, continued)).unwrap() {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "continuation did not install its native RPC owner:\n{}",
            terminal.parser.screen().contents()
        );
    }
    assert_eq!(
        std::fs::read_to_string(source.join("events.jsonl")).unwrap(),
        raw
    );
    terminal.retire(continued);
}

impl Drop for NativeTerminal {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process) = self.child.process_id() {
            unsafe {
                libc::kill(process as i32, libc::SIGCONT);
            }
        }
        if let Ok(records) = atman_daemon::host_directory::discover(&self.root.join("data")) {
            for record in records {
                if record.role == atman_proto::hosts::HostRole::Tui {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(record.process_id as i32), libc::SIGCONT);
                    }
                    terminate_process(record.process_id);
                }
            }
        }
        let _ = self.child.kill();
        #[cfg(unix)]
        self.reader_stop
            .store(true, std::sync::atomic::Ordering::Release);
        if cfg!(unix) || self.eof {
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
        self.master.take();
    }
}

struct NativeDaemon {
    child: std::process::Child,
}

impl NativeDaemon {
    fn start(root: &Path) -> Self {
        let binary =
            std::path::Path::new(env!("CARGO_BIN_EXE_atman")).with_file_name(if cfg!(windows) {
                "atman-daemon.exe"
            } else {
                "atman-daemon"
            });
        assert!(
            binary.is_file(),
            "native host acceptance requires the sibling atman-daemon binary"
        );
        let log = std::fs::File::create(root.join("daemon.log")).unwrap();
        let child = std::process::Command::new(binary)
            .current_dir(root)
            .env("ATMAN_DATA_DIR", root.join("data"))
            .env("ATMAN_CONFIG_DIR", root.join("config"))
            .env("ATMAN_DAEMON_PORT", "0")
            .env("ATMAN_DISABLE_MIGRATION", "1")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Self { child }
    }

    fn ready(&mut self, root: &Path) -> atman_daemon::host_directory::HostRecord {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(record) = atman_daemon::host_directory::discover(&root.join("data"))
                .unwrap_or_default()
                .into_iter()
                .find(|record| record.role == atman_proto::hosts::HostRole::Daemon)
            {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                if runtime.block_on(describe(root, &record)).is_ok() {
                    return record;
                }
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "daemon failed to start:\n{}",
                std::fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "daemon did not become available:\n{}",
                std::fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        terminate_process(self.child.id());
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn terminate_process(process: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(process as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &process.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

async fn describe(
    root: &Path,
    record: &atman_daemon::host_directory::HostRecord,
) -> Result<atman_proto::hosts::HostDescription, String> {
    serde_json::from_value(
        native_rpc(
            root,
            record,
            atman_proto::methods::HOST_DESCRIBE,
            serde_json::json!({}),
        )
        .await?,
    )
    .map_err(|error| error.to_string())
}

async fn native_rpc(
    root: &Path,
    record: &atman_daemon::host_directory::HostRecord,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let config: atman_daemon::config::DaemonConfig = toml::from_str(
        &std::fs::read_to_string(root.join("config/daemon.toml"))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let reply: atman_proto::JsonRpcResponse = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| error.to_string())?
        .post(format!("http://{}/rpc", record.endpoint))
        .bearer_auth(config.auth_token)
        .json(&atman_proto::JsonRpcRequest::new(
            "native-acceptance",
            method,
            params,
        ))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| error.to_string())?;
    if let Some(error) = reply.error {
        return Err(error.message);
    }
    reply
        .result
        .ok_or_else(|| "missing native RPC result".to_owned())
}

enum NativeBoundary {
    Normal,
    #[cfg(unix)]
    SuspendResume,
    #[cfg(unix)]
    WrapperLost,
    #[cfg(unix)]
    TerminalLost,
}

fn held_native_turn(order: Option<bool>, pending_form: bool, late_form: bool) {
    held_native_turn_scenario(order, pending_form, late_form, NativeBoundary::Normal);
}

fn held_native_turn_scenario(
    order: Option<bool>,
    pending_form: bool,
    late_form: bool,
    boundary: NativeBoundary,
) {
    use atman_rt::{InstanceActivity, InstanceState, TurnOutcome};
    use atman_runtime::flow_store::view::FlowUpdate;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("config/commands")).unwrap();
    let config = atman_runtime::config_hub::ConfigHub::from_config_dir(root.join("config"));
    config
        .set_trust_config(&atman_runtime::trust::TrustConfig {
            mode: atman_runtime::trust::TrustMode::Reckless,
            ..Default::default()
        })
        .unwrap();
    config
        .upsert_provider(atman_runtime::config_hub::ProviderConfigUpdate {
            name: "fixture",
            kind: "openai-compat",
            api_key: Some("unused-fixture-credential"),
            api_key_env: None,
            base_url: Some("http://127.0.0.1:9/v1"),
            api: Some(atman_runtime::provider::OpenAiApi::ChatCompletions),
            max_tokens: None,
            max_tools: None,
            reasoning_format: None,
            prompt_cache_key: None,
            enabled: true,
        })
        .unwrap();
    config
        .upsert_model(atman_runtime::model_registry::ModelConfigUpdate {
            old_name: None,
            name: "fixture",
            model: "fixture-model",
            provider: Some("fixture"),
            api: Some(atman_runtime::provider::OpenAiApi::ChatCompletions),
            context_budget: 32768,
            reasoning: atman_runtime::provider::ReasoningSelection::Disabled,
            capabilities: None,
            image_detail: None,
            max_tokens: Some(1024),
            enabled: true,
        })
        .unwrap();
    config.bind_default_model("fixture").unwrap();
    for alias in ["smart", "cheap"] {
        assert_eq!(
            config
                .models()
                .model_entry(alias)
                .unwrap()
                .provider
                .as_deref(),
            Some("fixture")
        );
    }
    assert!(!config.models().is_first_run(&config.load_auth().unwrap()));
    let program = r#"flow held_native(input: string) -> string {
        contract { invocation { user_message: input } }
        count = fs.read(path: "entries.txt")
        fs.write(path: "entries.txt", content: count + "x")
        fs.write(path: "entered.txt", content: "entered")
        CONTROLLED_WAIT
        fs.write(path: "finished.txt", content: "finished")
        return final.answer(message: "original work delivered")
    }"#;
    let gate = r#"loop {
            gate = fs.read(path: "release.txt")
            when gate == "go" { break }
            sleep(ms: 50)
        }"#;
    let form = r#"answer = form.ask(kind: "text", prompt: "Reply after retirement")
        fs.write(path: "reply.txt", content: answer.text)"#;
    let wait = if pending_form {
        if late_form {
            format!("{gate}\n{form}")
        } else {
            form.to_owned()
        }
    } else {
        gate.to_owned()
    };
    let program = program.replace("CONTROLLED_WAIT", &wait);
    atman_rt::parse_file(&program).unwrap();
    std::fs::write(root.join("config/commands/held_native.at"), &program).unwrap();
    std::fs::write(
        root.join("config/routes.at"),
        "default_route { flow: held_native }",
    )
    .unwrap();
    std::fs::write(root.join("entries.txt"), "").unwrap();
    std::fs::write(root.join("release.txt"), "wait").unwrap();
    let mut daemon = order
        .filter(|first| *first)
        .map(|_| NativeDaemon::start(root));
    if let Some(daemon) = &mut daemon {
        daemon.ready(root);
    }
    let mut terminal = NativeTerminal::start(root);
    let session = terminal.ready(false);
    let flow = RuntimeStore::inspect_session(&root.join("data"), session)
        .unwrap()
        .projection
        .default_flow
        .unwrap();
    terminal.submit_visible_draft("hold native");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !root.join("entered.txt").is_file() {
        terminal.pump();
        assert!(
            Instant::now() < deadline,
            "native input never entered its controlled tool wait:\n{}",
            terminal.parser.screen().contents()
        );
    }
    while pending_form
        && !late_form
        && !terminal
            .parser
            .screen()
            .contents()
            .contains("Reply after retirement")
    {
        terminal.pump();
        assert!(
            Instant::now() < deadline,
            "native form did not reach its original UI:\n{}",
            terminal.parser.screen().contents()
        );
    }
    let before = RuntimeStore::inspect_session(&root.join("data"), session)
        .unwrap()
        .inspect_flow(&flow)
        .unwrap();
    let lease = match before.state.state() {
        InstanceState::Open(InstanceActivity::Active(active)) => active.lease.clone(),
        other => panic!("expected the original active invocation, got {other:?}"),
    };
    let original = atman_daemon::host_directory::discover(&root.join("data"))
        .unwrap()
        .into_iter()
        .find(|record| {
            record.role == atman_proto::hosts::HostRole::Tui && record.sessions.contains(&session)
        })
        .unwrap();
    let profile_identity =
        atman_runtime::flow_store::profile::ProfileStore::inspect_identity(&root.join("data"))
            .unwrap();
    assert_ne!(
        Some(original.process_id),
        terminal.child.process_id(),
        "interactive runtime was not a separate supervised worker"
    );
    if order == Some(false) {
        let mut started = NativeDaemon::start(root);
        started.ready(root);
        daemon = Some(started);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if let Some(daemon) = &mut daemon {
        let record = daemon.ready(root);
        let view = runtime.block_on(describe(root, &record)).unwrap();
        assert!(
            !view.sessions.contains(&atman_proto::SessionId(session)),
            "daemon stole active native execution"
        );
    }
    #[cfg(unix)]
    if matches!(boundary, NativeBoundary::SuspendResume) {
        terminal.suspend_and_resume(original.process_id);
    }
    let retirement_deadline = Instant::now() + Duration::from_secs(10);
    match boundary {
        NativeBoundary::Normal => terminal.retire(session),
        #[cfg(unix)]
        NativeBoundary::SuspendResume => terminal.retire(session),
        #[cfg(unix)]
        NativeBoundary::WrapperLost => terminal.lose_wrapper(),
        #[cfg(unix)]
        NativeBoundary::TerminalLost => terminal.lose_terminal(),
    }
    let competing = RuntimeStore::open(&root.join("data")).unwrap();
    let owner = loop {
        let after = RuntimeStore::inspect_session(&root.join("data"), session)
            .unwrap()
            .inspect_flow(&flow)
            .unwrap();
        assert!(
            matches!(after.state.state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == lease),
            "retiring UI interrupted or replayed its original turn"
        );
        assert!(
            competing.open_session(session).is_err(),
            "retired worker released an active writer"
        );
        let records = atman_daemon::host_directory::discover(&root.join("data")).unwrap();
        if let Some(owner) = records
            .into_iter()
            .find(|record| record.generation == original.generation)
        {
            assert_eq!(owner.process_id, original.process_id);
            if owner.presentation == atman_proto::hosts::HostPresentation::Detached {
                break owner;
            }
        }
        assert!(
            Instant::now() < retirement_deadline,
            "the original active owner did not publish presentation retirement"
        );
        terminal.pump();
    };
    let quiet = terminal.output.len();
    let delay = Instant::now() + Duration::from_millis(200);
    while Instant::now() < delay {
        terminal.pump();
    }
    assert_eq!(
        terminal.output.len(),
        quiet,
        "retired runtime wrote into the shell terminal"
    );
    if pending_form {
        if late_form {
            std::fs::write(root.join("release.txt"), "go").unwrap();
        }
        let prompt = runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                let mut after = None::<atman_proto::flows::FlowCursor>;
                loop {
                    let page: atman_proto::responses::FlowEventPage = serde_json::from_value(native_rpc(root, &owner, atman_proto::methods::FLOW_EVENTS, serde_json::json!({
                        "flow":{"session_id":session,"instance_id":flow.0.0}, "after":after, "limit":128,
                    })).await.unwrap()).unwrap();
                    for batch in page.batches {
                        for fact in batch.facts {
                            let update: FlowUpdate = serde_json::from_value(fact).unwrap();
                            if let FlowUpdate::ExecutionEvents(events) = update {
                                for event in events {
                                    if event.scope.lease() == Some(&lease)
                                        && let atman_runtime::event::Event::PendingPrompt { prompt_id, kind, payload } = event.event
                                        && kind == "form_ask"
                                        && payload["prompt"] == "Reply after retirement"
                                    {
                                        return prompt_id;
                                    }
                                }
                            }
                        }
                    }
                    if !page.has_more { break; }
                    after = page.next;
                }
                assert!(tokio::time::Instant::now() < deadline, "retired native form was not observable through its original HTTP event cursor");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
        let submission = atman_runtime::form::FormSubmission::Submitted {
            answers: vec![atman_runtime::form::FormAnswer::TextEntered {
                text: "accepted after retirement".into(),
            }],
        };
        let answer = runtime
            .block_on(native_rpc(
                root,
                &owner,
                atman_proto::methods::RESOLVE_PROMPT,
                serde_json::json!({
                    "session_id":session, "prompt_id":prompt, "answer":submission,
                }),
            ))
            .unwrap();
        assert_eq!(answer["resolved"], true);
    } else {
        std::fs::write(root.join("release.txt"), "go").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        terminal.pump();
        let inspected = RuntimeStore::inspect_session(&root.join("data"), session).unwrap();
        let mut completed = false;
        inspected.inspect_flow_updates(&flow, |_, batch| { completed |= batch.iter().any(|update| matches!(update, FlowUpdate::TurnCompleted(done) if done.lease == lease && done.outcome == TurnOutcome::Completed)); Ok(()) }).unwrap();
        if completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "original native invocation did not finish after UI retirement"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("entries.txt")).unwrap(),
        "x",
        "entry effects were replayed"
    );
    assert!(root.join("finished.txt").is_file());
    if pending_form {
        assert_eq!(
            std::fs::read_to_string(root.join("reply.txt")).unwrap(),
            "accepted after retirement"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        terminal.pump();
        let records = atman_daemon::host_directory::discover(&root.join("data")).unwrap();
        if let Some(daemon) = &mut daemon {
            let record = records
                .iter()
                .find(|record| record.process_id == daemon.child.id())
                .unwrap();
            let description = runtime.block_on(describe(root, record)).unwrap();
            if description
                .sessions
                .contains(&atman_proto::SessionId(session))
                && description.profile_owner
                && !records
                    .iter()
                    .any(|record| record.generation == original.generation)
            {
                let profile = runtime
                    .block_on(native_rpc(
                        root,
                        record,
                        atman_proto::methods::MANAGEMENT_GET,
                        serde_json::json!({}),
                    ))
                    .unwrap();
                assert_eq!(
                    profile["profile"]["profile_id"],
                    serde_json::json!(profile_identity)
                );
                assert!(
                    profile["unavailable"].is_null(),
                    "recovered daemon Profile is unavailable: {profile}"
                );
                break;
            }
        } else if !records
            .iter()
            .any(|record| record.generation == original.generation)
            && competing.open_session(session).is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native worker did not settle or return the exact completed Session"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        terminal.output.len(),
        quiet,
        "background completion escaped into the retired PTY"
    );
    let inspected = RuntimeStore::inspect_session(&root.join("data"), session).unwrap();
    let mut commits = 0;
    let mut completions = 0;
    let mut routed = None;
    inspected
        .inspect_flow_updates(&flow, |_, batch| {
            for update in batch {
                match update {
                    FlowUpdate::InputAccepted(input) => {
                        if let atman_runtime::flow_store::view::AcceptedInput::Command {
                            id,
                            name,
                            ..
                        } = input.as_ref()
                        {
                            assert_eq!(name, "held_native");
                            assert!(routed.replace(*id).is_none());
                        }
                    }
                    FlowUpdate::InputCommitted(input) if input.lease == lease => {
                        let atman_runtime::flow_store::inbox::InputContent::InvocationInput {
                            command,
                            ..
                        } = &input.content
                        else {
                            panic!("native text did not select its routed command")
                        };
                        assert!(command.is_some());
                        assert_eq!(*command, routed);
                        commits += 1;
                    }
                    FlowUpdate::TurnCompleted(done) if done.lease == lease => completions += 1,
                    _ => {}
                }
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        (commits, completions),
        (1, 1),
        "UI retirement changed the original admission or completion identity"
    );
}

#[test]
fn active_native_turn_outlives_ui_and_returns_to_an_existing_daemon() {
    held_native_turn(Some(true), false, false);
}

#[test]
fn daemon_started_after_native_ui_waits_then_installs_its_original_completed_session() {
    held_native_turn(Some(false), false, false);
}

#[test]
fn active_native_turn_outlives_ui_without_a_daemon_and_releases_after_completion() {
    held_native_turn(None, false, false);
}

#[test]
fn pending_native_form_outlives_ui_and_http_reply_completes_its_original_turn() {
    held_native_turn(None, true, false);
}

#[test]
fn form_first_requested_after_ui_retirement_keeps_its_original_http_reply_owner() {
    held_native_turn(None, true, true);
}

#[cfg(unix)]
#[test]
fn unix_suspend_resume_restores_shell_input_and_the_same_native_invocation() {
    held_native_turn_scenario(None, false, false, NativeBoundary::SuspendResume);
}

#[cfg(unix)]
#[test]
fn unix_wrapper_death_retires_terminal_without_interrupting_original_work() {
    held_native_turn_scenario(None, false, false, NativeBoundary::WrapperLost);
}

#[cfg(unix)]
#[test]
fn unix_unexpected_pty_disconnect_preserves_the_original_runtime_owner() {
    held_native_turn_scenario(None, false, false, NativeBoundary::TerminalLost);
}
