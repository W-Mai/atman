//! View-owned overview/activity selection; execution remains with the host.

use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::{Clear, Paragraph},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Select(bool),
    Toggle,
    Back,
}

#[derive(Default)]
pub(crate) struct FocusRail {
    pub activity: bool,
    compact: bool,
    page_open: bool,
    hits: Vec<(Rect, Action)>,
    hovered: Option<Action>,
    scroll: [u16; 2],
    viewport: Option<AuxiliaryViewport>,
}

/// One coordinate transform for the rendered page, controls, and text selection.
#[derive(Clone, Copy)]
pub(crate) struct AuxiliaryViewport {
    pub canvas: Rect,
    pub visible: Rect,
    pub target: Rect,
}

impl AuxiliaryViewport {
    pub fn project(self, rect: Rect) -> Option<Rect> {
        let visible = rect.intersection(self.visible);
        (!visible.is_empty()).then(|| {
            Rect::new(
                visible.x,
                self.target.y + (visible.y - self.visible.y),
                visible.width,
                visible.height,
            )
        })
    }

    pub fn project_option(self, rect: &mut Option<Rect>) {
        *rect = rect.and_then(|rect| self.project(rect));
    }
}

impl FocusRail {
    pub fn configure(&mut self, compact: bool) {
        self.compact = compact;
        if !compact {
            self.page_open = false;
        }
        self.clear_hits();
        self.viewport = None;
    }

    pub fn page_open(&self) -> bool {
        self.compact && self.page_open
    }

    pub fn viewport(&mut self, area: Rect) -> AuxiliaryViewport {
        // Retain a normal sidebar layout in a short surface without rasterizing
        // unbounded history or changing the section renderers' content budgets.
        let canvas = Rect::new(area.x, 0, area.width, area.height.max(60));
        let scroll = &mut self.scroll[usize::from(self.activity)];
        *scroll = (*scroll).min(canvas.height.saturating_sub(area.height));
        let viewport = AuxiliaryViewport {
            canvas,
            visible: Rect::new(area.x, *scroll, area.width, area.height),
            target: area,
        };
        self.viewport = Some(viewport);
        viewport
    }

    fn scroll_page(&mut self, rows: i32) {
        if let Some(viewport) = self.viewport {
            let scroll = &mut self.scroll[usize::from(self.activity)];
            *scroll = (i32::from(*scroll) + rows).clamp(
                0,
                i32::from(viewport.canvas.height - viewport.target.height),
            ) as u16;
        }
    }

    pub fn clear_hits(&mut self) {
        self.hits.clear();
    }

    pub(crate) fn reveal(&mut self, rect: Rect) {
        if let Some(viewport) = self.viewport {
            let scroll = &mut self.scroll[usize::from(self.activity)];
            if rect.y < viewport.visible.y {
                *scroll = rect.y;
            } else if rect.bottom() > viewport.visible.bottom() {
                *scroll = rect.bottom().saturating_sub(viewport.visible.height);
            }
            *scroll = (*scroll).min(
                viewport
                    .canvas
                    .height
                    .saturating_sub(viewport.target.height),
            );
        }
    }

    pub fn content_area(area: Rect) -> Rect {
        let header = if area.width >= 24 { 4 } else { 7 };
        Rect::new(
            area.x,
            area.y + header.min(area.height),
            area.width,
            area.height.saturating_sub(header),
        )
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, activity_frame: Option<u32>) {
        self.clear_hits();
        let t = crate::theme::theme();
        for (index, activity) in [false, true].into_iter().enumerate() {
            let wide = area.width >= 24;
            let width = if wide {
                (area.width - 1) / 2
            } else {
                area.width
            };
            let rect = Rect::new(
                area.x + if wide { index as u16 * (width + 1) } else { 0 },
                area.y + if wide { 0 } else { index as u16 * 3 },
                width,
                3,
            )
            .intersection(area);
            if rect.height < 3 {
                continue;
            }
            let selected = activity == self.activity;
            let bg = if selected {
                t.work_bg.lerp(t.accent, 0.15)
            } else if self.hovered == Some(Action::Select(activity)) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(format!(
                    "\n{}",
                    match (wide, activity) {
                        (true, false) => "Overview",
                        (true, true) => "Activity",
                        (false, false) => "O",
                        (false, true) => "A",
                    }
                ))
                .alignment(Alignment::Center)
                .style(Style::default().fg(t.heading.into()).bg(bg)),
                rect,
            );
            if selected {
                frame.render_widget(
                    Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, 3),
                );
            }
            if activity && !selected {
                render_activity_sweep(frame, rect, bg, activity_frame);
            }
            self.hits.push((rect, Action::Select(activity)));
        }
    }

    pub fn render_tabs(&mut self, frame: &mut Frame, area: Rect, activity_frame: Option<u32>) {
        let t = crate::theme::theme();
        let actions: &[(Action, &str)] = if self.page_open() {
            &[
                (Action::Back, "Back Esc"),
                (Action::Select(false), "Overview"),
                (Action::Select(true), "Activity"),
            ]
        } else {
            &[
                (Action::Toggle, "≡"),
                (Action::Select(false), "Overview"),
                (Action::Select(true), "Activity"),
            ]
        };
        let width = area.width.saturating_sub(actions.len() as u16 - 1) / actions.len() as u16;
        for (index, &(action, label)) in actions.iter().enumerate() {
            let rect = Rect::new(
                area.x + index as u16 * (width + 1),
                area.y,
                width,
                area.height.min(3),
            );
            let selected = action == Action::Select(self.activity);
            let bg = if selected {
                t.work_bg.lerp(t.accent, 0.15)
            } else if self.hovered == Some(action) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(
                Paragraph::new(format!("\n{label}"))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(t.heading.into()).bg(bg)),
                rect,
            );
            if selected {
                frame.render_widget(
                    Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, rect.height),
                );
            }
            if action == Action::Select(true) && !selected {
                render_activity_sweep(frame, rect, bg, activity_frame);
            }
            self.hits.push((rect, action));
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Option<Action> {
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains((event.column, event.row).into()))
            .map(|(_, activity)| *activity);
        matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            .then_some(self.hovered)
            .flatten()
    }
}

fn render_activity_sweep(
    frame: &mut Frame,
    rect: Rect,
    background: ratatui::style::Color,
    activity_frame: Option<u32>,
) {
    if let Some(tick) = activity_frame.filter(|_| rect.height >= 3) {
        frame.render_widget(
            Paragraph::new(crate::output::loading_sweep(tick, rect.width))
                .alignment(Alignment::Center)
                .style(
                    Style::default()
                        .fg(crate::theme::theme().accent.into())
                        .bg(background),
                ),
            Rect::new(rect.x, rect.y + 2, rect.width, 1),
        );
    }
}

impl crate::app::AppState {
    pub(crate) fn configure_focus_rail(&mut self, compact: bool) {
        self.focus_rail.configure(compact);
        if compact && self.focus_sidebar_expanded() {
            self.focus_rail.page_open = true;
        }
    }

    pub(crate) fn reveal_activity_controls(&mut self) {
        if !self.focus_sidebar_expanded()
            || (self.focus_rail.compact && !self.focus_rail.page_open())
        {
            self.set_focus_sidebar(crate::sidebar::SidebarMode::Open);
        }
    }

    pub(crate) fn reveal_activity_viewport(
        &mut self,
        viewport: AuxiliaryViewport,
    ) -> AuxiliaryViewport {
        if let Some(rect) = self.flow_activity.take_reveal_rect() {
            self.focus_rail.reveal(rect);
            self.focus_rail.viewport(viewport.target)
        } else {
            viewport
        }
    }

    pub(crate) fn workspace_mouse(&mut self, mouse: MouseEvent) -> Option<crate::flow_hub::Action> {
        let hovered = self
            .workspace_rect
            .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()));
        if self.workspace_hovered != hovered {
            self.workspace_hovered = hovered;
            self.mark_visual_dirty();
        }
        (self.workspace_hovered && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)))
            .then(|| self.workspace_action())
            .flatten()
    }

    pub(crate) fn workspace_action(&self) -> Option<crate::flow_hub::Action> {
        self.items
            .snapshot()
            .filter(|_| crate::panel_registry::EXTENDED)
            .map(|snapshot| {
                crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::FlowWorkspace {
                    owner: snapshot.status.execution.address().clone(),
                })
            })
    }

    pub(crate) fn select_focus_rail(&mut self, activity: bool) {
        self.focus_rail.activity = activity;
        self.flow_activity.keyboard_focus = activity && !self.sidebar_collapsed;
        self.mark_visual_dirty();
    }

    pub(crate) fn apply_focus_action(&mut self, action: Action) {
        match action {
            Action::Select(activity) => self.select_focus_rail(activity),
            Action::Toggle => self.toggle_focus_sidebar(),
            Action::Back => {
                self.set_focus_sidebar(crate::sidebar::SidebarMode::Closed);
            }
        }
    }

    pub(crate) fn toggle_focus_sidebar(&mut self) {
        let open = if self.focus_rail.compact {
            !self.focus_rail.page_open()
        } else {
            !self.focus_sidebar_expanded()
        };
        self.set_focus_sidebar(if open {
            crate::sidebar::SidebarMode::Open
        } else {
            crate::sidebar::SidebarMode::Closed
        });
    }

    pub(crate) fn set_focus_sidebar(&mut self, mode: crate::sidebar::SidebarMode) {
        let open = mode == crate::sidebar::SidebarMode::Open;
        self.sidebar_mode = mode;
        self.sidebar_collapsed = !open;
        self.focus_rail.page_open = open && self.focus_rail.compact;
        if open {
            self.task_panel_collapsed = false;
            self.task_panel_runtime_collapsed = false;
            self.sidebar_upper_collapsed = false;
            self.sidebar_lower_collapsed = false;
            self.sidebar_upper_runtime_collapsed = false;
            self.sidebar_lower_runtime_collapsed = false;
        } else {
            self.sidebar_popup = None;
            self.flow_activity.release_focus();
        }
        self.save_ui_state();
        self.mark_visual_dirty();
    }

    pub(crate) fn focus_sidebar_expanded(&self) -> bool {
        !(self.sidebar_collapsed
            || (self.sidebar_upper_runtime_collapsed
                && self.sidebar_lower_runtime_collapsed
                && self.task_panel_runtime_collapsed))
    }

    pub(crate) fn toggle_focus_sidebar_panel(&mut self, upper: bool) {
        let upper_collapsed = self.sidebar_collapsed
            || self.sidebar_upper_collapsed
            || self.sidebar_upper_runtime_collapsed;
        let lower_collapsed = self.sidebar_collapsed
            || self.sidebar_lower_collapsed
            || self.sidebar_lower_runtime_collapsed;
        self.sidebar_collapsed = false;
        self.sidebar_mode = crate::sidebar::SidebarMode::Open;
        self.sidebar_upper_runtime_collapsed = false;
        self.sidebar_lower_runtime_collapsed = false;
        self.sidebar_upper_collapsed = if upper {
            !upper_collapsed
        } else {
            upper_collapsed
        };
        self.sidebar_lower_collapsed = if upper {
            lower_collapsed
        } else {
            !lower_collapsed
        };
        self.sidebar_collapsed = self.sidebar_upper_collapsed && self.sidebar_lower_collapsed;
        self.mark_visual_dirty();
        self.save_ui_state();
    }

    pub(crate) fn toggle_focus_activity(&mut self) {
        self.select_focus_rail(!self.focus_rail.activity);
    }

    /// Handle view navigation before window closure and composer editing.
    pub(crate) fn focus_page_event(&mut self, event: &Event) -> bool {
        use crate::keys::KeyAction;
        if !self.focus_rail.page_open() {
            return false;
        }
        if let Event::Key(key) = event {
            use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
            if key.kind == KeyEventKind::Release
                || (key.kind == KeyEventKind::Repeat
                    && !matches!(
                        crate::keys::map(*key),
                        KeyAction::HistoryUp
                            | KeyAction::HistoryDown
                            | KeyAction::PageUp
                            | KeyAction::PageDown
                    ))
            {
                return true;
            }
            if matches!(key.code, KeyCode::F(6 | 7 | 10))
                || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
                || (matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
                    && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return false;
            }
        }
        match event {
            Event::Key(key) => match crate::keys::map(*key) {
                KeyAction::Char('w' | 'W') if !self.focus_rail.activity => {
                    self.open_flow_hub = self.workspace_action();
                }
                KeyAction::Escape => {
                    if self.sidebar_popup.take().is_none() {
                        self.apply_focus_action(Action::Back);
                    }
                }
                KeyAction::Tab
                | KeyAction::BackTab
                | KeyAction::CursorLeft
                | KeyAction::CursorRight => self.select_focus_rail(!self.focus_rail.activity),
                KeyAction::ToggleActivity
                | KeyAction::OpenCommandPalette
                | KeyAction::OpenProjectHub
                | KeyAction::OpenRelations
                | KeyAction::ToggleGoal
                | KeyAction::SearchHistory
                | KeyAction::Quit
                | KeyAction::Interrupt
                | KeyAction::HardStop
                | KeyAction::HelpModal
                | KeyAction::ToggleMouseCapture
                | KeyAction::CyclePanelForward
                | KeyAction::CyclePanelBackward => return false,
                KeyAction::HistoryUp => self.focus_rail.scroll_page(-1),
                KeyAction::HistoryDown => self.focus_rail.scroll_page(1),
                KeyAction::PageUp | KeyAction::PageDown => {
                    let rows = self
                        .focus_rail
                        .viewport
                        .map_or(1, |v| i32::from(v.target.height));
                    self.focus_rail
                        .scroll_page(if crate::keys::map(*key) == KeyAction::PageUp {
                            -rows
                        } else {
                            rows
                        });
                }
                KeyAction::CursorHome => self.focus_rail.scroll_page(-i32::from(u16::MAX)),
                KeyAction::CursorEnd => self.focus_rail.scroll_page(i32::from(u16::MAX)),
                _ => {}
            },
            Event::Paste(_) => {}
            Event::Mouse(mouse)
                if self
                    .focus_rail
                    .viewport
                    .is_some_and(|v| v.target.contains((mouse.column, mouse.row).into())) =>
            {
                if mouse
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT)
                    && !self.focus_rail.activity
                {
                    return false;
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.focus_rail.scroll_page(-3),
                    MouseEventKind::ScrollDown => self.focus_rail.scroll_page(3),
                    _ => return false,
                }
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_animation_uses_all_sources_independently_of_selection() {
        use crate::task_panel::{ActivityNode, ActivityStatus};
        use atman_runtime::{TaskId, TaskKind, TaskSnapshot, TaskStatus};
        let mut app = crate::app::AppState::new("activity-test".into(), None);
        app.sidebar_collapsed = true;
        app.focus_rail.activity = false;
        assert!(!app.has_active_animation());
        app.activity_nodes.push(ActivityNode {
            run_id: "run".into(),
            node_id: "node".into(),
            parent_node_id: None,
            label: "Working".into(),
            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                path: "test.work".into(),
            },
            status: ActivityStatus::Running,
            started_at: std::time::Instant::now(),
            ended_at: None,
        });
        assert!(app.has_active_animation());
        app.activity_nodes[0].status = ActivityStatus::Ok;
        assert!(!app.has_active_animation());
        app.task_snapshots.push(TaskSnapshot {
            id: TaskId(uuid::Uuid::now_v7()),
            kind: TaskKind::Bash,
            label: "Background task".into(),
            command: None,
            status: TaskStatus::Running,
            started_at: std::time::Instant::now(),
            ended_at: None,
            source_handle: "task".into(),
            session_id: "activity-test".into(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        });
        for (status, active) in [
            (TaskStatus::Running, true),
            (TaskStatus::Killing, true),
            (TaskStatus::Ok, false),
        ] {
            app.task_snapshots[0].status = status;
            assert_eq!(app.has_active_animation(), active);
        }
        let address = atman_runtime::flow_store::FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let mut state = atman_rt::FlowInstance::prepared(address.clone());
        state.activate().unwrap();
        app.flow_activity.snapshot.flows.push(
            atman_runtime::flow_runtime::activity::ActivityFlow {
                registration_order: 0,
                address,
                title: "Worker".into(),
                preview: String::new(),
                state: Some(state.clone()),
                error: None,
            },
        );
        assert!(!app.has_active_animation());
        state
            .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
            .unwrap();
        app.flow_activity.snapshot.flows[0].state = Some(state);
        for selected in [true, false] {
            app.focus_rail.activity = selected;
            assert!(app.has_active_animation());
        }
    }

    #[test]
    fn activity_sweep_preserves_control_geometry_in_every_surface() {
        for (tabs, width) in [(false, 72), (false, 5), (true, 40)] {
            for selected in [false, true] {
                let mut rail = FocusRail {
                    activity: selected,
                    ..Default::default()
                };
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
                let area = Rect::new(3, 2, width, if tabs { 3 } else { 6 });
                let mut previous = None;
                for tick in std::iter::once(None)
                    .chain((0..12).map(Some))
                    .chain(std::iter::once(None))
                {
                    terminal
                        .draw(|frame| {
                            rail.clear_hits();
                            if tabs {
                                rail.render_tabs(frame, area, tick);
                            } else {
                                rail.render(frame, area, tick);
                            }
                        })
                        .unwrap();
                    if let Some(hits) = &previous {
                        assert_eq!(&rail.hits, hits);
                    }
                    previous = Some(rail.hits.clone());
                    let rect = rail
                        .hits
                        .iter()
                        .find(|(_, action)| *action == Action::Select(true))
                        .unwrap()
                        .0;
                    let row = |y| {
                        (rect.x..rect.right())
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    };
                    assert_eq!(
                        row(rect.y + 1).trim_start_matches('▌').trim(),
                        if !tabs && width < 24 { "A" } else { "Activity" }
                    );
                    if let Some(tick) = tick.filter(|_| !selected) {
                        assert_eq!(
                            row(rect.y + 2).trim(),
                            crate::output::loading_sweep(tick, rect.width)
                        );
                        assert_eq!(row(rect.y + 2).matches('━').count(), 2);
                    } else {
                        assert!(!row(rect.y + 2).contains('━'));
                    }
                    for y in rect.y..rect.bottom() {
                        assert_eq!(
                            rail.mouse(MouseEvent {
                                kind: MouseEventKind::Down(MouseButton::Left),
                                column: rect.x + 2,
                                row: y,
                                modifiers: crossterm::event::KeyModifiers::NONE
                            }),
                            Some(Action::Select(true))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn activity_key_dispatch_survives_redraw() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut ui = crate::UiState::new(crate::app::AppState::new("test".into(), None));
        let mut editor = crate::input::InputEditor::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 50)).unwrap();
        for (key, selected) in [
            (
                KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL),
                true,
            ),
            (
                KeyEvent::new(KeyCode::Char('\u{1d}'), KeyModifiers::NONE),
                false,
            ),
            (
                KeyEvent::new(
                    KeyCode::Char('a'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                true,
            ),
        ] {
            let action = crate::keys::map(key);
            let (consumed, commands) = ui.wm.dispatch_key(&action, &mut ui.app, None);
            ui.wm.apply_commands(&mut ui.app, commands, None);
            if !consumed {
                crate::key_handler::handle_key(action, &mut ui, &mut editor, &mut None, None, None);
            }
            assert_eq!(ui.app.focus_rail.activity, selected);
            for _ in 0..3 {
                terminal
                    .draw(|f| crate::render::render_frame(f, &mut ui, &editor))
                    .unwrap();
                assert_eq!(ui.app.focus_rail.activity, selected);
            }
        }
    }

    #[test]
    fn tab_selection_and_shortcuts_preserve_expansion_in_every_surface() {
        for compact in [false, true] {
            for open in [false, true] {
                let mut app = crate::app::AppState::new("test".into(), None);
                app.focus_rail.configure(compact);
                app.set_focus_sidebar(if open {
                    crate::sidebar::SidebarMode::Open
                } else {
                    crate::sidebar::SidebarMode::Closed
                });
                for activity in [true, false, true] {
                    app.select_focus_rail(activity);
                    assert_eq!(app.focus_sidebar_expanded(), open);
                    assert_eq!(app.focus_rail.page_open(), compact && open);
                    app.toggle_focus_activity();
                    assert_eq!(app.focus_rail.activity, !activity);
                    assert_eq!(app.focus_sidebar_expanded(), open);
                    assert_eq!(app.focus_rail.page_open(), compact && open);
                }
            }
        }
    }

    #[test]
    fn explicit_sidebar_commands_and_toggle_share_layout_state() {
        use crate::sidebar::SidebarMode;
        for compact in [false, true] {
            let mut app = crate::app::AppState::new("test".into(), None);
            app.focus_rail.configure(compact);
            for open in [false, false, true, true, false] {
                app.sidebar_upper_collapsed = true;
                app.sidebar_lower_collapsed = true;
                app.task_panel_collapsed = true;
                app.set_focus_sidebar(if open {
                    SidebarMode::Open
                } else {
                    SidebarMode::Closed
                });
                assert_eq!(
                    if compact {
                        app.focus_rail.page_open()
                    } else {
                        !app.sidebar_collapsed
                    },
                    open
                );
                if open {
                    assert!(app.focus_sidebar_expanded());
                }
            }
            app.toggle_focus_sidebar();
            assert_eq!(app.sidebar_mode, SidebarMode::Open);
            assert!(if compact {
                app.focus_rail.page_open()
            } else {
                !app.sidebar_collapsed
            });
        }
    }

    #[test]
    fn panel_controls_use_effective_collapse_and_preserve_the_other_panel() {
        for mask in 0..32 {
            for upper in [true, false] {
                let mut app = crate::app::AppState::new("test".into(), None);
                app.sidebar_collapsed = mask & 1 != 0;
                app.sidebar_upper_collapsed = mask & 2 != 0;
                app.sidebar_lower_collapsed = mask & 4 != 0;
                app.sidebar_upper_runtime_collapsed = mask & 8 != 0;
                app.sidebar_lower_runtime_collapsed = mask & 16 != 0;
                let before = (mask & (1 | 2 | 8) != 0, mask & (1 | 4 | 16) != 0);
                app.toggle_focus_sidebar_panel(upper);
                let expected = if upper {
                    (!before.0, before.1)
                } else {
                    (before.0, !before.1)
                };
                assert_eq!(
                    (app.sidebar_upper_collapsed, app.sidebar_lower_collapsed),
                    expected
                );
                assert_eq!(app.sidebar_collapsed, expected.0 && expected.1);
                assert!(
                    !app.sidebar_upper_runtime_collapsed && !app.sidebar_lower_runtime_collapsed
                );
                assert_eq!(app.focus_sidebar_expanded(), !expected.0 || !expected.1);
                let snapshot = crate::states::PersistedUiState::snapshot(&app);
                snapshot.apply(&mut app);
                assert_eq!(app.focus_sidebar_expanded(), !expected.0 || !expected.1);
                if !app.focus_sidebar_expanded() {
                    app.toggle_focus_sidebar();
                    assert!(app.focus_sidebar_expanded());
                }
            }
        }
    }

    #[test]
    fn auxiliary_scroll_is_view_owned_bounded_and_independent_between_pages() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::app::AppState::new("test".into(), None);
        app.focus_rail.configure(true);
        app.set_focus_sidebar(crate::sidebar::SidebarMode::Open);
        app.select_focus_rail(false);
        let area = Rect::new(7, 9, 40, 7);
        app.focus_rail.viewport(area);
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        for code in [KeyCode::Tab, KeyCode::BackTab] {
            assert!(!app.focus_page_event(&Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))));
        }
        assert!(app.focus_page_event(&key(KeyCode::End)));
        assert_eq!(app.focus_rail.viewport(area).visible.y, 53);
        assert!(app.focus_page_event(&key(KeyCode::Down)));
        assert_eq!(app.focus_rail.viewport(area).visible.y, 53);
        app.select_focus_rail(true);
        assert_eq!(app.focus_rail.viewport(area).visible.y, 0);
        assert!(app.focus_page_event(&key(KeyCode::PageDown)));
        assert_eq!(app.focus_rail.viewport(area).visible.y, 7);
        app.select_focus_rail(false);
        assert_eq!(app.focus_rail.viewport(area).visible.y, 53);
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            row: 10,
            column: 8,
            modifiers: KeyModifiers::SHIFT,
        };
        assert!(!app.focus_page_event(&Event::Mouse(mouse)));
        assert!(app.focus_page_event(&Event::Mouse(MouseEvent {
            modifiers: KeyModifiers::NONE,
            ..mouse
        })));
        assert_eq!(app.focus_rail.viewport(area).visible.y, 50);
        assert!(app.focus_page_event(&key(KeyCode::Home)));
        assert_eq!(app.focus_rail.viewport(area).visible.y, 0);
        app.sidebar_popup = Some(crate::sidebar::SidebarPopupKind::Plan(0));
        assert!(app.focus_page_event(&key(KeyCode::Esc)));
        assert!(app.sidebar_popup.is_none());
        assert!(app.focus_rail.page_open());
        assert!(app.focus_page_event(&key(KeyCode::Esc)));
        assert!(!app.focus_rail.page_open());
        let edge = AuxiliaryViewport {
            canvas: Rect::new(0, 0, 40, 60),
            visible: Rect::new(0, 53, 40, 7),
            target: Rect::new(0, u16::MAX - 7, 40, 7),
        };
        assert_eq!(
            edge.project(Rect::new(0, 59, 1, 1)),
            Some(Rect::new(0, u16::MAX - 1, 1, 1))
        );
    }

    #[test]
    fn rail_and_conversation_keep_disjoint_bounds_and_full_button_targets() {
        for (x, y) in [(0, 0), (7, 6)] {
            for width in [40, 60, 100, 140, 200] {
                for height in [8, 24, 45, 80] {
                    for expanded in [false, true] {
                        let area = Rect::new(x, y, width, height);
                        let layout = crate::layout::FocusLayout::new(area, true, expanded);
                        let sidebar = layout.rail.or(layout.tabs).unwrap();
                        assert!(layout.conversation.width >= 40);
                        if layout.tabs.is_some() {
                            assert_eq!(layout.conversation.width, area.width);
                        }
                        assert_eq!(sidebar.intersection(area), sidebar);
                        assert_eq!(layout.conversation.intersection(area), layout.conversation);
                        assert!(sidebar.intersection(layout.conversation).is_empty());
                        let input = crate::layout::compute_input_rect(layout.conversation, 3);
                        assert!(sidebar.intersection(input).is_empty());
                        let mut rail = FocusRail::default();
                        let mut terminal = ratatui::Terminal::new(
                            ratatui::backend::TestBackend::new(area.right(), area.bottom()),
                        )
                        .unwrap();
                        terminal
                            .draw(|frame| {
                                if layout.tabs.is_some() {
                                    rail.render_tabs(frame, sidebar, None);
                                } else {
                                    rail.render(frame, sidebar, None);
                                }
                            })
                            .unwrap();
                        for (rect, activity) in rail.hits.clone() {
                            for row in rect.y..rect.bottom() {
                                for column in rect.x..rect.right() {
                                    assert_eq!(
                                        rail.mouse(MouseEvent {
                                            kind: MouseEventKind::Down(MouseButton::Left),
                                            row,
                                            column,
                                            modifiers: crossterm::event::KeyModifiers::NONE
                                        }),
                                        Some(activity)
                                    );
                                }
                            }
                            assert!(
                                rect.intersection(FocusRail::content_area(sidebar))
                                    .is_empty()
                            );
                        }
                        rail.clear_hits();
                        assert!(
                            rail.mouse(MouseEvent {
                                kind: MouseEventKind::Down(MouseButton::Left),
                                row: sidebar.y,
                                column: sidebar.x,
                                modifiers: crossterm::event::KeyModifiers::NONE
                            })
                            .is_none()
                        );
                    }
                }
            }
        }
    }
}
