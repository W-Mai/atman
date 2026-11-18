#![cfg(windows)]

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use atman_runtime::platform::with_process_creation;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use portable_pty::{CommandBuilder, PtySize};
use serde_json::{Value, json};
use windows_sys::Win32::System::Console::{
    ENABLE_MOUSE_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, INPUT_RECORD, KEY_EVENT,
    PeekConsoleInputW, SetConsoleMode,
};

#[path = "../src/terminal_input/windows.rs"]
mod windows_input;

const ROOT: &str = "_ATMAN_WINDOWS_INPUT_TEST_ROOT";
const PAYLOAD: &str = "first\r\nsecond 中文🙂\nthird\r";
const DEADLINE: Duration = Duration::from_secs(15);

fn read_crossterm(timeout: Duration) -> io::Result<Option<Event>> {
    if crossterm::event::poll(timeout)? {
        crossterm::event::read().map(Some)
    } else {
        Ok(None)
    }
}

fn publish(root: &Path, name: &str, value: &str) {
    let temporary = root.join(format!(".{name}.tmp"));
    fs::write(&temporary, value).unwrap();
    fs::rename(temporary, root.join(name)).unwrap();
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + DEADLINE;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing input helper receipt: {path:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn raw_records() -> Vec<Value> {
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let mut records = vec![unsafe { std::mem::zeroed::<INPUT_RECORD>() }; 1024];
    let mut count = 0;
    assert_ne!(
        unsafe {
            PeekConsoleInputW(
                file.as_raw_handle(),
                records.as_mut_ptr(),
                records.len() as u32,
                &mut count,
            )
        },
        0,
        "peek actual native input: {}",
        io::Error::last_os_error()
    );
    records[..count as usize]
        .iter()
        .map(|record| {
            if u32::from(record.EventType) == KEY_EVENT {
                let key = unsafe { record.Event.KeyEvent };
                json!({
                    "kind": "key",
                    "down": key.bKeyDown != 0,
                    "repeat": key.wRepeatCount,
                    "vkey": key.wVirtualKeyCode,
                    "scan": key.wVirtualScanCode,
                    "control": key.dwControlKeyState,
                    "unicode": unsafe { key.uChar.UnicodeChar },
                })
            } else {
                json!({"kind": "other", "type": record.EventType})
            }
        })
        .collect()
}

fn console_mode() -> u32 {
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let mut mode = 0;
    assert_ne!(
        unsafe { GetConsoleMode(file.as_raw_handle(), &mut mode) },
        0
    );
    mode
}

fn console_set_mode(mode: u32) {
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    assert_ne!(unsafe { SetConsoleMode(file.as_raw_handle(), mode) }, 0);
}

fn check_owned_mode_restoration() {
    let active = console_mode();
    for original_vt in [0, ENABLE_VIRTUAL_TERMINAL_INPUT] {
        console_set_mode((active & !ENABLE_VIRTUAL_TERMINAL_INPUT) | original_vt);
        let guard = windows_input::ReadMode::new().unwrap();
        guard.activate().unwrap();
        assert_ne!(console_mode() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        windows_input::set_mouse_capture(false).unwrap();
        let changed = console_mode();
        assert_eq!(changed & ENABLE_MOUSE_INPUT, 0);
        drop(guard);
        assert_eq!(
            console_mode(),
            (changed & !ENABLE_VIRTUAL_TERMINAL_INPUT) | original_vt
        );
    }
    console_set_mode(active);
}

struct NativeInput {
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    capture: Arc<Mutex<Vec<u8>>>,
    output: mpsc::Receiver<()>,
}

impl NativeInput {
    fn start(root: &Path) -> Self {
        let pair = with_process_creation(|| {
            portable_pty::native_pty_system().openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
        })
        .unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let responder = Arc::clone(&writer);
        let capture = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&capture);
        let (send, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            let mut pending = Vec::new();
            while let Ok(length) = reader.read(&mut buffer) {
                if length == 0 {
                    break;
                }
                observed
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buffer[..length]);
                pending.extend_from_slice(&buffer[..length]);
                const QUERY: &[u8] = b"\x1b[6n";
                let mut consumed = 0;
                while let Some(offset) = pending[consumed..]
                    .windows(QUERY.len())
                    .position(|part| part == QUERY)
                {
                    let mut writer = responder.lock().unwrap();
                    writer.write_all(b"\x1b[1;1R").unwrap();
                    writer.flush().unwrap();
                    consumed += offset + QUERY.len();
                }
                let retired = consumed.max(pending.len().saturating_sub(QUERY.len() - 1));
                pending.drain(..retired);
            }
            let _ = send.send(());
        });
        let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "windows_input_child",
            "--nocapture",
            "--test-threads=1",
        ]);
        command.env(ROOT, root);
        let child = with_process_creation(|| pair.slave.spawn_command(command)).unwrap();
        drop(pair.slave);
        Self {
            master: Some(pair.master),
            child,
            writer,
            capture,
            output,
        }
    }

    fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    fn await_receipt(&mut self, root: &Path, name: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(receipt) = fs::read_to_string(root.join(name)) {
                return receipt;
            }
            let status = self.child.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < deadline,
                "input helper missing {name}: status={status:?}, trace={:?}, output={}",
                fs::read_to_string(root.join("captured.json")),
                String::from_utf8_lossy(&self.capture.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "input helper failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "input helper failed to exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        let master = self.master.take().unwrap();
        let (send, closed) = mpsc::channel();
        std::thread::spawn(move || {
            drop(master);
            let _ = send.send(());
        });
        closed
            .recv_timeout(DEADLINE)
            .expect("native input ConPTY did not close");
        self.output
            .recv_timeout(DEADLINE)
            .expect("native input output did not reach EOF");
    }
}

impl Drop for NativeInput {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(master) = self.master.take() {
            std::thread::spawn(move || drop(master));
        }
    }
}

#[test]
fn conpty_paste_records_reach_the_original_reader_without_enter() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut terminal = NativeInput::start(root);
    terminal.await_receipt(root, "ready");
    terminal.write(b"\x1b[20");
    publish(root, "prefix-written", "yes");
    let prefix = terminal.await_receipt(root, "prefix.json");
    eprintln!("Actual ConPTY partial paste prefix: {prefix}");
    terminal.write(b"0~");
    terminal.write(PAYLOAD.as_bytes());
    terminal.write(b"\x1b[201~");
    publish(root, "frame-written", "yes");
    let receipt = terminal.await_receipt(root, "captured.json");
    eprintln!("Actual ConPTY fixed paste records and events: {receipt}");
    let trace: Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(
        trace["premature_enter"], false,
        "native paste escaped as Enter: {receipt}"
    );
    assert_eq!(
        trace["paste"], PAYLOAD,
        "native paste was not delivered intact: {receipt}"
    );
    terminal.write(b"\r");
    assert_eq!(terminal.await_receipt(root, "entered"), "once");
    terminal.write(b"\x1b[A\x1b[1;5A\x1b[13;2u\x1b[97;1:3u");
    terminal.write(b"\x1b[38;72;0;1;256;1_\x1b[38;72;0;0;256;1_");
    terminal.write(b"\x1b[38;72;0;1;264;1_\x1b[38;72;0;0;264;1_");
    terminal.write(b"\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_");
    terminal.write(b"\x1b[88;45;120;1;0;1_\x1b[88;45;120;0;0;1_");
    terminal.write(b"\x1b[<0;4;5M\x1b[<0;4;5m\x1b[I\x1b[O");
    terminal.write("外🙂".as_bytes());
    terminal
        .master
        .as_ref()
        .unwrap()
        .resize(PtySize {
            rows: 25,
            cols: 101,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let normal = terminal.await_receipt(root, "normal.json");
    eprintln!("Actual ConPTY ordinary input after VT activation: {normal}");
    let normal: Value = serde_json::from_str(&normal).unwrap();
    assert_eq!(normal["all_observed"], true);
    terminal.finish();
}

#[test]
fn windows_input_child() {
    let Some(root) = std::env::var_os(ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let original_mode = console_mode();
    let terminal = atman_tui::terminal_guard::TerminalGuard::install().unwrap();
    assert_ne!(console_mode() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
    let mut input = windows_input::Input::new().unwrap();
    publish(&root, "ready", "yes");
    wait_for(&root.join("prefix-written"));
    std::thread::sleep(Duration::from_millis(100));
    publish(
        &root,
        "prefix.json",
        &serde_json::to_string(&raw_records()).unwrap(),
    );
    wait_for(&root.join("frame-written"));
    std::thread::sleep(Duration::from_millis(100));
    let raw = raw_records();
    let mut events = Vec::new();
    let mut paste = None;
    let mut premature_enter = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(event) = input.read_event(Duration::from_millis(50)).unwrap() {
            events.push(format!("{event:?}"));
            match event {
                Event::Paste(text) => {
                    paste = Some(text);
                    break;
                }
                Event::Key(key)
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter =>
                {
                    premature_enter = true;
                    break;
                }
                _ => {}
            }
        }
    }
    let trace = json!({"mode": console_mode(), "raw": raw, "events": events, "paste": paste, "premature_enter": premature_enter});
    publish(
        &root,
        "captured.json",
        &serde_json::to_string(&trace).unwrap(),
    );
    assert!(!premature_enter, "framed native paste produced Enter");
    assert_eq!(
        trace["paste"], PAYLOAD,
        "framed native paste was not captured"
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        if matches!(input.read_event(Duration::from_millis(50)).unwrap(), Some(Event::Key(key)) if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter)
        {
            publish(&root, "entered", "once");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "explicit native Enter did not arrive"
        );
    }
    let mut observed = [false; 12];
    let mut controls = [0; 3];
    let mut normal = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while observed.iter().any(|value| !value) && Instant::now() < deadline {
        let Some(event) = input.read_event(Duration::from_millis(50)).unwrap() else {
            continue;
        };
        normal.push(format!("{event:?}"));
        match event {
            Event::Key(key) => {
                let index = match (key.code, key.modifiers, key.kind) {
                    (KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press) => Some(0),
                    (KeyCode::Up, KeyModifiers::CONTROL, KeyEventKind::Press) => Some(1),
                    (KeyCode::Enter, KeyModifiers::SHIFT, KeyEventKind::Press) => Some(2),
                    (KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Release) => Some(3),
                    (KeyCode::Char('外'), KeyModifiers::NONE, KeyEventKind::Press) => Some(9),
                    (KeyCode::Char('🙂'), KeyModifiers::NONE, KeyEventKind::Press) => Some(10),
                    (KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Release) => Some(11),
                    _ => None,
                };
                if let Some(index) = index {
                    if index < controls.len() {
                        controls[index] += 1;
                        observed[index] = controls[index] >= 2;
                    } else {
                        observed[index] = true;
                    }
                }
            }
            Event::Mouse(mouse) if (mouse.column, mouse.row) == (3, 4) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => observed[4] = true,
                MouseEventKind::Up(MouseButton::Left) => observed[5] = true,
                _ => {}
            },
            Event::FocusGained => observed[6] = true,
            Event::FocusLost => observed[7] = true,
            Event::Resize(101, 25) => observed[8] = true,
            _ => {}
        }
    }
    let all_observed = observed.iter().all(|value| *value);
    publish(
        &root,
        "normal.json",
        &json!({"all_observed": all_observed, "observed": observed, "controls": controls, "events": normal}).to_string(),
    );
    assert!(
        all_observed,
        "ordinary input lost after VT activation: {observed:?}"
    );
    check_owned_mode_restoration();
    atman_tui::terminal_guard::set_mouse_capture(false).unwrap();
    assert_ne!(console_mode() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
    atman_tui::terminal_guard::set_mouse_capture(true).unwrap();
    assert_ne!(console_mode() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
    drop(input);
    drop(terminal);
    assert_eq!(
        console_mode() & ENABLE_VIRTUAL_TERMINAL_INPUT,
        original_mode & ENABLE_VIRTUAL_TERMINAL_INPUT
    );
}
