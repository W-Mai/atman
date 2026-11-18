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
    CAPSLOCK_ON, DOUBLE_CLICK, ENABLE_MOUSE_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, FOCUS_EVENT,
    FROM_LEFT_1ST_BUTTON_PRESSED, FROM_LEFT_2ND_BUTTON_PRESSED, FROM_LEFT_3RD_BUTTON_PRESSED,
    FROM_LEFT_4TH_BUTTON_PRESSED, GetConsoleMode, GetNumberOfConsoleInputEvents, INPUT_RECORD,
    KEY_EVENT, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, MOUSE_EVENT, MOUSE_HWHEELED, MOUSE_MOVED,
    MOUSE_WHEELED, NUMLOCK_ON, PeekConsoleInputW, RIGHTMOST_BUTTON_PRESSED, SCROLLLOCK_ON,
    SHIFT_PRESSED, SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT, WriteConsoleInputW,
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

fn check_fragmented_reader_frame(input: &mut windows_input::Input) {
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let mut queued = 0;
    let mut drain = |input: &mut windows_input::Input| {
        let mut events = Vec::new();
        loop {
            assert_ne!(
                unsafe { GetNumberOfConsoleInputEvents(file.as_raw_handle(), &mut queued) },
                0
            );
            let event = input.read_event(Duration::ZERO).unwrap();
            if let Some(event) = event {
                events.push(event);
            } else if queued == 0 {
                break;
            }
        }
        events
    };
    drain(input);
    // ConPTY parses its upstream byte stream before CONIN$. Inject record
    // fragments here to test the reader boundary independently of that parser.
    let fragments = [
        "\x1b[20",
        "0~first\r",
        "\nsecond 中文",
        "🙂\nthird\r\x1b[20",
        "1~",
    ];
    for (index, fragment) in fragments.into_iter().enumerate() {
        let records: Vec<_> = fragment
            .encode_utf16()
            .map(|unit| {
                let mut record = unsafe { std::mem::zeroed::<INPUT_RECORD>() };
                record.EventType = KEY_EVENT as u16;
                let key = unsafe { &mut record.Event.KeyEvent };
                key.bKeyDown = 1;
                key.wRepeatCount = 1;
                key.uChar.UnicodeChar = unit;
                record
            })
            .collect();
        let mut written = 0;
        let active = console_mode();
        // Inject already-encoded raw units without asking the console to wrap
        // each synthetic KEY_EVENT in another Win32 packet.
        console_set_mode(active & !ENABLE_VIRTUAL_TERMINAL_INPUT);
        assert_ne!(
            unsafe {
                WriteConsoleInputW(
                    file.as_raw_handle(),
                    records.as_ptr(),
                    records.len() as u32,
                    &mut written,
                )
            },
            0
        );
        console_set_mode(active);
        assert_eq!(written as usize, records.len());
        let events = drain(input);
        if index + 1 == fragments.len() {
            assert_eq!(events, vec![Event::Paste(PAYLOAD.into())]);
        } else {
            assert!(
                events.is_empty(),
                "incomplete reader frame leaked: {events:?}"
            );
        }
    }
    let idle = Instant::now();
    assert_eq!(input.read_event(Duration::from_millis(20)).unwrap(), None);
    assert!(idle.elapsed() < Duration::from_secs(1));
}

fn collect_console_events(
    file: &File,
    read: &mut dyn FnMut() -> io::Result<Option<Event>>,
) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut queued = 0;
        assert_ne!(
            unsafe { GetNumberOfConsoleInputEvents(file.as_raw_handle(), &mut queued) },
            0
        );
        if let Some(event) = read().unwrap() {
            events.push(event);
        } else if queued == 0 {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "native event comparison did not drain"
        );
    }
}

fn check_native_keyboard_equivalence(input: &mut windows_input::Input) -> usize {
    fn record(
        key: u16,
        scan: u16,
        unicode: u16,
        down: bool,
        state: u32,
        repeat: u16,
    ) -> INPUT_RECORD {
        let mut record = unsafe { std::mem::zeroed::<INPUT_RECORD>() };
        record.EventType = KEY_EVENT as u16;
        let event = unsafe { &mut record.Event.KeyEvent };
        event.wVirtualKeyCode = key;
        event.wVirtualScanCode = scan;
        event.uChar.UnicodeChar = unicode;
        event.bKeyDown = i32::from(down);
        event.dwControlKeyState = state;
        event.wRepeatCount = repeat;
        record
    }
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let collect =
        |read: &mut dyn FnMut() -> io::Result<Option<Event>>| collect_console_events(&file, read);
    collect(&mut || input.read_event(Duration::ZERO));
    let active = console_mode();
    let mut cases = Vec::new();
    for key in [
        0x08, 0x09, 0x0d, 0x1b, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2d, 0x2e, 0x70,
        0x87,
    ] {
        for state in [
            0,
            SHIFT_PRESSED,
            LEFT_CTRL_PRESSED,
            LEFT_ALT_PRESSED,
            SHIFT_PRESSED | LEFT_CTRL_PRESSED,
        ] {
            cases.push(vec![
                record(key, 0, if key == 0x0d { 13 } else { 0 }, true, state, 1),
                record(key, 0, if key == 0x0d { 13 } else { 0 }, false, state, 1),
            ]);
        }
    }
    for state in [
        LEFT_CTRL_PRESSED,
        LEFT_CTRL_PRESSED | CAPSLOCK_ON,
        LEFT_CTRL_PRESSED | SHIFT_PRESSED,
        LEFT_CTRL_PRESSED | NUMLOCK_ON | SCROLLLOCK_ON,
    ] {
        cases.push(vec![
            record(0x43, 0x2e, 3, true, state, 1),
            record(0x43, 0x2e, 3, false, state, 1),
        ]);
    }
    for state in [
        0,
        CAPSLOCK_ON,
        NUMLOCK_ON | SCROLLLOCK_ON,
        CAPSLOCK_ON | SHIFT_PRESSED,
    ] {
        let unicode = if state & CAPSLOCK_ON != 0 && state & SHIFT_PRESSED == 0 {
            b'A'
        } else {
            b'a'
        };
        cases.push(vec![
            record(0x41, 0x1e, u16::from(unicode), true, state, 1),
            record(0x41, 0x1e, u16::from(unicode), false, state, 1),
        ]);
    }
    cases.push(vec![record(0x58, 0x2d, u16::from(b'x'), false, 0, 1)]);
    cases.push(vec![record(0, 0, '外' as u16, true, 0, 1)]);
    cases.push(vec![record(0, 0, u16::from(b'A'), true, 0, 1)]);
    cases.push(vec![
        record(0, 0, 0xd83d, true, 0, 1),
        record(0, 0, 0xde42, true, 0, 1),
    ]);
    cases.push(vec![
        record(0x12, 0x38, 0xd83d, false, 0, 1),
        record(0x12, 0x38, 0xde42, false, 0, 1),
    ]);
    cases.push(vec![record(0x41, 0x1e, u16::from(b'a'), true, 0, 3)]);
    for (index, records) in cases.iter().enumerate() {
        let raw_source = records
            .iter()
            .all(|record| unsafe { record.Event.KeyEvent.wVirtualKeyCode } == 0);
        let expanded: Vec<_> = records
            .iter()
            .flat_map(|record| {
                let mut record = *record;
                let repeat = unsafe { record.Event.KeyEvent.wRepeatCount };
                record.Event.KeyEvent.wRepeatCount = 1;
                std::iter::repeat_n(record, repeat as usize)
            })
            .collect();
        let write = |records: &[INPUT_RECORD]| {
            let mut written = 0;
            assert_ne!(
                unsafe {
                    WriteConsoleInputW(
                        file.as_raw_handle(),
                        records.as_ptr(),
                        records.len() as u32,
                        &mut written,
                    )
                },
                0
            );
            assert_eq!(written as usize, records.len());
        };
        console_set_mode(active & !ENABLE_VIRTUAL_TERMINAL_INPUT);
        write(&expanded);
        // Native records use crossterm as their authority. VK=0 was already
        // owned by the original raw reader, including case and UTF-16 parsing.
        let native = if raw_source {
            console_set_mode(active);
            let before = input.decoded_win32_packets();
            let raw = collect(&mut || input.read_event(Duration::ZERO));
            assert_eq!(
                input.decoded_win32_packets(),
                before,
                "raw comparison {index} baseline was wrapped by the console"
            );
            raw
        } else {
            collect(&mut || read_crossterm(Duration::ZERO))
        };
        console_set_mode(active);
        let before = input.decoded_win32_packets();
        write(records);
        let framed = collect(&mut || input.read_event(Duration::ZERO));
        assert_eq!(
            input.decoded_win32_packets() - before,
            records.len() as u64,
            "native comparison {index} bypassed the Win32 packet decoder"
        );
        assert!(
            !native.is_empty(),
            "native comparison {index} did not produce an event"
        );
        assert_eq!(
            framed, native,
            "native comparison {index} changed typed keyboard semantics"
        );
    }
    console_set_mode(active);
    cases.len()
}

fn check_native_pointer_equivalence(input: &mut windows_input::Input) -> usize {
    fn mouse(buttons: u32, flags: u32) -> INPUT_RECORD {
        let mut record = unsafe { std::mem::zeroed::<INPUT_RECORD>() };
        record.EventType = MOUSE_EVENT as u16;
        let event = unsafe { &mut record.Event.MouseEvent };
        event.dwMousePosition.X = 3;
        event.dwMousePosition.Y = 4;
        event.dwButtonState = buttons;
        event.dwEventFlags = flags;
        event.dwControlKeyState = SHIFT_PRESSED | LEFT_CTRL_PRESSED | LEFT_ALT_PRESSED;
        record
    }
    let file = File::options()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    collect_console_events(&file, &mut || input.read_event(Duration::ZERO));
    let active = console_mode();
    let mut cases = Vec::new();
    for button in [
        FROM_LEFT_1ST_BUTTON_PRESSED,
        RIGHTMOST_BUTTON_PRESSED,
        FROM_LEFT_2ND_BUTTON_PRESSED,
        FROM_LEFT_3RD_BUTTON_PRESSED,
        FROM_LEFT_4TH_BUTTON_PRESSED,
    ] {
        cases.push(vec![
            mouse(button, 0),
            mouse(button, DOUBLE_CLICK),
            mouse(0, 0),
        ]);
        cases.push(vec![
            mouse(button, 0),
            mouse(button, MOUSE_MOVED),
            mouse(0, 0),
        ]);
    }
    cases.push(vec![mouse(0, MOUSE_MOVED)]);
    cases.push(vec![
        mouse(
            FROM_LEFT_1ST_BUTTON_PRESSED | RIGHTMOST_BUTTON_PRESSED,
            MOUSE_MOVED,
        ),
        mouse(0, 0),
    ]);
    for flags in [MOUSE_WHEELED, MOUSE_HWHEELED] {
        for delta in [120i32, -120] {
            cases.push(vec![mouse((delta << 16) as u32, flags)]);
        }
    }
    let mut focused = unsafe { std::mem::zeroed::<INPUT_RECORD>() };
    focused.EventType = FOCUS_EVENT as u16;
    focused.Event.FocusEvent.bSetFocus = 1;
    let mut blurred = focused;
    blurred.Event.FocusEvent.bSetFocus = 0;
    cases.push(vec![focused, blurred]);
    let mut resized = unsafe { std::mem::zeroed::<INPUT_RECORD>() };
    resized.EventType = WINDOW_BUFFER_SIZE_EVENT as u16;
    resized.Event.WindowBufferSizeEvent.dwSize.X = 100;
    resized.Event.WindowBufferSizeEvent.dwSize.Y = 24;
    cases.push(vec![resized]);
    for (index, records) in cases.iter().enumerate() {
        let write = || {
            let mut written = 0;
            assert_ne!(
                unsafe {
                    WriteConsoleInputW(
                        file.as_raw_handle(),
                        records.as_ptr(),
                        records.len() as u32,
                        &mut written,
                    )
                },
                0
            );
            assert_eq!(written as usize, records.len());
        };
        // Keep these as actual native records on both paths, without VT wrapping.
        console_set_mode(active & !ENABLE_VIRTUAL_TERMINAL_INPUT);
        write();
        let original = collect_console_events(&file, &mut || read_crossterm(Duration::ZERO));
        write();
        let owned = collect_console_events(&file, &mut || input.read_event(Duration::ZERO));
        assert!(
            !original.is_empty(),
            "native pointer comparison {index} had no baseline"
        );
        assert_eq!(
            owned, original,
            "native pointer comparison {index} changed semantics"
        );
    }
    console_set_mode(active);
    cases.len()
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
    // Windows Terminal writes each bracketed-paste marker atomically. Payload
    // fragmentation remains independent of the host's marker recognition.
    terminal.write(b"\x1b[200~");
    terminal.write(b"first\r");
    terminal.write("\nsecond 中文".as_bytes());
    terminal.write("🙂\nthird\r".as_bytes());
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
    assert_eq!(terminal.await_receipt(root, "fragmented-reader"), "intact");
    let compared: usize = terminal
        .await_receipt(root, "native-equivalence")
        .parse()
        .unwrap();
    assert!(compared >= 90);
    let compared: usize = terminal
        .await_receipt(root, "pointer-equivalence")
        .parse()
        .unwrap();
    assert!(compared >= 18);
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
    check_fragmented_reader_frame(&mut input);
    publish(&root, "fragmented-reader", "intact");
    let compared = check_native_keyboard_equivalence(&mut input);
    publish(&root, "native-equivalence", &compared.to_string());
    let compared = check_native_pointer_equivalence(&mut input);
    publish(&root, "pointer-equivalence", &compared.to_string());
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
