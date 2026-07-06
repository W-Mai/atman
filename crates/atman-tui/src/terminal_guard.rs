use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct TerminalGuard {
    prev_hook_slot: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalGuard {
    pub fn install() -> Result<Self> {
        let prev_hook = std::panic::take_hook();
        let prev_slot: Arc<Mutex<Option<PanicHook>>> = Arc::new(Mutex::new(Some(prev_hook)));
        let prev_for_hook = Arc::clone(&prev_slot);
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            if let Ok(guard) = prev_for_hook.lock()
                && let Some(prev) = guard.as_ref()
            {
                prev(info);
            }
        }));

        enable_raw_mode().context("enable raw mode")?;
        execute!(
            stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
        .context("enter alternate screen")?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self {
            prev_hook_slot: prev_slot,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
        if let Ok(mut slot) = self.prev_hook_slot.lock()
            && let Some(prev) = slot.take()
        {
            std::panic::set_hook(prev);
        }
    }
}

fn restore_terminal() -> Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    let _ = execute!(
        stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    Ok(())
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
}
