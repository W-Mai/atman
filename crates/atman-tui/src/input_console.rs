//! Source-bound permission editing inside the composer's existing text area.

use atman_runtime::trust::{EscalationPolicy, TrustConfig, TrustMode};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::{
    ControlSender, TuiControl, app::AppState, control::ControlTicket, flow_view::DocumentIdentity,
    input::InputAction, keys::KeyAction,
};

const POLICIES: [EscalationPolicy; 3] = [
    EscalationPolicy::Deny,
    EscalationPolicy::Ask,
    EscalationPolicy::Allow,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Mode(TrustMode),
    Policy(EscalationPolicy),
    More,
    Close,
}

struct Pending {
    ticket: Option<ControlTicket>,
    previous: TrustMode,
    requested: TrustConfig,
}

#[derive(Default)]
pub(crate) struct Permissions {
    pub(crate) open: bool,
    document: Option<DocumentIdentity>,
    selected: Option<Action>,
    hovered: Option<Action>,
    hits: Vec<(Action, Rect)>,
    bounds: Rect,
    pending: Option<Pending>,
    error: Option<String>,
}

impl Permissions {
    pub(crate) fn open(&mut self, app: &AppState, policy: bool) {
        if app.items.is_archive()
            || (self.pending.is_some() && self.document != Some(app.items.identity()))
        {
            return;
        }
        if self.document != Some(app.items.identity()) {
            self.error = None;
        }
        self.document = Some(app.items.identity());
        self.open = true;
        self.hits.clear();
        self.hovered = None;
        self.selected = Some(if policy && app.trust.mode == TrustMode::Eager {
            Action::Policy(app.trust.escalation)
        } else {
            Action::Mode(app.trust.mode)
        });
    }

    fn close(&mut self, app: &mut AppState) {
        self.open = false;
        self.hits.clear();
        self.hovered = None;
        self.bounds = Rect::default();
        app.input_hover = None;
        app.mark_visual_dirty();
    }

    fn current(&mut self, app: &mut AppState) -> bool {
        if !self.open || app.items.is_archive() || self.document != Some(app.items.identity()) {
            self.close(app);
            return false;
        }
        true
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn animates(&self) -> bool {
        self.open && self.pending.is_some() && !self.bounds.is_empty()
    }

    pub(crate) fn finish(
        &mut self,
        ticket: Option<&ControlTicket>,
        result: &Result<TrustConfig, String>,
    ) -> Option<TrustMode> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.ticket.as_ref() == ticket)
        {
            return None;
        }
        let pending = self.pending.take()?;
        self.error = result.as_ref().err().cloned();
        Some(pending.previous)
    }

    fn actions(&self, app: &AppState) -> Vec<Action> {
        let mut actions: Vec<_> = TrustMode::all().into_iter().map(Action::Mode).collect();
        if app.trust.mode == TrustMode::Eager {
            actions.extend(POLICIES.into_iter().map(Action::Policy));
        }
        actions.extend([Action::More, Action::Close]);
        actions
    }

    fn activate(&mut self, action: Action, app: &mut AppState, tx: Option<&ControlSender>) -> bool {
        if action == Action::Close {
            self.close(app);
            return false;
        }
        if self.pending.is_some() {
            return false;
        }
        if action == Action::More {
            self.close(app);
            return true;
        }
        let mut requested = app.trust.clone();
        match action {
            Action::Mode(mode) => requested.mode = mode,
            Action::Policy(policy) if app.trust.mode == TrustMode::Eager => {
                requested.escalation = policy;
            }
            _ => return false,
        }
        if requested == app.trust {
            self.error = None;
            return false;
        }
        let sent = tx.map(|tx| tx.send_tracked(TuiControl::UpdateTrust(requested.clone())));
        match sent {
            Some(Ok(ticket)) => {
                self.pending = Some(Pending {
                    ticket,
                    previous: app.trust.mode,
                    requested,
                });
                self.error = None;
            }
            _ => {
                self.error =
                    Some("Permission settings are unavailable; retry when connected".into())
            }
        }
        app.mark_visual_dirty();
        false
    }

    pub(crate) fn key(
        &mut self,
        key: &KeyAction,
        app: &mut AppState,
        tx: Option<&ControlSender>,
    ) -> bool {
        if !self.current(app) {
            return false;
        }
        let actions = self.actions(app);
        let selected = self.selected.unwrap_or(Action::Mode(app.trust.mode));
        let index = actions
            .iter()
            .position(|action| *action == selected)
            .unwrap_or(0);
        match key {
            KeyAction::Escape | KeyAction::Interrupt => self.close(app),
            KeyAction::Quit => {
                app.should_quit = true;
                self.close(app);
            }
            KeyAction::Submit => return self.activate(selected, app, tx),
            KeyAction::Tab | KeyAction::CursorRight | KeyAction::HistoryDown => {
                self.selected = Some(actions[(index + 1) % actions.len()]);
                self.hovered = None;
            }
            KeyAction::BackTab | KeyAction::CursorLeft | KeyAction::HistoryUp => {
                self.selected = Some(actions[(index + actions.len() - 1) % actions.len()]);
                self.hovered = None;
            }
            KeyAction::Home | KeyAction::CursorHome => self.selected = actions.first().copied(),
            KeyAction::End | KeyAction::CursorEnd => self.selected = actions.last().copied(),
            KeyAction::CycleReasoning => {
                app.reconcile_input_reasoning();
                app.cycle_input_reasoning();
                app.reasoning_meter.show_feedback();
            }
            _ => {}
        }
        app.input_hover = (self.selected == Some(Action::Close)).then_some(InputAction::Attach);
        app.mark_visual_dirty();
        false
    }

    pub(crate) fn mouse(
        &mut self,
        event: &MouseEvent,
        app: &mut AppState,
        tx: Option<&ControlSender>,
    ) -> bool {
        if !self.current(app) {
            return false;
        }
        let point = (event.column, event.row).into();
        let action = self
            .hits
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(action, _)| *action);
        self.hovered = action;
        app.input_hover = (action == Some(Action::Close)).then_some(InputAction::Attach);
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) = action {
                self.selected = Some(action);
                return self.activate(action, app, tx);
            }
            if !self.bounds.contains(point) {
                self.close(app);
            }
        }
        app.mark_visual_dirty();
        false
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, app: &mut AppState) {
        if !self.current(app) {
            return;
        }
        let Some(input) = app.input_rect else {
            self.close(app);
            return;
        };
        let body = crate::layout::input_editor_rect(input).intersection(frame.area());
        if body.width < 4 || body.height < 2 {
            self.close(app);
            return;
        }
        self.bounds = body;
        self.hits.clear();
        if matches!(self.selected, Some(Action::Policy(_))) && app.trust.mode != TrustMode::Eager {
            self.selected = Some(Action::Mode(app.trust.mode));
        }
        frame.render_widget(Clear, body);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(Color::Reset)),
            body,
        );
        let theme = crate::theme::theme();
        let pending = self.pending.is_some();
        let mut paint = |action, rect: Rect, label: &str, color: Color, current: bool| {
            if rect.is_empty() {
                return;
            }
            let label = crate::width::truncate(label, rect.width.saturating_sub(2) as usize);
            let focused = self.selected == Some(action);
            let hover = self.hovered == Some(action);
            let bg = if focused || hover {
                theme.work_hover_bg.into()
            } else {
                Color::Reset
            };
            let style = Style::default()
                .fg(if current || focused || hover {
                    color
                } else {
                    theme.subtle_fg.into()
                })
                .bg(bg);
            let style = if pending {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            };
            let caption = Span::styled(
                label,
                if current {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                },
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::raw(" "), caption, Span::raw(" ")]))
                    .style(style),
                rect,
            );
            self.hits.push((action, rect));
        };
        for (index, mode) in TrustMode::all().into_iter().enumerate() {
            let left = body.width * index as u16 / 4;
            let right = body.width * (index + 1) as u16 / 4;
            let display = app.trust.theme.display(mode);
            paint(
                Action::Mode(mode),
                Rect::new(body.x + left, body.y, right - left, 1),
                display.name,
                theme.permission_color(display.color),
                app.trust.mode == mode,
            );
        }
        let eager = app.trust.mode == TrustMode::Eager;
        if eager {
            let width = body.width.saturating_sub(2);
            for (index, policy) in POLICIES.into_iter().enumerate() {
                let left = width * index as u16 / 3;
                let right = width * (index + 1) as u16 / 3;
                let display = app.trust.theme.escalation_display(policy);
                paint(
                    Action::Policy(policy),
                    Rect::new(body.x + 2 + left, body.y + 1, right - left, 1),
                    display.name,
                    theme.permission_color(display.color),
                    app.trust.escalation == policy,
                );
            }
        }
        let detail_y = (body.y + if eager { 2 } else { 1 }).min(body.bottom() - 1);
        let more = Rect::new(body.right() - 3, detail_y, 3, 1);
        paint(Action::More, more, "⋯", theme.subtle_fg.into(), false);
        if eager {
            frame.render_widget(
                Paragraph::new("↳")
                    .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
                Rect::new(body.x, body.y + 1, 1, 1),
            );
        }
        let detail = if let Some(error) = &self.error {
            error.clone()
        } else if let Some(pending) = &self.pending {
            format!(
                "{} Saving · {}",
                crate::loading::glyph(app.animation_frame),
                pending.requested.theme.display(pending.requested.mode).name
            )
        } else {
            description(
                self.hovered
                    .or(self.selected)
                    .unwrap_or(Action::Mode(app.trust.mode)),
            )
            .into()
        };
        frame.render_widget(
            Paragraph::new(crate::width::truncate(
                &detail,
                more.x.saturating_sub(body.x + 1) as usize,
            ))
            .style(
                Style::default()
                    .fg(if self.error.is_some() {
                        theme.error.into()
                    } else {
                        theme.subtle_fg.into()
                    })
                    .bg(Color::Reset),
            ),
            Rect::new(body.x, detail_y, more.x.saturating_sub(body.x), 1),
        );
        for (action, rect) in &app.input_controls {
            if matches!(
                action,
                InputAction::Attach | InputAction::Trust | InputAction::Escalation
            ) {
                self.hits.push((Action::Close, *rect));
            }
        }
    }
}

fn description(action: Action) -> &'static str {
    match action {
        Action::Mode(TrustMode::Calm) => "Read-only: auto · Other actions: ask",
        Action::Mode(TrustMode::Steady) => "Low-risk: auto · Escalation: ask",
        Action::Mode(TrustMode::Eager) => "Routine tools: auto · Escalation: policy",
        Action::Mode(TrustMode::Reckless) => "Sandbox off · No approval required",
        Action::Policy(EscalationPolicy::Deny) => "Escalation: deny",
        Action::Policy(EscalationPolicy::Ask) => "Escalation: ask before running",
        Action::Policy(EscalationPolicy::Allow) => {
            "Escalation: auto-allow · Explicit denies still apply"
        }
        Action::More => "Tool tiers and resource-risk policies",
        Action::Close => "Return to draft · Esc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        UiState,
        control::{ViewAddress, ViewBinding},
        input::InputEditor,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn writes_retain_full_policy_and_require_the_original_receipt_even_after_close() {
        let mut app = AppState::new("console".into(), None);
        app.trust.mode = TrustMode::Eager;
        app.trust.escalation = EscalationPolicy::Allow;
        app.trust.tiers.eager.tier2 = Some(atman_runtime::trust::PolicyAction::Deny);
        let original = app.trust.clone();
        let flow = atman_runtime::flow_store::FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let binding = ViewBinding {
            view_id: uuid::Uuid::now_v7(),
            revision: 1,
            flow: ViewAddress::Instance(flow),
        };
        let (outbound, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let (sender, _replies) = ControlSender::for_flow(binding, outbound);
        let mut console = Permissions::default();
        console.open(&app, false);
        assert!(!console.activate(Action::Mode(TrustMode::Steady), &mut app, Some(&sender)));
        let request = requests.try_recv().unwrap();
        let TuiControl::UpdateTrust(config) = request.command else {
            panic!("wrong command")
        };
        assert_eq!(
            app.trust, original,
            "queue acceptance is not a committed setting"
        );
        assert_eq!(
            config,
            TrustConfig {
                mode: TrustMode::Steady,
                ..original.clone()
            }
        );
        console.activate(Action::Mode(TrustMode::Reckless), &mut app, Some(&sender));
        assert!(
            requests.try_recv().is_err(),
            "pending full-config saves must be serialized"
        );
        let mut foreign = request.ticket.clone();
        foreign.request_id = uuid::Uuid::now_v7();
        assert!(
            console
                .finish(Some(&foreign), &Ok(config.clone()))
                .is_none()
        );
        assert!(console.is_pending());
        console.close(&mut app);
        assert!(console.is_pending() && !console.animates());
        assert_eq!(
            console.finish(Some(&request.ticket), &Err("save failed".into())),
            Some(TrustMode::Eager)
        );
        assert!(!console.is_pending());
        assert_eq!(app.trust, original);
        console.open(&app, false);
        assert_eq!(console.error.as_deref(), Some("save failed"));
        console.activate(Action::Mode(TrustMode::Steady), &mut app, Some(&sender));
        let retry = requests.try_recv().unwrap();
        assert_eq!(
            console.finish(Some(&retry.ticket), &Ok(config)),
            Some(TrustMode::Eager)
        );
        assert!(console.error.is_none());
    }

    #[test]
    fn foreign_documents_and_inactive_escalation_cannot_use_old_actions() {
        let mut app = AppState::new("console".into(), None);
        let (sender, mut requests) = ControlSender::channel();
        let mut console = Permissions::default();
        console.open(&app, true);
        console.activate(
            Action::Policy(EscalationPolicy::Allow),
            &mut app,
            Some(&sender),
        );
        assert!(requests.try_recv().is_err());
        app.items = Default::default();
        console.key(&KeyAction::CursorRight, &mut app, Some(&sender));
        console.key(&KeyAction::Submit, &mut app, Some(&sender));
        assert!(!console.open && requests.try_recv().is_err());
    }

    #[test]
    fn console_preserves_the_composer_geometry_and_draft_in_all_themes() {
        for palette in crate::theme::Theme::test_modes() {
            palette.with(|| {
                for width in [40, 64, 100] {
                    for theme in [
                        atman_runtime::trust::Theme::Default,
                        atman_runtime::trust::Theme::Wuxia,
                        atman_runtime::trust::Theme::Animal,
                        atman_runtime::trust::Theme::Weather,
                        atman_runtime::trust::Theme::Drink,
                    ] {
                        for mode in TrustMode::all() {
                            let mut ui = UiState::new(AppState::new("console".into(), None));
                            ui.app.trust.theme = theme;
                            ui.app.trust.mode = mode;
                            let mut editor = InputEditor::default();
                            editor.insert_str("retained draft");
                            let revision = ui.app.items.revision_clock();
                            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                            terminal
                                .draw(|f| crate::render::render_frame(f, &mut ui, &editor))
                                .unwrap();
                            let before = (ui.app.input_rect, ui.app.input_controls.clone());
                            ui.wm.modals.input_console.open(&ui.app, true);
                            terminal
                                .draw(|f| crate::render::render_frame(f, &mut ui, &editor))
                                .unwrap();
                            assert_eq!(before, (ui.app.input_rect, ui.app.input_controls.clone()));
                            assert_eq!(editor.buf(), "retained draft");
                            assert_eq!(ui.app.items.revision_clock(), revision);
                            let console = &ui.wm.modals.input_console;
                            assert!(console.open);
                            assert_eq!(
                                console
                                    .hits
                                    .iter()
                                    .filter(|(a, _)| matches!(a, Action::Policy(_)))
                                    .count(),
                                if mode == TrustMode::Eager { 3 } else { 0 }
                            );
                            for (index, (_, hit)) in console.hits.iter().enumerate() {
                                assert_eq!(
                                    hit.intersection(terminal.backend().buffer().area),
                                    *hit
                                );
                                for (_, other) in &console.hits[index + 1..] {
                                    assert!(hit.intersection(*other).is_empty());
                                }
                            }
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn wm_owns_paste_and_enter_until_returning_to_the_original_editor() {
        let mut ui = UiState::new(AppState::new("console".into(), None));
        let mut editor = InputEditor::default();
        editor.insert_str("draft");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| crate::render::render_frame(f, &mut ui, &editor))
            .unwrap();
        ui.wm.modals.input_console.open(&ui.app, false);
        let (sender, mut requests) = ControlSender::channel();
        assert!(
            ui.wm
                .dispatch_paste("never insert", &mut ui.app, Some(&sender))
        );
        assert!(
            ui.wm
                .dispatch_key(&KeyAction::Submit, &mut ui.app, Some(&sender))
                .0
        );
        assert!(requests.try_recv().is_err());
        ui.wm
            .dispatch_key(&KeyAction::CursorRight, &mut ui.app, Some(&sender));
        ui.wm
            .dispatch_key(&KeyAction::Submit, &mut ui.app, Some(&sender));
        assert!(matches!(
            requests.try_recv(),
            Ok(TuiControl::UpdateTrust(_))
        ));
        ui.wm
            .dispatch_key(&KeyAction::Escape, &mut ui.app, Some(&sender));
        assert!(!ui.wm.modals.input_console.open);
        assert!(ui.wm.modals.retains_settings_operation());
        assert_eq!(editor.buf(), "draft");
        ui.wm.modals.input_console.open(&ui.app, false);
        ui.wm
            .dispatch_key(&KeyAction::Quit, &mut ui.app, Some(&sender));
        assert!(ui.app.should_quit && ui.wm.modals.input_console.is_pending());
    }
}
