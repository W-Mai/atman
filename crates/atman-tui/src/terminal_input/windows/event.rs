use crossterm::event as ct;
use termina::event as vt;

macro_rules! convert_enum {
    ($name:ident, $source:ty, $target:ty, $($variant:ident),+ $(,)?) => {
        fn $name(value: $source) -> $target {
            match value { $(<$source>::$variant => <$target>::$variant),+ }
        }
    };
}

convert_enum!(
    kind,
    vt::KeyEventKind,
    ct::KeyEventKind,
    Press,
    Release,
    Repeat
);
convert_enum!(
    button,
    vt::MouseButton,
    ct::MouseButton,
    Left,
    Right,
    Middle
);
convert_enum!(
    media,
    vt::MediaKeyCode,
    ct::MediaKeyCode,
    Play,
    Pause,
    PlayPause,
    Reverse,
    Stop,
    FastForward,
    Rewind,
    TrackNext,
    TrackPrevious,
    Record,
    LowerVolume,
    RaiseVolume,
    MuteVolume
);
convert_enum!(
    modifier,
    vt::ModifierKeyCode,
    ct::ModifierKeyCode,
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift
);

macro_rules! convert_code {
    ($($variant:ident),+ $(,)?) => {
        fn code(value: vt::KeyCode) -> ct::KeyCode {
            match value {
                $(vt::KeyCode::$variant => ct::KeyCode::$variant,)+
                vt::KeyCode::Escape => ct::KeyCode::Esc,
                vt::KeyCode::Function(number) => ct::KeyCode::F(number),
                vt::KeyCode::Char(ch) => ct::KeyCode::Char(ch),
                vt::KeyCode::Media(key) => ct::KeyCode::Media(media(key)),
                vt::KeyCode::Modifier(key) => ct::KeyCode::Modifier(modifier(key)),
            }
        }
    };
}

convert_code!(
    Enter,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    BackTab,
    PageUp,
    PageDown,
    Insert,
    Delete,
    KeypadBegin,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    Null,
);

fn modifiers(value: vt::Modifiers) -> ct::KeyModifiers {
    ct::KeyModifiers::from_bits_truncate(value.bits())
}

fn key(value: vt::KeyEvent) -> ct::KeyEvent {
    let mut state = ct::KeyEventState::NONE;
    for (source, target) in [
        (vt::KeyEventState::KEYPAD, ct::KeyEventState::KEYPAD),
        (vt::KeyEventState::CAPS_LOCK, ct::KeyEventState::CAPS_LOCK),
        (vt::KeyEventState::NUM_LOCK, ct::KeyEventState::NUM_LOCK),
    ] {
        if value.state.contains(source) {
            state |= target;
        }
    }
    ct::KeyEvent::new_with_kind_and_state(
        code(value.code),
        modifiers(value.modifiers),
        kind(value.kind),
        state,
    )
}

fn mouse_kind(value: vt::MouseEventKind) -> ct::MouseEventKind {
    match value {
        vt::MouseEventKind::Down(value) => ct::MouseEventKind::Down(button(value)),
        vt::MouseEventKind::Up(value) => ct::MouseEventKind::Up(button(value)),
        vt::MouseEventKind::Drag(value) => ct::MouseEventKind::Drag(button(value)),
        vt::MouseEventKind::Moved => ct::MouseEventKind::Moved,
        vt::MouseEventKind::ScrollUp => ct::MouseEventKind::ScrollUp,
        vt::MouseEventKind::ScrollDown => ct::MouseEventKind::ScrollDown,
        vt::MouseEventKind::ScrollLeft => ct::MouseEventKind::ScrollLeft,
        vt::MouseEventKind::ScrollRight => ct::MouseEventKind::ScrollRight,
    }
}

pub(super) fn convert(event: termina::Event) -> Option<ct::Event> {
    Some(match event {
        termina::Event::Paste(text) => ct::Event::Paste(text),
        termina::Event::Key(value) => ct::Event::Key(key(value)),
        termina::Event::Mouse(value) => ct::Event::Mouse(ct::MouseEvent {
            kind: mouse_kind(value.kind),
            column: value.column,
            row: value.row,
            modifiers: modifiers(value.modifiers),
        }),
        termina::Event::FocusIn => ct::Event::FocusGained,
        termina::Event::FocusOut => ct::Event::FocusLost,
        termina::Event::WindowResized(size) => ct::Event::Resize(size.cols, size.rows),
        termina::Event::Csi(_) | termina::Event::Osc(_) | termina::Event::Dcs(_) => {
            return None;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_replies_do_not_enter_the_keyboard_domain() {
        let mut parser = termina::Parser::default();
        for bytes in [
            b"\x1b[1;1R".as_slice(),
            b"\x1b]11;rgb:2828/2828/2828\x1b\\",
            b"\x1bP1$r0m\x1b\\",
            b"\r\x1b[I\x1b[O\x1b[200~kept\x1b[201~",
        ] {
            parser.parse(bytes, true);
        }
        for _ in 0..3 {
            let reply = parser.pop().unwrap();
            assert!(reply.is_escape());
            assert_eq!(convert(reply), None);
        }
        let mut input = Vec::new();
        while let Some(event) = parser.pop() {
            if let Some(event) = convert(event) {
                input.push(event);
            }
        }
        assert_eq!(
            input,
            vec![
                ct::Event::Key(ct::KeyEvent::new(
                    ct::KeyCode::Enter,
                    ct::KeyModifiers::NONE
                )),
                ct::Event::FocusGained,
                ct::Event::FocusLost,
                ct::Event::Paste("kept".into()),
            ]
        );
    }

    #[test]
    fn framed_text_and_typed_controls_retain_distinct_event_semantics() {
        let mut parser = termina::Parser::default();
        for bytes in [b"\x1b[20".as_slice(), b"0~first\r\nsecond\n", b"\x1b[201~"] {
            parser.parse(bytes, true);
        }
        assert_eq!(
            convert(parser.pop().unwrap()).unwrap(),
            ct::Event::Paste("first\r\nsecond\n".into())
        );
        assert!(parser.pop().is_none());
        for bytes in [
            b"\r".as_slice(),
            b"\x1b[13;2u",
            b"\x1b[1;5A",
            b"\x1b[97;1:3u",
        ] {
            parser.parse(bytes, true);
        }
        let expected = [
            ct::KeyEvent::new(ct::KeyCode::Enter, ct::KeyModifiers::NONE),
            ct::KeyEvent::new(ct::KeyCode::Enter, ct::KeyModifiers::SHIFT),
            ct::KeyEvent::new(ct::KeyCode::Up, ct::KeyModifiers::CONTROL),
            ct::KeyEvent::new_with_kind(
                ct::KeyCode::Char('a'),
                ct::KeyModifiers::NONE,
                ct::KeyEventKind::Release,
            ),
        ];
        for key in expected {
            assert_eq!(convert(parser.pop().unwrap()).unwrap(), ct::Event::Key(key));
        }
        assert!(parser.pop().is_none());
    }
}
