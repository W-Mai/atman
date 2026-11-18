use std::{
    collections::VecDeque,
    fs::File,
    io,
    os::windows::io::AsRawHandle,
    time::{Duration, Instant},
};

use crossterm::event::Event;
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{
            CONSOLE_MODE, ENABLE_MOUSE_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode,
            INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, PeekConsoleInputW, ReadConsoleInputW,
            SetConsoleMode,
        },
        Threading::WaitForSingleObject,
    },
};

#[path = "windows/event.rs"]
mod event;

fn source() -> io::Result<File> {
    File::options().read(true).write(true).open("CONIN$")
}

fn mode(file: &File) -> io::Result<CONSOLE_MODE> {
    let mut mode = 0;
    if unsafe { GetConsoleMode(file.as_raw_handle(), &mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mode)
}

fn set_mode(file: &File, mode: CONSOLE_MODE) -> io::Result<()> {
    if unsafe { SetConsoleMode(file.as_raw_handle(), mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) struct ReadMode {
    file: File,
    original_vt: CONSOLE_MODE,
}

impl ReadMode {
    pub fn new() -> io::Result<Self> {
        let file = source()?;
        let original_vt = mode(&file)? & ENABLE_VIRTUAL_TERMINAL_INPUT;
        Ok(Self { file, original_vt })
    }

    pub fn activate(&self) -> io::Result<()> {
        set_mode(
            &self.file,
            mode(&self.file)? | ENABLE_VIRTUAL_TERMINAL_INPUT,
        )
    }
}

impl Drop for ReadMode {
    fn drop(&mut self) {
        if let Ok(current) = mode(&self.file) {
            let restored = (current & !ENABLE_VIRTUAL_TERMINAL_INPUT) | self.original_vt;
            let _ = set_mode(&self.file, restored);
        }
    }
}

pub(crate) fn set_mouse_capture(enabled: bool) -> io::Result<()> {
    let file = source()?;
    let current = mode(&file)?;
    let changed = if enabled {
        current | ENABLE_MOUSE_INPUT
    } else {
        current & !ENABLE_MOUSE_INPUT
    };
    // A single mode update preserves VT input while toggling mouse capture.
    set_mode(&file, changed)
}

pub(crate) struct Input {
    file: File,
    parser: termina::Parser,
    high_surrogate: Option<u16>,
    frame_tail: VecDeque<u16>,
    paste_units: Option<Vec<u16>>,
    in_paste: bool,
    deferred: VecDeque<Event>,
}

fn key(record: &INPUT_RECORD) -> Option<KEY_EVENT_RECORD> {
    if u32::from(record.EventType) == KEY_EVENT {
        Some(unsafe { record.Event.KeyEvent })
    } else {
        None
    }
}

impl Input {
    pub fn new() -> io::Result<Self> {
        let file = source()?;
        if mode(&file)? & ENABLE_VIRTUAL_TERMINAL_INPUT == 0 {
            return Err(io::Error::other("terminal VT input mode is not active"));
        }
        Ok(Self {
            file,
            parser: termina::Parser::default(),
            high_surrogate: None,
            frame_tail: VecDeque::with_capacity(6),
            paste_units: None,
            in_paste: false,
            deferred: VecDeque::new(),
        })
    }

    fn peek(&self) -> io::Result<Option<INPUT_RECORD>> {
        let mut record = unsafe { std::mem::zeroed() };
        let mut read = 0;
        if unsafe { PeekConsoleInputW(self.file.as_raw_handle(), &mut record, 1, &mut read) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((read != 0).then_some(record))
    }

    fn take(&self) -> io::Result<KEY_EVENT_RECORD> {
        let mut record = unsafe { std::mem::zeroed() };
        let mut read = 0;
        if unsafe { ReadConsoleInputW(self.file.as_raw_handle(), &mut record, 1, &mut read) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if read != 1 {
            return Err(io::Error::other("console input changed during capture"));
        }
        key(&record).ok_or_else(|| io::Error::other("console key changed during capture"))
    }

    fn parse_char(&mut self, ch: char) {
        let mut bytes = [0; 4];
        self.parser
            .parse(ch.encode_utf8(&mut bytes).as_bytes(), true);
    }

    fn parse_unit(&mut self, unit: u16) {
        const START: &[u16] = &[0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e];
        const END: &[u16] = &[0x1b, 0x5b, 0x32, 0x30, 0x31, 0x7e];
        if let Some(paste) = self.paste_units.as_mut() {
            paste.push(unit);
            if paste.ends_with(END) {
                // The parser already holds START. Feed the framed payload once so a
                // large paste is not reparsed from its beginning for every code unit.
                let text = String::from_utf16_lossy(paste);
                self.parser.parse(text.as_bytes(), true);
                self.paste_units = None;
                self.frame_tail.clear();
            }
            return;
        }
        self.frame_tail.push_back(unit);
        if self.frame_tail.len() > START.len() {
            self.frame_tail.pop_front();
        }
        if self.frame_tail.iter().copied().eq(START.iter().copied()) {
            self.in_paste = true;
            self.paste_units = Some(Vec::new());
        }
        if let Some(high) = self.high_surrogate.take() {
            if let Some(Ok(ch)) = char::decode_utf16([high, unit]).next() {
                self.parse_char(ch);
                return;
            }
            self.parse_char(char::REPLACEMENT_CHARACTER);
        }
        if (0xd800..=0xdbff).contains(&unit) {
            self.high_surrogate = Some(unit);
        } else {
            self.parse_char(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }

    fn parsed(&mut self) -> Option<Event> {
        while let Some(parsed) = self.parser.pop() {
            if matches!(parsed, termina::Event::Paste(_)) {
                self.in_paste = false;
            }
            if let Some(event) = event::convert(parsed) {
                return Some(event);
            }
        }
        if !self.in_paste {
            return self.deferred.pop_front();
        }
        None
    }

    pub fn read_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = self.parsed() {
                return Ok(Some(event));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match unsafe {
                WaitForSingleObject(self.file.as_raw_handle(), remaining.as_millis() as u32)
            } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => {
                    self.parser.parse(&[], false);
                    return Ok(self.parsed());
                }
                _ => return Err(io::Error::last_os_error()),
            }
            let Some(record) = self.peek()? else {
                return Ok(None);
            };
            if let Some(record) = key(&record)
                && (record.wVirtualKeyCode == 0 || self.in_paste)
            {
                let record = self.take()?;
                let unit = unsafe { record.uChar.UnicodeChar };
                // Native VT bytes are press records. Keep legacy Alt-code Unicode releases
                // if they occur inside a framed payload, without copying modifier records.
                let alt_code = record.wVirtualKeyCode == 0x12 && record.bKeyDown == 0 && unit != 0;
                if (record.bKeyDown != 0 || alt_code)
                    && !(unit == 0 && matches!(record.wVirtualKeyCode, 0x10..=0x12))
                {
                    for _ in 0..record.wRepeatCount {
                        self.parse_unit(unit);
                    }
                }
            } else if let Some(event) = super::read_crossterm(Duration::ZERO)? {
                if self.in_paste {
                    self.deferred.push_back(event);
                } else {
                    return Ok(Some(event));
                }
            }
            if Instant::now() >= deadline {
                return Ok(self.parsed());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> Input {
        Input {
            file: File::open(std::env::current_exe().unwrap()).unwrap(),
            parser: termina::Parser::default(),
            high_surrogate: None,
            frame_tail: VecDeque::with_capacity(6),
            paste_units: None,
            in_paste: false,
            deferred: VecDeque::new(),
        }
    }

    #[test]
    fn source_projection_skips_replies_and_preserves_scalar_boundaries() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

        let mut input = prepared();
        input.parser.parse(b"\x1b[1;1R\r", true);
        assert_eq!(
            input.parsed(),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))
        );
        assert_eq!(input.parsed(), None);

        for units in [
            &[0xd83d, 0xde42][..],
            &[0xd800, 0x61],
            &[0xd800, 0xd83d, 0xde42],
            &[0xdc00],
            &[0xd800],
        ] {
            let mut input = prepared();
            for &unit in units {
                input.parse_unit(unit);
            }
            // The following scalar establishes whether a trailing high surrogate was malformed.
            input.parse_unit(u16::from(b'!'));
            let mut text = String::new();
            while let Some(event) = input.parsed() {
                match event {
                    Event::Key(KeyEvent {
                        code: KeyCode::Char(ch),
                        kind: KeyEventKind::Press,
                        ..
                    }) => text.push(ch),
                    other => panic!("unexpected scalar event: {other:?}"),
                }
            }
            let mut expected = units.to_vec();
            expected.push(u16::from(b'!'));
            assert_eq!(text, String::from_utf16_lossy(&expected));
        }
    }
}
