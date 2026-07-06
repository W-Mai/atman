use std::io::{Stdout, stdout};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast, mpsc};

pub mod app;
pub mod highlight;
pub mod input;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod output;
pub mod status;
pub mod terminal_guard;

use app::{AppState, NoteLevel};
use atman_runtime::stream::StreamFrame;
use input::{InputEditor, input_paragraph};
use keys::{KeyAction, map as map_key};
use terminal_guard::TerminalGuard;

pub enum TuiNote {
    Info(String),
    Warn(String),
    Error(String),
}

impl TuiNote {
    fn into_parts(self) -> (String, NoteLevel) {
        match self {
            Self::Info(t) => (t, NoteLevel::Info),
            Self::Warn(t) => (t, NoteLevel::Warn),
            Self::Error(t) => (t, NoteLevel::Error),
        }
    }
}

pub struct TuiHandle {
    pub session_id: String,
    pub goal: Option<String>,
    pub stream_rx: broadcast::Receiver<StreamFrame>,
    pub submit_tx: Option<mpsc::UnboundedSender<String>>,
    pub note_rx: Option<mpsc::UnboundedReceiver<TuiNote>>,
    pub shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl TuiHandle {
    pub fn from_session(session: &atman_runtime::Session) -> Self {
        Self {
            session_id: session.id().to_string(),
            goal: session.goal(),
            stream_rx: session.stream_subscribe(),
            submit_tx: None,
            note_rx: None,
            shutdown_rx: None,
        }
    }
}

pub async fn run_tui(handle: TuiHandle) -> Result<()> {
    let _guard = TerminalGuard::install()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    run_frames(&mut terminal, handle).await
}

async fn run_frames(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: TuiHandle,
) -> Result<()> {
    let mut app = AppState::new(handle.session_id.clone(), handle.goal.clone());
    let mut editor = InputEditor::default();
    let mut key_events = EventStream::new();
    let mut interrupt_prompt = false;
    let mut shutdown = handle.shutdown_rx.take();
    let mut sigterm = build_sigterm_stream();

    loop {
        terminal.draw(|f| render_frame(f, &app, &editor))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;
            _ = wait_shutdown(shutdown.as_mut()) => {
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            _ = wait_sigterm(sigterm.as_mut()) => {
                break;
            }
            key = key_events.next() => {
                if let Some(Ok(CtEvent::Key(ke))) = key {
                    handle_key(
                        map_key(ke),
                        &mut app,
                        &mut editor,
                        &mut interrupt_prompt,
                        handle.submit_tx.as_ref(),
                    );
                }
            }
            frame = handle.stream_rx.recv() => {
                if let Ok(frame) = frame {
                    app.apply_stream_frame(frame);
                    interrupt_prompt = false;
                }
            }
            note = recv_note(handle.note_rx.as_mut()) => {
                if let Some(n) = note {
                    let (text, level) = n.into_parts();
                    app.items.push(app::OutputItem::SystemNote { text, level });
                }
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn build_sigterm_stream() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn build_sigterm_stream() -> Option<()> {
    None
}

#[cfg(unix)]
async fn wait_sigterm(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            let _ = s.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn wait_sigterm(_sig: Option<&mut ()>) {
    std::future::pending().await
}

async fn recv_note(rx: Option<&mut mpsc::UnboundedReceiver<TuiNote>>) -> Option<TuiNote> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_shutdown(rx: Option<&mut tokio::sync::oneshot::Receiver<()>>) {
    match rx {
        Some(r) => {
            let _ = r.await;
        }
        None => std::future::pending().await,
    }
}

fn handle_key(
    action: KeyAction,
    app: &mut AppState,
    editor: &mut InputEditor,
    interrupt_prompt: &mut bool,
    submit_tx: Option<&mpsc::UnboundedSender<String>>,
) {
    match action {
        KeyAction::Char(c) => {
            editor.push_char(c);
            *interrupt_prompt = false;
        }
        KeyAction::Backspace => {
            editor.backspace();
            *interrupt_prompt = false;
        }
        KeyAction::Submit => {
            if let Some(line) = editor.submit() {
                app.push_user_turn(line.clone());
                if let Some(tx) = submit_tx {
                    let _ = tx.send(line);
                }
            }
            *interrupt_prompt = false;
        }
        KeyAction::HistoryUp => {
            editor.history_up();
            *interrupt_prompt = false;
        }
        KeyAction::HistoryDown => {
            editor.history_down();
            *interrupt_prompt = false;
        }
        KeyAction::NudgePrefill => {
            editor.prefill("/nudge ");
            *interrupt_prompt = false;
        }
        KeyAction::CoursePrefill => {
            editor.prefill("/course ");
            *interrupt_prompt = false;
        }
        KeyAction::RedirectPrefill => {
            editor.prefill("/redirect ");
            *interrupt_prompt = false;
        }
        KeyAction::HardStop => {
            editor.prefill("/hard-stop ");
            *interrupt_prompt = false;
        }
        KeyAction::ScrollUp | KeyAction::PageUp => {
            app.scroll_up(if matches!(action, KeyAction::PageUp) {
                10
            } else {
                1
            });
            *interrupt_prompt = false;
        }
        KeyAction::ScrollDown | KeyAction::PageDown => {
            app.scroll_down(if matches!(action, KeyAction::PageDown) {
                10
            } else {
                1
            });
            *interrupt_prompt = false;
        }
        KeyAction::Home => {
            app.scroll_offset = 0;
            app.follow_tail = false;
            *interrupt_prompt = false;
        }
        KeyAction::End => {
            app.scroll_to_tail();
            *interrupt_prompt = false;
        }
        KeyAction::Interrupt => {
            if *interrupt_prompt {
                app.should_quit = true;
            } else {
                *interrupt_prompt = true;
                app.items.push(app::OutputItem::SystemNote {
                    text: "press Ctrl+C again to quit".into(),
                    level: app::NoteLevel::Warn,
                });
            }
        }
        KeyAction::Quit => {
            app.should_quit = true;
        }
        KeyAction::Ignore => {
            *interrupt_prompt = false;
        }
    }
}

fn render_frame(f: &mut ratatui::Frame, app: &AppState, editor: &InputEditor) {
    let area = f.area();
    let input_height = compute_input_height(editor.buf(), area.width);
    let l = layout::compute(area, input_height);
    f.render_widget(
        status::render_bar(&app.session_id, app.goal.as_deref(), app.streaming),
        l.status,
    );
    if app.items.is_empty() {
        f.render_widget(output::empty_hint(), l.output);
    } else {
        let mut state = ratatui::widgets::ListState::default().with_offset(app.scroll_offset);
        f.render_stateful_widget(output::build_list(&app.items), l.output, &mut state);
    }
    f.render_widget(input_paragraph(editor.buf(), app.streaming), l.input);
}

fn compute_input_height(buf: &str, width: u16) -> u16 {
    let prompt_len = "atman> ".len() as u16;
    let usable = width.saturating_sub(prompt_len).max(1) as usize;
    let visible = buf.chars().count();
    let lines = visible.div_ceil(usable);
    (lines as u16).clamp(1, 5)
}
