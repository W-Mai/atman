use std::sync::Arc;

use atman_runtime::goal::{GoalAction, GoalController, GoalExecution, GoalSnapshot};
use atman_runtime::memory::goal::GoalStatus;
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Wrap},
};

use crate::input::{InputEditor, cursor_from_wrapped, wrapped_cursor_position};
use crate::keys::KeyAction;
use crate::settings_form::{Action, render_action, render_record};
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmCommand, WmEvent, WmEventResult,
};

pub(crate) fn status(snapshot: &GoalSnapshot) -> String {
    if let Some(owner) = &snapshot.paused_by
        && snapshot.goal.as_ref().is_some_and(|g| &g.owner != owner)
    {
        return "Paused by parent".into();
    }
    match snapshot.execution {
        GoalExecution::Pausing => format!("Pausing · {} in flight", snapshot.running_operations),
        GoalExecution::Finishing => "Paused · finishing turn".into(),
        GoalExecution::Running => "Running".into(),
        GoalExecution::Paused => snapshot
            .goal
            .as_ref()
            .and_then(|g| g.pause.as_ref())
            .map(|p| {
                match p.reason {
                    atman_runtime::memory::goal::GoalPauseReason::Completed => "Completed · paused",
                    atman_runtime::memory::goal::GoalPauseReason::NeedsInput => "Waiting for input",
                    atman_runtime::memory::goal::GoalPauseReason::Blocked => "Blocked · paused",
                    _ => "Paused",
                }
                .into()
            })
            .unwrap_or_else(|| "Paused".into()),
        GoalExecution::Idle => if snapshot.goal.is_some() {
            "Active · ready"
        } else {
            "No goal"
        }
        .into(),
    }
}

pub(crate) fn toggle(snapshot: &GoalSnapshot) -> GoalAction {
    if snapshot
        .goal
        .as_ref()
        .is_some_and(|g| g.status == GoalStatus::Active)
    {
        GoalAction::Pause
    } else {
        GoalAction::Resume
    }
}

pub(crate) fn command(
    control: &GoalController,
    target: Option<String>,
    action: GoalAction,
) -> WmCommand {
    let record = control.record();
    WmCommand::GoalControl {
        request_id: None,
        target,
        goal_id: record.as_ref().map(|g| g.id.clone()),
        revision: record.map(|g| g.revision),
        action,
    }
}

pub struct GoalPanelContent {
    session: Arc<atman_runtime::Session>,
    selected: usize,
    focus: usize,
    hover: Option<usize>,
    hits: Vec<(usize, Rect)>,
    text_rect: Rect,
    editor: Option<InputEditor>,
    edit_revision: Option<(String, u64)>,
    scroll: u16,
    confirm_clear: bool,
    cursor_visible: bool,
    pending_save: Option<u64>,
    save_error: Option<String>,
}

impl GoalPanelContent {
    pub fn new(session: Arc<atman_runtime::Session>) -> Self {
        Self {
            session,
            selected: 0,
            focus: 1,
            hover: None,
            hits: vec![],
            text_rect: Rect::default(),
            editor: None,
            edit_revision: None,
            scroll: 0,
            confirm_clear: false,
            cursor_visible: false,
            pending_save: None,
            save_error: None,
        }
    }

    fn choices(&self) -> Vec<(Option<String>, Arc<GoalController>)> {
        let mut entries = vec![(None, self.session.goal_control.clone())];
        entries.extend(
            self.session
                .flow_registry
                .managed_goals()
                .into_iter()
                .map(|(h, c)| (Some(h), c)),
        );
        entries
    }

    fn activate(&mut self, index: usize) -> Vec<WmCommand> {
        if self.pending_save.is_some() {
            return vec![];
        }
        let choices = self.choices();
        let (target, control) = &choices[self.selected.min(choices.len() - 1)];
        match index {
            0 if self.editor.is_none() => {
                self.selected = (self.selected + 1) % choices.len();
                self.scroll = 0;
                self.confirm_clear = false;
                vec![]
            }
            1 if self.editor.is_none() && control.record().is_some() => vec![command(
                control,
                target.clone(),
                toggle(&control.snapshot()),
            )],
            2 => {
                if let Some(editor) = self.editor.as_ref() {
                    if editor.buf().trim().is_empty() {
                        self.save_error = Some("Enter an objective before saving.".into());
                        return vec![];
                    }
                    static NEXT_SAVE: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(1);
                    let request_id = NEXT_SAVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.pending_save = Some(request_id);
                    self.save_error = None;
                    let expected = self.edit_revision.clone();
                    vec![WmCommand::GoalControl {
                        request_id: Some(request_id),
                        target: target.clone(),
                        goal_id: expected.as_ref().map(|e| e.0.clone()),
                        revision: expected.map(|e| e.1),
                        action: GoalAction::Set {
                            text: editor.buf().into(),
                        },
                    }]
                } else {
                    let record = control.record();
                    let mut editor = InputEditor::default();
                    if let Some(goal) = &record {
                        editor.insert_str(&goal.objective);
                    }
                    self.edit_revision = record.map(|g| (g.id, g.revision));
                    self.editor = Some(editor);
                    self.save_error = None;
                    self.focus = 2;
                    self.scroll = 0;
                    vec![]
                }
            }
            3 if control.record().is_some() && self.editor.is_none() => {
                if self.confirm_clear {
                    self.confirm_clear = false;
                    vec![command(control, target.clone(), GoalAction::Clear)]
                } else {
                    self.confirm_clear = true;
                    vec![]
                }
            }
            _ => vec![],
        }
    }
}

impl WindowComponent for GoalPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, _: &RenderCtx) -> Vec<HitRegion> {
        self.hits.clear();
        self.cursor_visible = false;
        if area.width < 8 || area.height < 9 {
            return vec![];
        }
        let t = crate::theme::theme();
        let width = area.width.saturating_sub(4).min(88);
        let x = area.x + (area.width - width) / 2;
        let top = Rect::new(x, area.y + 1, width, 3);
        let choices = self.choices();
        self.selected = self.selected.min(choices.len() - 1);
        let (target, control) = &choices[self.selected];
        let snapshot = control.snapshot();
        let name = target.as_deref().unwrap_or("Main agent");
        render_action(
            frame,
            top,
            &Action {
                label: &format!("{} / {} · {name}", self.selected + 1, choices.len()),
                shortcut: "← →",
                focused: self.focus == 0,
                hovered: self.hover == Some(0),
                disabled: self.editor.is_some(),
            },
            &t,
        );
        self.hits.push((0, top));
        let error_rows = self
            .save_error
            .as_ref()
            .map_or(0, |error| {
                crate::width::word_wrap(error, width.max(1) as usize).len() as u16
            })
            .min(area.height.saturating_sub(11));
        if let Some(error) = &self.save_error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(t.error.into())),
                Rect::new(x, top.bottom(), width, error_rows),
            );
        }
        let body = Rect::new(
            x,
            top.bottom() + 1 + error_rows,
            width,
            area.height.saturating_sub(10 + error_rows),
        );
        render_record(frame, body, self.editor.is_some(), false, &t);
        self.text_rect = Rect::new(
            body.x + 2,
            body.y,
            body.width.saturating_sub(4),
            body.height,
        );
        let text = self
            .editor
            .as_ref()
            .map(|e| e.buf().to_owned())
            .unwrap_or_else(|| {
                let goal = snapshot.goal.as_ref();
                format!(
                    "{}\n\n{}{}",
                    status(&snapshot),
                    goal.map(|g| g.objective.as_str())
                        .unwrap_or("Create a goal for work that should continue across turns."),
                    goal.and_then(|g| g.pause.as_ref())
                        .map(|p| format!("\n\n{}", p.summary))
                        .unwrap_or_default()
                )
            });
        let wrapped = crate::width::word_wrap(&text, self.text_rect.width.max(1) as usize);
        self.scroll = self
            .scroll
            .min((wrapped.len() as u16).saturating_sub(self.text_rect.height));
        if let Some(editor) = &self.editor {
            let (row, col) = wrapped_cursor_position(
                editor.buf(),
                editor.cursor(),
                self.text_rect.width.max(1) as usize,
            );
            if row < self.scroll as usize {
                self.scroll = row as u16;
            }
            if row >= (self.scroll + self.text_rect.height) as usize {
                self.scroll = (row + 1).saturating_sub(self.text_rect.height as usize) as u16;
            }
            if self.text_rect.height > 0 {
                frame.set_cursor_position((
                    self.text_rect.x + col as u16,
                    self.text_rect.y + row as u16 - self.scroll,
                ));
                self.cursor_visible = true;
            }
        }
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .style(Style::default().fg(t.tinted_fg.into())),
            self.text_rect,
        );
        let labels = [
            if matches!(toggle(&snapshot), GoalAction::Pause) {
                "Pause"
            } else {
                "Resume"
            },
            if self.pending_save.is_some() {
                "Saving…"
            } else if self.editor.is_some() {
                "Save"
            } else if snapshot.goal.is_some() {
                "Edit"
            } else {
                "New goal"
            },
            if self.confirm_clear {
                "Confirm clear"
            } else {
                "Clear"
            },
        ];
        let button_w = width.saturating_sub(4) / 3;
        for (i, label) in labels.iter().enumerate() {
            let index = i + 1;
            let rect = Rect::new(
                x + i as u16 * (button_w + 2),
                area.bottom() - 4,
                button_w,
                3,
            );
            render_action(
                frame,
                rect,
                &Action {
                    label,
                    shortcut: if index == 2 && self.editor.is_some() {
                        "Ctrl+S"
                    } else {
                        "Enter"
                    },
                    focused: self.focus == index,
                    hovered: self.hover == Some(index),
                    disabled: self.pending_save.is_some()
                        || (index != 2 && (snapshot.goal.is_none() || self.editor.is_some())),
                },
                &t,
            );
            self.hits.push((index, rect));
        }
        frame.render_widget(
            Paragraph::new(if self.editor.is_some() {
                "Ctrl+S save · Esc discard · click to place cursor"
            } else {
                "Tab focus · Space pause/resume · ↑↓ scroll · Esc close"
            })
            .style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(x, area.bottom() - 1, width, 1),
        );
        vec![]
    }

    fn handle_event(&mut self, event: &WmEvent, _: &mut EventCtx) -> WmEventResult {
        let mut commands = vec![];
        match event {
            WmEvent::GoalSaved { request_id, result } if self.pending_save == Some(*request_id) => {
                self.pending_save = None;
                match result {
                    Ok(()) => {
                        self.editor = None;
                        self.edit_revision = None;
                        self.save_error = None;
                        self.scroll = 0;
                    }
                    Err(error) => self.save_error = Some(error.clone()),
                }
            }
            WmEvent::GoalSaved { .. } => return WmEventResult::Ignored,
            _ if self.pending_save.is_some() => return WmEventResult::Consumed(vec![]),
            WmEvent::Mouse(mouse) => {
                let point = (mouse.column, mouse.row).into();
                self.hover = self
                    .hits
                    .iter()
                    .find(|(_, r)| r.contains(point))
                    .map(|(i, _)| *i);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) = self.hover {
                            self.focus = index;
                            commands = self.activate(index);
                        } else if self.text_rect.contains(point)
                            && let Some(editor) = &mut self.editor
                        {
                            editor.set_cursor(cursor_from_wrapped(
                                editor.buf(),
                                (mouse.row - self.text_rect.y + self.scroll) as usize,
                                (mouse.column - self.text_rect.x) as usize,
                                self.text_rect.width.max(1) as usize,
                            ));
                        }
                    }
                    MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                    _ => {}
                }
            }
            WmEvent::Paste(text) if self.editor.is_some() => {
                self.editor.as_mut().unwrap().insert_str(text)
            }
            WmEvent::Key(key) if self.editor.is_some() => match key {
                KeyAction::Escape => {
                    self.editor = None;
                    self.edit_revision = None;
                    self.save_error = None;
                    self.confirm_clear = false;
                }
                KeyAction::Save => commands = self.activate(2),
                KeyAction::HistoryUp => {
                    self.editor
                        .as_mut()
                        .unwrap()
                        .move_line_up_visual(self.text_rect.width.max(1) as usize);
                }
                KeyAction::HistoryDown => {
                    self.editor
                        .as_mut()
                        .unwrap()
                        .move_line_down_visual(self.text_rect.width.max(1) as usize);
                }
                KeyAction::Submit => self.editor.as_mut().unwrap().insert_str("\n"),
                _ => {
                    self.editor.as_mut().unwrap().handle_key(key);
                }
            },
            WmEvent::Key(key) => match key {
                KeyAction::Tab => self.focus = (self.focus + 1) % 4,
                KeyAction::BackTab => self.focus = (self.focus + 3) % 4,
                KeyAction::Submit => commands = self.activate(self.focus),
                KeyAction::Char(' ') | KeyAction::ToggleGoal => commands = self.activate(1),
                KeyAction::CursorRight if self.focus == 0 => commands = self.activate(0),
                KeyAction::CursorLeft if self.focus == 0 => {
                    let len = self.choices().len();
                    self.selected = (self.selected + len - 1) % len;
                    self.scroll = 0;
                }
                KeyAction::HistoryUp | KeyAction::PageUp => {
                    self.scroll = self.scroll.saturating_sub(3)
                }
                KeyAction::HistoryDown | KeyAction::PageDown => {
                    self.scroll = self.scroll.saturating_add(3)
                }
                KeyAction::Escape if self.confirm_clear => self.confirm_clear = false,
                _ => return WmEventResult::Ignored,
            },
            _ => return WmEventResult::Ignored,
        }
        WmEventResult::Consumed(commands)
    }
    fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }
    fn on_close(&mut self) -> crate::wm::component::CloseOutcome {
        if self.editor.is_some() {
            crate::wm::component::CloseOutcome::Block(
                "Save with Ctrl+S or discard with Esc before closing".into(),
            )
        } else {
            crate::wm::component::CloseOutcome::Close
        }
    }
    fn preferred_size(&self, _: Rect) -> SizeHint {
        SizeHint {
            min: (32, 14),
            max: None,
            preferred: (92, 26),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw(panel: &mut GoalPanelContent, terminal: &mut Terminal<TestBackend>) {
        let index = Default::default();
        let details = Default::default();
        let set = Default::default();
        let resources = Default::default();
        let prompts = Default::default();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: Default::default(),
            content_revision: 0,
            resources: &resources,
            prompts: &prompts,
            hovered_action: None,
            remove_armed: None,
        };
        terminal
            .draw(|frame| {
                panel.render_content(
                    frame.area(),
                    frame,
                    &RenderCtx {
                        window_id: crate::wm::WindowId(1),
                        snapshots: &[],
                        items: &[],
                        item_revisions: &[],
                        handle_index: &index,
                        detached_task_details: &details,
                        task_handle_index: &index,
                        workflow_run_to_panel: &index,
                        task_snapshots_revision: 0,
                        interaction_revision: 0,
                        animation_frame: 0,
                        expanded_tools: &set,
                        activity_nodes: &[],
                        mcp_servers: &[],
                        expanded_mcp_servers: &set,
                        mcp_selected: 0,
                        hovered_mcp_row: &None,
                        mcp_browser: &browser,
                        hovered_history_row: &None,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn goal_panel_uses_padded_actions_and_shared_keyboard_mouse_commands() {
        for (width, height) in [(92, 26), (40, 16)] {
            let session = Arc::new(atman_runtime::Session::open_ephemeral());
            session
                .goal_control
                .set("Finish the requested change", None)
                .unwrap();
            let mut panel = GoalPanelContent::new(session.clone());
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            draw(&mut panel, &mut terminal);
            assert_eq!(panel.hits.len(), 4);
            assert!(panel.hits.iter().all(|(_, rect)| rect.height == 3
                && rect.right() <= width
                && rect.bottom() <= height));
            let (_, rect) = panel.hits[1];
            assert!(rect.y >= panel.text_rect.bottom());
            let mut scroll = 0;
            let mut horizontal = 0;
            let mut ctx = EventCtx {
                scroll: &mut scroll,
                h_scroll: &mut horizontal,
            };
            for event in [
                WmEvent::Key(KeyAction::Char(' ')),
                WmEvent::Key(crate::keys::map(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('y'),
                    crossterm::event::KeyModifiers::CONTROL,
                ))),
                WmEvent::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row: rect.y + 1,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                }),
            ] {
                let WmEventResult::Consumed(commands) = panel.handle_event(&event, &mut ctx) else {
                    panic!("goal action ignored")
                };
                assert!(matches!(
                    commands.as_slice(),
                    [WmCommand::GoalControl {
                        target: None,
                        action: GoalAction::Pause,
                        revision: Some(1),
                        ..
                    }]
                ));
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                crate::wm::WindowManager::default().apply_commands(
                    &mut crate::app::AppState::new("goal-test".into(), None),
                    commands,
                    Some(&tx),
                );
                assert!(matches!(
                    rx.try_recv(),
                    Ok(crate::TuiControl::GoalControl {
                        target: None,
                        action: GoalAction::Pause,
                        revision: Some(1),
                        ..
                    })
                ));
            }
            panel.activate(2);
            let original = panel.selected;
            panel.activate(0);
            assert_eq!(
                panel.selected, original,
                "editing must not switch the target"
            );
            assert!(matches!(
                panel.on_close(),
                crate::wm::component::CloseOutcome::Block(_)
            ));
            for use_mouse in [false, true] {
                if panel.editor.is_none() {
                    panel.activate(2);
                }
                panel.editor.as_mut().unwrap().insert_str(" updated");
                draw(&mut panel, &mut terminal);
                let save = panel.hits.iter().find(|(i, _)| *i == 2).unwrap().1;
                let event = if use_mouse {
                    WmEvent::Mouse(crossterm::event::MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: save.x + 1,
                        row: save.y + 1,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    })
                } else {
                    WmEvent::Key(KeyAction::Save)
                };
                let WmEventResult::Consumed(commands) = panel.handle_event(&event, &mut ctx) else {
                    panic!("save ignored")
                };
                let [
                    WmCommand::GoalControl {
                        request_id: Some(request_id),
                        target,
                        goal_id,
                        revision,
                        action,
                    },
                ] = commands.as_slice()
                else {
                    panic!("missing save command")
                };
                assert!(panel.editor.is_some(), "wait for the persistence result");
                panel.handle_event(
                    &WmEvent::GoalSaved {
                        request_id: request_id + 1,
                        result: Ok(()),
                    },
                    &mut ctx,
                );
                assert!(panel.editor.is_some(), "ignore unrelated replies");
                let result = session
                    .control_goal(
                        target.as_deref(),
                        action.clone(),
                        goal_id.as_deref().zip(*revision),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                assert!(result.is_ok());
                panel.handle_event(
                    &WmEvent::GoalSaved {
                        request_id: *request_id,
                        result,
                    },
                    &mut ctx,
                );
                assert!(panel.editor.is_none(), "successful save exits editing");
            }
            panel.activate(2);
            let draft = panel.editor.as_ref().unwrap().buf().to_string();
            panel.activate(2);
            let request_id = panel.pending_save.unwrap();
            panel.handle_event(
                &WmEvent::GoalSaved {
                    request_id,
                    result: Err("Goal revision changed".into()),
                },
                &mut ctx,
            );
            assert_eq!(panel.editor.as_ref().unwrap().buf(), draft);
            assert_eq!(panel.save_error.as_deref(), Some("Goal revision changed"));
            panel.handle_event(&WmEvent::Key(KeyAction::Escape), &mut ctx);
            assert!(panel.editor.is_none());
        }
    }
}
