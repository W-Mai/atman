use std::io::{Stdout, stdout};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::{broadcast, mpsc};

pub mod app;
pub mod highlight;
pub mod history;
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

pub enum TuiControl {
    CancelFlow,
}

pub struct TuiHandle {
    pub session_id: String,
    pub goal: Option<String>,
    pub stream_rx: broadcast::Receiver<StreamFrame>,
    pub submit_tx: Option<mpsc::UnboundedSender<String>>,
    pub note_rx: Option<mpsc::UnboundedReceiver<TuiNote>>,
    pub shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    pub control_tx: Option<mpsc::UnboundedSender<TuiControl>>,
    pub initial_items: Vec<app::OutputItem>,
    pub goal_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    pub context_rx: Option<tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
    pub attach_rx: Option<tokio::sync::watch::Receiver<usize>>,
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
            control_tx: None,
            initial_items: Vec::new(),
            goal_rx: Some(session.subscribe_goal()),
            context_rx: Some(session.subscribe_context()),
            attach_rx: Some(session.subscribe_attach()),
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
    let mut app = AppState::new(handle.session_id.clone(), handle.goal.clone())
        .with_initial_items(std::mem::take(&mut handle.initial_items));
    let mut editor = InputEditor::default();
    let mut key_events = EventStream::new();
    let mut interrupt_prompt = false;
    let mut shutdown = handle.shutdown_rx.take();
    let mut sigterm = build_sigterm_stream();

    loop {
        terminal.draw(|f| render_frame(f, &mut app, &editor))?;

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
                match key {
                    Some(Ok(CtEvent::Key(ke))) => {
                        handle_key(
                            map_key(ke),
                            &mut app,
                            &mut editor,
                            &mut interrupt_prompt,
                            handle.submit_tx.as_ref(),
                            handle.control_tx.as_ref(),
                        );
                    }
                    Some(Ok(CtEvent::Paste(s))) => {
                        editor.insert_str(&s);
                        interrupt_prompt = false;
                    }
                    Some(Ok(CtEvent::Resize(_, _))) => {}
                    _ => {}
                }
            }
            frame = handle.stream_rx.recv() => {
                match frame {
                    Ok(frame) => {
                        app.apply_stream_frame(frame);
                        interrupt_prompt = false;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        app.record_lag(n, std::time::Instant::now());
                        interrupt_prompt = false;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            note = recv_note(handle.note_rx.as_mut()) => {
                if let Some(n) = note {
                    let (text, level) = n.into_parts();
                    app.push_note(text, level);
                }
            }
            _ = wait_goal_change(handle.goal_rx.as_mut()) => {
                if let Some(rx) = handle.goal_rx.as_mut() {
                    app.goal = rx.borrow().clone();
                }
            }
            _ = wait_context_change(handle.context_rx.as_mut()) => {
                if let Some(rx) = handle.context_rx.as_mut() {
                    app.context = rx.borrow().clone();
                }
            }
            _ = wait_attach_change(handle.attach_rx.as_mut()) => {
                if let Some(rx) = handle.attach_rx.as_mut() {
                    app.attach_count = *rx.borrow();
                }
            }
        }
    }
    Ok(())
}

async fn wait_goal_change(rx: Option<&mut tokio::sync::watch::Receiver<Option<String>>>) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_context_change(
    rx: Option<&mut tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_attach_change(rx: Option<&mut tokio::sync::watch::Receiver<usize>>) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
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
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    match action {
        KeyAction::Char(c) => {
            editor.insert_char(c);
            *interrupt_prompt = false;
        }
        KeyAction::Backspace => {
            editor.backspace();
            *interrupt_prompt = false;
        }
        KeyAction::DeleteWordBackward => {
            editor.delete_word_backward();
            *interrupt_prompt = false;
        }
        KeyAction::Newline => {
            editor.insert_newline();
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
        KeyAction::CursorLeft => {
            editor.move_left();
            *interrupt_prompt = false;
        }
        KeyAction::CursorRight => {
            editor.move_right();
            *interrupt_prompt = false;
        }
        KeyAction::CursorHome => {
            editor.move_home();
            *interrupt_prompt = false;
        }
        KeyAction::CursorEnd => {
            editor.move_end();
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
            app.scroll_to_top();
            *interrupt_prompt = false;
        }
        KeyAction::End => {
            app.scroll_to_tail();
            *interrupt_prompt = false;
        }
        KeyAction::Escape => {
            if app.streaming {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CancelFlow);
                }
                app.push_note("cancel requested", app::NoteLevel::Warn);
            } else if !editor.buf().is_empty() {
                editor.clear();
            }
            *interrupt_prompt = false;
        }
        KeyAction::Interrupt => {
            if *interrupt_prompt {
                app.should_quit = true;
            } else {
                *interrupt_prompt = true;
                app.push_note("press Ctrl+C again to quit", app::NoteLevel::Warn);
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

fn render_frame(f: &mut ratatui::Frame, app: &mut AppState, editor: &InputEditor) {
    let area = f.area();
    if area.width < 40 || area.height < 8 {
        let msg = Paragraph::new(Line::from("terminal too small (need 40×8)"))
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }
    let input_height = compute_input_height(editor.buf(), area.width);
    let compact_status = area.width < layout::SIDEBAR_MIN_TOTAL_WIDTH;
    let status_height = if compact_status { 2 } else { 1 };
    let l = layout::compute_ex(area, input_height, true, status_height);
    f.render_widget(
        status::render_bar(status::StatusInputs {
            session_id: &app.session_id,
            goal: app.goal.as_deref(),
            streaming: app.streaming,
            context: &app.context,
            attach_count: app.attach_count,
            include_compact_line: compact_status,
        }),
        l.status,
    );
    let transcript_area = l.transcript;
    if app.items.is_empty() {
        app.resolve_scroll(0, transcript_area.height);
        f.render_widget(output::empty_hint(), transcript_area);
    } else {
        let lines = output::build_lines(&app.items);
        let paragraph =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        let total_rows = paragraph.line_count(transcript_area.width) as u16;
        app.resolve_scroll(total_rows, transcript_area.height);
        f.render_widget(paragraph.scroll((app.scroll_offset, 0)), transcript_area);
    }
    if let Some(sidebar) = l.sidebar {
        f.render_widget(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)),
            sidebar,
        );
    }
    f.render_widget(
        input_paragraph(
            editor.buf(),
            editor.cursor(),
            app.streaming,
            app.pending_below_rows(),
        ),
        l.input,
    );
}

fn compute_input_height(buf: &str, width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let border_padding: u16 = 2;
    let prompt_len: u16 = 2;
    let usable = width.saturating_sub(border_padding + prompt_len).max(1) as usize;
    let mut wrapped_rows: usize = 0;
    for logical_line in buf.split('\n') {
        let w = logical_line.width().max(1);
        wrapped_rows += w.div_ceil(usable);
    }
    let content = wrapped_rows.max(1) as u16;
    (content + border_padding).clamp(3, 9)
}
