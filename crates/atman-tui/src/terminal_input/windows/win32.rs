use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use windows_sys::Win32::{
    System::Console::{
        CAPSLOCK_ON, KEY_EVENT_RECORD, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED,
        RIGHT_CTRL_PRESSED, SHIFT_PRESSED,
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetKeyboardLayout, ToUnicodeEx, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
            VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_NUMPAD0,
            VK_NUMPAD9, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
        },
        WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    },
};

// CSI Vk;Sc;Uc;Kd;Cs;Rc_ is the lossless KEY_EVENT_RECORD envelope.
// A numeric packet is bounded by the widths of its six native fields.
pub(super) const MAX_FRAME_UNITS: usize = 70;

pub(super) fn record(frame: &[u16]) -> Option<KEY_EVENT_RECORD> {
    let mut fields = [0u32; 6];
    fields[5] = 1;
    let mut field = 0;
    let mut value = None::<u32>;
    for &unit in frame.strip_prefix(&[0x1b, 0x5b])?.strip_suffix(&[0x5f])? {
        match unit {
            0x30..=0x39 => {
                value = Some(
                    value
                        .unwrap_or(0)
                        .checked_mul(10)?
                        .checked_add(u32::from(unit - 0x30))?,
                );
            }
            0x3b => {
                let default = *fields.get(field)?;
                *fields.get_mut(field)? = value.unwrap_or(default);
                field += 1;
                value = None;
            }
            _ => return None,
        }
    }
    let default = *fields.get(field)?;
    *fields.get_mut(field)? = value.unwrap_or(default);
    let mut record = unsafe { std::mem::zeroed::<KEY_EVENT_RECORD>() };
    record.wVirtualKeyCode = fields[0].try_into().ok()?;
    record.wVirtualScanCode = fields[1].try_into().ok()?;
    record.uChar.UnicodeChar = fields[2].try_into().ok()?;
    record.bKeyDown = match fields[3] {
        0 => 0,
        1 => 1,
        _ => return None,
    };
    record.dwControlKeyState = fields[4];
    record.wRepeatCount = fields[5].try_into().ok()?;
    Some(record)
}

fn modifiers(state: u32) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    for (native, typed) in [
        (SHIFT_PRESSED, KeyModifiers::SHIFT),
        (
            LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED,
            KeyModifiers::CONTROL,
        ),
        (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED, KeyModifiers::ALT),
    ] {
        if state & native != 0 {
            modifiers |= typed;
        }
    }
    modifiers
}

fn layout_char(record: &KEY_EVENT_RECORD) -> Option<char> {
    let layout = unsafe {
        GetKeyboardLayout(GetWindowThreadProcessId(
            GetForegroundWindow(),
            std::ptr::null_mut(),
        ))
    };
    let mut text = [0u16; 16];
    let length = unsafe {
        ToUnicodeEx(
            u32::from(record.wVirtualKeyCode),
            u32::from(record.wVirtualScanCode),
            [0u8; 256].as_ptr(),
            text.as_mut_ptr(),
            text.len() as i32,
            0x4, // Do not change the kernel keyboard/dead-key state.
            layout,
        )
    };
    let length = usize::try_from(length).ok()?;
    let mut chars = char::decode_utf16(text.get(..length)?.iter().copied());
    let ch = chars.next()?.ok()?;
    if chars.next().is_some() {
        return None;
    }
    let upper = (record.dwControlKeyState & SHIFT_PRESSED != 0)
        ^ (record.dwControlKeyState & CAPSLOCK_ON != 0);
    fn single(mut converted: impl Iterator<Item = char>, original: char) -> char {
        let candidate = converted.next().unwrap_or(original);
        if converted.next().is_none() {
            candidate
        } else {
            original
        }
    }
    Some(if upper {
        single(ch.to_uppercase(), ch)
    } else {
        single(ch.to_lowercase(), ch)
    })
}

#[derive(Default)]
pub(super) struct Keyboard {
    surrogate: Option<u16>,
}

impl Keyboard {
    pub(super) fn decode(&mut self, record: &KEY_EVENT_RECORD) -> Option<KeyEvent> {
        let modifiers = modifiers(record.dwControlKeyState);
        let key = record.wVirtualKeyCode;
        let unicode = unsafe { record.uChar.UnicodeChar };
        let alt_code = key == VK_MENU && record.bKeyDown == 0 && unicode != 0;
        if (VK_NUMPAD0..=VK_NUMPAD9).contains(&key)
            && modifiers.contains(KeyModifiers::ALT)
            && !modifiers.contains(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
        {
            return None;
        }
        let code = if alt_code {
            None
        } else {
            match key {
                VK_SHIFT | VK_CONTROL | VK_MENU => return None,
                VK_BACK => Some(KeyCode::Backspace),
                VK_ESCAPE => Some(KeyCode::Esc),
                VK_RETURN => Some(KeyCode::Enter),
                VK_F1..=VK_F24 => Some(KeyCode::F((key - VK_F1 + 1) as u8)),
                VK_LEFT => Some(KeyCode::Left),
                VK_UP => Some(KeyCode::Up),
                VK_RIGHT => Some(KeyCode::Right),
                VK_DOWN => Some(KeyCode::Down),
                VK_PRIOR => Some(KeyCode::PageUp),
                VK_NEXT => Some(KeyCode::PageDown),
                VK_HOME => Some(KeyCode::Home),
                VK_END => Some(KeyCode::End),
                VK_DELETE => Some(KeyCode::Delete),
                VK_INSERT => Some(KeyCode::Insert),
                VK_TAB if modifiers.contains(KeyModifiers::SHIFT) => Some(KeyCode::BackTab),
                VK_TAB => Some(KeyCode::Tab),
                _ => None,
            }
        };
        let code = if let Some(code) = code {
            self.surrogate = None;
            code
        } else if (0xd800..=0xdfff).contains(&unicode) {
            let Some(high) = self.surrogate.take() else {
                self.surrogate = Some(unicode);
                return None;
            };
            let ch = char::decode_utf16([high, unicode]).next()?.ok()?;
            return Some(KeyEvent::new(KeyCode::Char(ch), modifiers));
        } else {
            let ch = if unicode <= 0x1f && !alt_code {
                layout_char(record)?
            } else {
                char::from_u32(u32::from(unicode))?
            };
            self.surrogate = None;
            KeyCode::Char(ch)
        };
        Some(KeyEvent::new_with_kind(
            code,
            modifiers,
            if record.bKeyDown != 0 {
                KeyEventKind::Press
            } else {
                KeyEventKind::Release
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_fields_retain_native_identity_and_reject_invalid_records() {
        let frame: Vec<_> = "\x1b[13;28;13;1;16;3_".encode_utf16().collect();
        let parsed = record(&frame).unwrap();
        assert_eq!(parsed.wVirtualKeyCode, VK_RETURN);
        assert_eq!(parsed.wVirtualScanCode, 28);
        assert_eq!(unsafe { parsed.uChar.UnicodeChar }, 13);
        assert_eq!(parsed.bKeyDown, 1);
        assert_eq!(parsed.dwControlKeyState, SHIFT_PRESSED);
        assert_eq!(parsed.wRepeatCount, 3);
        let omitted: Vec<_> = "\x1b[;28;;;;_".encode_utf16().collect();
        let omitted = record(&omitted).unwrap();
        assert_eq!(omitted.wVirtualKeyCode, 0);
        assert_eq!(omitted.wVirtualScanCode, 28);
        assert_eq!(omitted.wRepeatCount, 1);
        for malformed in [
            "\x1b[65536;0;0;1;0;1_",
            "\x1b[0;65536;0;1;0;1_",
            "\x1b[0;0;65536;1;0;1_",
            "\x1b[0;0;0;2;0;1_",
            "\x1b[0;0;0;1;4294967296;1_",
            "\x1b[0;0;0;1;0;65536_",
            "\x1b[0;0;0;1;0;1;0_",
            "\x1b[0:0;0;0;1;0;1_",
            "\x1b[0;0;0;1;0;1R",
        ] {
            assert!(
                record(&malformed.encode_utf16().collect::<Vec<_>>()).is_none(),
                "accepted {malformed:?}"
            );
        }
    }
}
