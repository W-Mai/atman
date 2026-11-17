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
            INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, PeekConsoleInputW, ReadConsoleInputW,
        },
        Threading::WaitForSingleObject,
    },
};

const PASTE_START: &[u16] = &[
    0x1b,
    b'[' as u16,
    b'2' as u16,
    b'0' as u16,
    b'0' as u16,
    b'~' as u16,
];
const PASTE_END: &[u16] = &[
    0x1b,
    b'[' as u16,
    b'2' as u16,
    b'0' as u16,
    b'1' as u16,
    b'~' as u16,
];

pub(crate) struct Input {
    file: File,
    paste: Option<Vec<u16>>,
    deferred: VecDeque<Event>,
}

#[derive(Debug, PartialEq, Eq)]
enum Prefix {
    Other,
    Incomplete,
    Complete { records: usize, trailing: usize },
}

fn key(record: &INPUT_RECORD) -> Option<KEY_EVENT_RECORD> {
    if u32::from(record.EventType) == KEY_EVENT {
        Some(unsafe { record.Event.KeyEvent })
    } else {
        None
    }
}

fn character(key: &KEY_EVENT_RECORD) -> u16 {
    unsafe { key.uChar.UnicodeChar }
}

fn paste_prefix(records: &[INPUT_RECORD]) -> Prefix {
    let mut matched = 0;
    for (index, record) in records.iter().enumerate() {
        let Some(key) = key(record) else {
            return Prefix::Other;
        };
        // ConPTY forwards an unknown CSI as raw Unicode records, not physical key presses.
        if key.wVirtualKeyCode != 0 || key.wVirtualScanCode != 0 || key.dwControlKeyState != 0 {
            return Prefix::Other;
        }
        if key.bKeyDown == 0 {
            if matched == 0 {
                return Prefix::Other;
            }
            continue;
        }
        for repeated in 0..key.wRepeatCount {
            if character(&key) != PASTE_START[matched] {
                return Prefix::Other;
            }
            matched += 1;
            if matched == PASTE_START.len() {
                return Prefix::Complete {
                    records: index + 1,
                    trailing: usize::from(key.wRepeatCount - repeated - 1),
                };
            }
        }
    }
    if matched == 0 {
        Prefix::Other
    } else {
        Prefix::Incomplete
    }
}

fn append_paste(paste: &mut Vec<u16>, key: &KEY_EVENT_RECORD) -> Option<usize> {
    if key.bKeyDown == 0 {
        return None;
    }
    let ch = character(key);
    // Modifier records synthesized around payload characters do not contribute text.
    if ch == 0 && matches!(key.wVirtualKeyCode, 0x10..=0x12) {
        return None;
    }
    for repeated in 0..key.wRepeatCount {
        paste.push(ch);
        if paste.ends_with(PASTE_END) {
            return Some(usize::from(key.wRepeatCount - repeated - 1));
        }
    }
    None
}

impl Input {
    pub fn new() -> io::Result<Self> {
        // crossterm's Windows reader opens the same console input buffer through CONIN$.
        Ok(Self {
            file: File::options().read(true).write(true).open("CONIN$")?,
            paste: None,
            deferred: VecDeque::new(),
        })
    }

    fn peek(&self, records: &mut [INPUT_RECORD]) -> io::Result<usize> {
        let mut read = 0;
        if unsafe {
            PeekConsoleInputW(
                self.file.as_raw_handle(),
                records.as_mut_ptr(),
                records.len() as u32,
                &mut read,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn take(&self, records: &mut [INPUT_RECORD]) -> io::Result<()> {
        let mut read = 0;
        if unsafe {
            ReadConsoleInputW(
                self.file.as_raw_handle(),
                records.as_mut_ptr(),
                records.len() as u32,
                &mut read,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if read as usize != records.len() {
            return Err(io::Error::other(
                "console input changed during paste capture",
            ));
        }
        Ok(())
    }

    pub fn read_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if self.paste.is_none()
            && let Some(event) = self.deferred.pop_front()
        {
            return Ok(Some(event));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match unsafe {
                WaitForSingleObject(
                    self.file.as_raw_handle(),
                    remaining.as_millis().min(u128::from(u32::MAX)) as u32,
                )
            } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(None),
                _ => return Err(io::Error::last_os_error()),
            }
            let mut records = [unsafe { std::mem::zeroed() }; 32];
            let read = self.peek(&mut records)?;
            if read == 0 {
                return Ok(None);
            }
            if self.paste.is_none() {
                match paste_prefix(&records[..read]) {
                    Prefix::Other => return super::read_crossterm(Duration::ZERO),
                    Prefix::Incomplete => {
                        // Keep an explicit partial frame in the original queue. This wait only
                        // bounds the reader's retry; it never classifies typing by elapsed time.
                        std::thread::sleep(remaining);
                        return Ok(None);
                    }
                    Prefix::Complete {
                        records: count,
                        trailing,
                    } => {
                        self.take(&mut records[..count])?;
                        if paste_prefix(&records[..count])
                            != (Prefix::Complete {
                                records: count,
                                trailing,
                            })
                        {
                            return Err(io::Error::other(
                                "console paste prefix changed before capture",
                            ));
                        }
                        self.paste = Some(vec![b'~' as u16; trailing]);
                    }
                }
            } else if key(&records[0]).is_some() {
                self.take(&mut records[..1])?;
                let record = key(&records[0]).ok_or_else(|| {
                    io::Error::other("console paste record changed before capture")
                })?;
                let paste = self.paste.as_mut().unwrap();
                if let Some(trailing) = append_paste(paste, &record) {
                    paste.truncate(paste.len() - PASTE_END.len());
                    let text = String::from_utf16_lossy(paste);
                    self.paste = None;
                    for _ in 0..trailing {
                        self.deferred
                            .push_back(Event::Key(crossterm::event::KeyEvent::new(
                                crossterm::event::KeyCode::Char('~'),
                                crossterm::event::KeyModifiers::NONE,
                            )));
                    }
                    return Ok(Some(Event::Paste(text)));
                }
            } else if let Some(event) = super::read_crossterm(Duration::ZERO)? {
                // Let crossterm retain mouse/resize/focus semantics, but do not let one
                // change the receiving view in the middle of a framed text payload.
                self.deferred.push_back(event);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(ch: u16) -> INPUT_RECORD {
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = KEY_EVENT as u16;
        record.Event.KeyEvent = KEY_EVENT_RECORD {
            bKeyDown: 1,
            wRepeatCount: 1,
            uChar: windows_sys::Win32::System::Console::KEY_EVENT_RECORD_0 { UnicodeChar: ch },
            ..unsafe { std::mem::zeroed() }
        };
        record
    }

    #[test]
    fn only_explicit_raw_framing_captures_unicode_and_line_breaks() {
        let start: Vec<_> = PASTE_START.iter().copied().map(raw).collect();
        for count in 1..start.len() {
            assert_eq!(paste_prefix(&start[..count]), Prefix::Incomplete);
        }
        assert_eq!(
            paste_prefix(&start),
            Prefix::Complete {
                records: 6,
                trailing: 0
            }
        );
        let mut physical_escape = raw(0x1b);
        physical_escape.Event.KeyEvent.wVirtualKeyCode = 0x1b;
        assert_eq!(paste_prefix(&[physical_escape]), Prefix::Other);
        assert_eq!(paste_prefix(&[raw(b'a' as u16)]), Prefix::Other);

        let text = "first\r\nsecond\n第三行🙂\r";
        let mut paste = Vec::new();
        for unit in text.encode_utf16() {
            let mut record = key(&raw(unit)).unwrap();
            if unit == u16::from(b'\r') {
                record.wVirtualKeyCode = 0x0d;
            }
            assert_eq!(append_paste(&mut paste, &record), None);
        }
        for (index, unit) in PASTE_END.iter().copied().enumerate() {
            assert_eq!(
                append_paste(&mut paste, &key(&raw(unit)).unwrap()),
                (index + 1 == PASTE_END.len()).then_some(0)
            );
        }
        paste.truncate(paste.len() - PASTE_END.len());
        assert_eq!(String::from_utf16(&paste).unwrap(), text);
    }
}
