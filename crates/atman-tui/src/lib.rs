use std::io::{Stdout, stdout};

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub mod app;
pub mod highlight;
pub mod input;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod output;
pub mod status;

use app::AppState;
use input::{InputEditor, input_paragraph};
use keys::{KeyAction, map as map_key};

pub async fn run_tui(session: &atman_runtime::Session) -> Result<()> {
    enable_raw_mode().context("enable raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend).context("terminal init")?;

    let result = run_frames(&mut terminal, session).await;

    let mut out = stdout();
    let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
    let _ = disable_raw_mode();
    result
}

async fn run_frames(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    session: &atman_runtime::Session,
) -> Result<()> {
    let goal = session.goal();
    let mut app = AppState::new(session.id().to_string(), goal);
    let mut editor = InputEditor::default();
    let mut key_events = EventStream::new();
    let mut stream_rx = session.stream_subscribe();
    let mut interrupt_prompt = false;

    loop {
        terminal.draw(|f| render_frame(f, &app, &editor))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;
            key = key_events.next() => {
                if let Some(Ok(CtEvent::Key(ke))) = key {
                    handle_key(map_key(ke), &mut app, &mut editor, &mut interrupt_prompt);
                }
            }
            frame = stream_rx.recv() => {
                if let Ok(frame) = frame {
                    app.apply_stream_frame(frame);
                    interrupt_prompt = false;
                }
            }
        }
    }
    Ok(())
}

fn handle_key(
    action: KeyAction,
    app: &mut AppState,
    editor: &mut InputEditor,
    interrupt_prompt: &mut bool,
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
                app.push_user_turn(line);
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
