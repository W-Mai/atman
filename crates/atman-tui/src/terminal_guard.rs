use std::io::{Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{Attribute, ResetColor, SetAttribute};
use crossterm::terminal::{
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, supports_keyboard_enhancement,
};
use crossterm::{execute, queue};

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard {
    prev_hook_slot: Option<Arc<Mutex<Option<PanicHook>>>>,
    input_mode_slot: Option<Arc<Mutex<Option<crate::terminal_input::ReadMode>>>>,
    owner: bool,
}

impl TerminalGuard {
    pub fn install() -> Result<Self> {
        // Idempotent: a second install() while the terminal is already
        // in alternate mode yields a "borrowed" guard that skips
        // LeaveAlternateScreen on drop. This lets an outer scope
        // (cmd_repl) hold the master guard across many session
        // switches without the inner run_tui teardown flashing the
        // real terminal back at the user each time.
        if TERMINAL_ACTIVE.load(Ordering::SeqCst) {
            return Ok(Self {
                prev_hook_slot: None,
                input_mode_slot: None,
                owner: false,
            });
        }
        let prev_hook = std::panic::take_hook();
        let prev_slot: Arc<Mutex<Option<PanicHook>>> = Arc::new(Mutex::new(Some(prev_hook)));
        let prev_for_hook = Arc::clone(&prev_slot);
        let input_mode = Arc::new(Mutex::new(None));
        let input_for_hook = Arc::clone(&input_mode);
        std::panic::set_hook(Box::new(move |info| {
            dispatch_panic_hook(
                atman_runtime::is_panic_capture_active(),
                || {
                    release_input_mode(&input_for_hook);
                    let _ = restore_terminal();
                },
                || {
                    if let Ok(guard) = prev_for_hook.lock()
                        && let Some(prev) = guard.as_ref()
                    {
                        prev(info);
                    }
                },
            );
        }));

        let guard = Self {
            prev_hook_slot: Some(prev_slot),
            input_mode_slot: Some(input_mode.clone()),
            owner: true,
        };
        enable_raw_mode().context("enable raw mode")?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        let bounded =
            crate::terminal_input::ReadMode::new().context("bound terminal input reads")?;
        *input_mode.lock().unwrap() = Some(bounded);
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("enter alternate screen")?;
        input_mode
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .activate()
            .context("activate terminal input reads")?;
        #[cfg(windows)]
        execute!(stdout(), crossterm::event::EnableFocusChange)
            .context("enable terminal focus input")?;
        if supports_keyboard_enhancement().unwrap_or(false) {
            let _ = execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        Ok(guard)
    }
}

fn dispatch_panic_hook(capture_active: bool, restore: impl FnOnce(), previous: impl FnOnce()) {
    if capture_active {
        return;
    }
    restore();
    previous();
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        if let Some(slot) = self.input_mode_slot.take() {
            release_input_mode(&slot);
        }
        let _ = restore_terminal();
        if let Some(slot) = self.prev_hook_slot.as_ref()
            && let Ok(mut slot) = slot.lock()
            && let Some(prev) = slot.take()
        {
            std::panic::set_hook(prev);
        }
    }
}

fn release_input_mode(slot: &Mutex<Option<crate::terminal_input::ReadMode>>) {
    let input = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(input);
}

fn restore_terminal() -> Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let mut output = stdout();
    let output_result = (|| {
        #[cfg(windows)]
        queue!(output, crossterm::event::DisableFocusChange)?;
        queue!(
            output,
            EndSynchronizedUpdate,
            DisableMouseCapture,
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            Show,
            SetAttribute(Attribute::Reset),
            ResetColor,
        )?;
        output.flush()
    })();
    let raw_result = disable_raw_mode();
    output_result.context("restore terminal output modes")?;
    raw_result.context("disable raw mode")?;
    Ok(())
}

/// Reassert shell-facing input and display modes after post-TUI output.
pub fn ensure_shell_mode() -> Result<()> {
    let mut output = stdout();
    let output_result = queue!(
        output,
        DisableMouseCapture,
        DisableBracketedPaste,
        Show,
        SetAttribute(Attribute::Reset),
        ResetColor,
    )
    .and_then(|()| output.flush());
    let raw_result = disable_raw_mode();
    output_result.context("restore shell output modes")?;
    raw_result.context("restore shell input mode")?;
    Ok(())
}

pub fn set_mouse_capture(enabled: bool) -> Result<()> {
    #[cfg(windows)]
    {
        crate::terminal_input::set_mouse_capture(enabled).context("set mouse capture")
    }
    #[cfg(not(windows))]
    {
        if enabled {
            execute!(stdout(), EnableMouseCapture).context("enable mouse capture")?;
        } else {
            execute!(stdout(), DisableMouseCapture).context("disable mouse capture")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_terminal_noops_when_flag_is_false() {
        TERMINAL_ACTIVE.store(false, Ordering::SeqCst);
        assert!(restore_terminal().is_ok());
        assert!(!TERMINAL_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn restore_terminal_flips_flag_to_false() {
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        let _ = restore_terminal();
        assert!(!TERMINAL_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn captured_panics_skip_terminal_restore_and_previous_hook() {
        let restores = std::cell::Cell::new(0);
        let previous_calls = std::cell::Cell::new(0);
        dispatch_panic_hook(
            true,
            || restores.set(restores.get() + 1),
            || previous_calls.set(previous_calls.get() + 1),
        );
        assert_eq!(restores.get(), 0);
        assert_eq!(previous_calls.get(), 0);

        dispatch_panic_hook(
            false,
            || restores.set(restores.get() + 1),
            || previous_calls.set(previous_calls.get() + 1),
        );
        assert_eq!(restores.get(), 1);
        assert_eq!(previous_calls.get(), 1);
    }
}
