//! A watcher draft retains its registration identity across failed host acknowledgements.

use atman_runtime::flow_store::{
    FlowRef,
    subscription::{FlowEventFilter, FlowSubscription, SubscriptionPurpose},
};
use chrono::{TimeDelta, Utc};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Wrap},
};
use uuid::Uuid;

use super::{Action, HubRequest};
use crate::{
    input::InputEditor,
    keys::KeyAction,
    settings_form::{self, Field, FieldHit, FormLayout},
};

const FIELDS: usize = 6;

pub(super) struct Draft {
    pub owner: FlowRef,
    pub picking: bool,
    source: Option<(FlowRef, String)>,
    event: usize,
    pattern: InputEditor,
    turn: InputEditor,
    expiry: InputEditor,
    once: bool,
    selected: usize,
    request: Option<FlowSubscription>,
    error: Option<String>,
    hits: Vec<(usize, FieldHit)>,
    buttons: Vec<Rect>,
    hovered: Option<usize>,
}

impl Draft {
    pub fn new(owner: FlowRef) -> Self {
        Self {
            owner,
            picking: true,
            source: None,
            event: 0,
            pattern: InputEditor::default(),
            turn: InputEditor::default(),
            expiry: InputEditor::default(),
            once: true,
            selected: 0,
            request: None,
            error: None,
            hits: Vec::new(),
            buttons: Vec::new(),
            hovered: None,
        }
    }

    pub fn choose(&mut self, source: FlowRef, title: String) {
        if self.source.as_ref().map(|(flow, _)| flow) != Some(&source) {
            self.changed();
        }
        self.source = Some((source, title));
        self.picking = false;
        self.selected = 1;
    }

    pub fn source_directory(&self) -> HubRequest {
        HubRequest::Flows {
            session: self
                .source
                .as_ref()
                .map_or(self.owner.session_id.0, |(source, _)| source.session_id.0),
            before: None,
        }
    }

    pub fn back_from_picker(&mut self) -> bool {
        if self.source.is_some() {
            self.picking = false;
            true
        } else {
            false
        }
    }

    fn changed(&mut self) {
        self.request = None;
        self.error = None;
    }

    fn editor(&mut self) -> Option<&mut InputEditor> {
        match self.selected {
            2 if self.event == 0 => Some(&mut self.pattern),
            3 if self.event == 1 => Some(&mut self.turn),
            5 => Some(&mut self.expiry),
            _ => None,
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(editor) = self.editor() {
            let previous = editor.buf().to_owned();
            editor.paste_single_line(text);
            if editor.buf() != previous {
                self.changed();
            }
        }
    }

    fn prepare(&mut self) -> Result<FlowSubscription, String> {
        if let Some(request) = &self.request {
            return Ok(request.clone());
        }
        let source = self
            .source
            .as_ref()
            .ok_or("Select a source Flow")?
            .0
            .clone();
        let filter = match self.event {
            0 if self.pattern.buf().is_empty() => FlowEventFilter::FinalAnswers,
            0 => FlowEventFilter::AnswerContains(self.pattern.buf().to_owned()),
            1 => FlowEventFilter::TurnCompleted {
                turn: if self.turn.buf().trim().is_empty() {
                    None
                } else {
                    Some(atman_rt::TurnId(
                        Uuid::parse_str(self.turn.buf().trim())
                            .map_err(|_| "Turn must be a UUID or blank")?
                            .into(),
                    ))
                },
            },
            _ => FlowEventFilter::InstanceClosed,
        };
        let expires_at = if self.expiry.buf().trim().is_empty() {
            None
        } else {
            let seconds: i64 = self
                .expiry
                .buf()
                .trim()
                .parse()
                .map_err(|_| "Expiry must be a positive number of seconds or blank")?;
            if seconds <= 0 {
                return Err("Expiry must be greater than zero".into());
            }
            Some(
                TimeDelta::try_seconds(seconds)
                    .and_then(|duration| Utc::now().checked_add_signed(duration))
                    .ok_or("Expiry is outside the supported date range")?,
            )
        };
        let request = FlowSubscription {
            id: Uuid::now_v7(),
            source,
            filter,
            once: self.once,
            purpose: SubscriptionPurpose::Watcher,
            expires_at,
            reply: None,
        };
        self.request = Some(request.clone());
        Ok(request)
    }

    fn submit(&mut self) -> Action {
        match self.prepare() {
            Ok(subscription) => Action::RegisterWatcher {
                owner: self.owner.clone(),
                subscription,
            },
            Err(error) => {
                self.error = Some(error);
                Action::None
            }
        }
    }

    pub fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            return Action::Load(HubRequest::Watchers {
                owner: self.owner.clone(),
            });
        }
        if pending {
            return Action::None;
        }
        match key {
            KeyAction::Save => return self.submit(),
            KeyAction::Tab | KeyAction::HistoryDown => {
                self.selected = (self.selected + 1) % (FIELDS + 2)
            }
            KeyAction::BackTab | KeyAction::HistoryUp => {
                self.selected = (self.selected + FIELDS + 1) % (FIELDS + 2)
            }
            KeyAction::Submit if self.selected == FIELDS => return self.submit(),
            KeyAction::Submit if self.selected == FIELDS + 1 => {
                return self.key(KeyAction::Escape, false);
            }
            KeyAction::Submit | KeyAction::CursorLeft | KeyAction::CursorRight
                if matches!(self.selected, 0 | 1 | 4) =>
            {
                match self.selected {
                    0 => {
                        self.picking = true;
                        return Action::Load(self.source_directory());
                    }
                    1 => {
                        self.event =
                            (self.event + if key == KeyAction::CursorLeft { 2 } else { 1 }) % 3;
                    }
                    4 => self.once = !self.once,
                    _ => unreachable!(),
                }
                self.changed();
            }
            KeyAction::Newline | KeyAction::Submit => {}
            _ => {
                if let Some(editor) = self.editor() {
                    let previous = editor.buf().to_owned();
                    editor.handle_key(&key);
                    if editor.buf() != previous {
                        self.changed();
                    }
                }
            }
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        let point = (event.column, event.row).into();
        self.hovered = self
            .hits
            .iter()
            .find(|(_, field)| field.record.contains(point))
            .map(|(index, _)| *index)
            .or_else(|| {
                self.buttons
                    .iter()
                    .position(|rect| rect.contains(point))
                    .map(|index| FIELDS + index)
            });
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return Action::None;
        }
        let Some(selected) = self.hovered else {
            return Action::None;
        };
        if selected == FIELDS + 1 {
            return self.key(KeyAction::Escape, pending);
        }
        if pending {
            return Action::None;
        }
        self.selected = selected;
        if selected >= FIELDS {
            return self.key(KeyAction::Submit, false);
        }
        if let Some((_, hit)) = self
            .hits
            .iter()
            .find(|(index, _)| *index == selected)
            .copied()
        {
            if let Some((left, right)) = hit.arrows {
                if left.contains(point) {
                    return self.key(KeyAction::CursorLeft, false);
                }
                if right.contains(point) || hit.value.contains(point) {
                    return self.key(KeyAction::CursorRight, false);
                }
            }
            if hit.value.contains(point)
                && let Some(editor) = self.editor()
            {
                editor.set_cursor_by_display(
                    0,
                    event
                        .column
                        .saturating_sub(hit.value.x)
                        .saturating_add(hit.offset as u16),
                );
            }
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, pending: bool, error: Option<&str>) {
        let t = crate::theme::theme();
        self.hits.clear();
        self.buttons.clear();
        if area.width < 40 || area.height < 16 {
            frame.render_widget(Paragraph::new("New watcher needs 40×16 · Esc back"), area);
            return;
        }
        let width = area.width.saturating_sub(4).min(100);
        let height = area
            .height
            .saturating_sub(2)
            .min(if width >= 70 { 32 } else { 38 });
        let content = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(
            Paragraph::new("WatchHub / New Flow watcher")
                .style(Style::default().fg(t.heading.into())),
            Rect::new(content.x + 1, content.y, width - 2, 1),
        );
        frame.render_widget(
            Paragraph::new(format!(
                "Recipient: {}",
                super::page::short_flow(&self.owner)
            ))
            .style(Style::default().fg(t.meta_fg.into())),
            Rect::new(content.x + 1, content.y + 1, width - 2, 1),
        );
        let layout = FormLayout::with_actions(
            Rect::new(content.x, content.y + 2, content.width, content.height - 2),
            FIELDS,
            self.selected,
            2,
        );
        let source = self
            .source
            .as_ref()
            .map(|(source, title)| {
                format!(
                    "{title} · {} · Session {}",
                    source.instance_id, source.session_id
                )
            })
            .unwrap_or_default();
        for (index, rect) in layout.fields {
            let (label, value, placeholder, cursor, selector, locked) = match index {
                0 => (
                    "Source Flow",
                    source.as_str(),
                    "Enter to select",
                    None,
                    true,
                    false,
                ),
                1 => (
                    "Event",
                    ["Final answer", "Turn completed", "Instance closed"][self.event],
                    "",
                    None,
                    true,
                    false,
                ),
                2 => (
                    "Contains text",
                    self.pattern.buf(),
                    "Blank matches any final answer",
                    Some(self.pattern.cursor_display_col()),
                    false,
                    self.event != 0,
                ),
                3 => (
                    "Turn UUID",
                    self.turn.buf(),
                    "Blank matches any completed turn",
                    Some(self.turn.cursor_display_col()),
                    false,
                    self.event != 1,
                ),
                4 => (
                    "Delivery",
                    if self.once { "Once" } else { "Persistent" },
                    "",
                    None,
                    true,
                    false,
                ),
                _ => (
                    "Expiry (seconds)",
                    self.expiry.buf(),
                    "No deadline",
                    Some(self.expiry.cursor_display_col()),
                    false,
                    false,
                ),
            };
            let hit = settings_form::render_field(
                frame,
                rect,
                &Field {
                    label,
                    value,
                    placeholder,
                    focused: index == self.selected && !pending,
                    hovered: self.hovered == Some(index) && !pending,
                    cursor,
                    selector,
                    locked: locked || pending,
                },
                &t,
            );
            self.hits.push((index, hit));
        }
        let feedback = if pending {
            "Waiting for host acknowledgement"
        } else {
            self.error.as_deref().or(error.filter(|_| self.request.is_some())).unwrap_or("Observes future events. Existing Watch permissions apply; registration does not resume either Flow.")
        };
        frame.render_widget(
            Paragraph::new(feedback)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(t.meta_fg.into())),
            layout.info,
        );
        for (index, rect) in layout.actions.iter().copied().enumerate() {
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label: if index == 0 { "Create" } else { "Back" },
                    shortcut: if index == 0 { "Ctrl+S" } else { "Esc" },
                    focused: self.selected == FIELDS + index,
                    hovered: self.hovered == Some(FIELDS + index),
                    disabled: pending && index == 0,
                },
                &t,
            );
        }
        self.buttons = layout.actions;
        frame.render_widget(
            Paragraph::new("Tab field · ←→ value · Ctrl+S create · Esc back")
                .style(Style::default().fg(t.meta_fg.into())),
            layout.help,
        );
        if layout.scroll.height > 0 {
            frame.render_widget(
                Paragraph::new("↑↓ fields · Tab actions")
                    .style(Style::default().fg(t.meta_fg.into())),
                layout.scroll,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, History, WatchDirectory},
    };
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_runtime::access::{InstancePage, InstanceSummary},
        flow_store::FlowInstanceId,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn watcher_creation_keeps_draft_request_and_original_reply_ownership() {
        let owner = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: owner.instance_id.clone(),
        };
        let receipt = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 1,
            digest: "test".into(),
        };
        let watchers = || {
            Box::new(FlowHubPage::Watchers(WatchDirectory {
                owner: owner.clone(),
                title: "WatchHub".into(),
                cursor: receipt.clone(),
                trailing_bytes: 0,
                records: vec![],
            }))
        };
        let flows = || {
            Box::new(FlowHubPage::Flows {
                session: source.session_id.0,
                title: "Source session".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        address: source.clone(),
                        entry: Some("Source".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                    }],
                    next: None,
                    total: 1,
                    catalog: receipt.clone(),
                },
                relations: Ok(vec![]),
            })
        };
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: owner.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        for (width, height) in [(40, 24), (120, 45)] {
            let mut history = History::default();
            let mut saved = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            saved.receive(Some(&ticket), Ok(flows()));
            saved.query = "Independent browsing".into();
            history.save(saved);
            let mut hub = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            history.receive(&mut hub, Some(&ticket), Ok(watchers()));
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            assert_eq!(hub.graph_button.height, 3);
            assert!(matches!(
                hub.key(KeyAction::CoursePrefill),
                Action::Load(HubRequest::Flows { .. })
            ));
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            history.receive(&mut hub, Some(&ticket), Ok(flows()));
            assert!(hub.query.is_empty());
            hub.key(KeyAction::Submit);
            assert!(!hub.create_watch.as_ref().unwrap().picking);
            hub.key(KeyAction::Tab); // Contains text.
            hub.paste("中🙂text");
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            let hit = hub
                .create_watch
                .as_ref()
                .unwrap()
                .hits
                .iter()
                .find(|(index, _)| *index == 2)
                .unwrap()
                .1;
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.value.x + 2,
                row: hit.value.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            hub.key(KeyAction::Char('!'));
            assert_eq!(
                hub.create_watch.as_ref().unwrap().pattern.buf(),
                "中!🙂text"
            );
            hub.create_watch.as_mut().unwrap().selected = 5;
            hub.paste("3600");
            let Action::RegisterWatcher {
                owner: recipient,
                subscription,
            } = hub.key(KeyAction::Save)
            else {
                panic!("missing registration")
            };
            assert_eq!(recipient, owner);
            assert_eq!(subscription.source, source);
            assert_eq!(subscription.purpose, SubscriptionPurpose::Watcher);
            assert_eq!(
                subscription.filter,
                FlowEventFilter::AnswerContains("中!🙂text".into())
            );
            assert!(subscription.expires_at.is_some());
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            assert!(matches!(hub.key(KeyAction::Save), Action::None));
            hub.paste("ignored");
            history.receive(&mut hub, Some(&ticket), Err("lost acknowledgement".into()));
            hub.key(KeyAction::CursorLeft); // Moving the cursor does not change identity.
            let Action::RegisterWatcher {
                subscription: retry,
                ..
            } = hub.key(KeyAction::Save)
            else {
                panic!("missing retry")
            };
            assert_eq!(subscription, retry);
            hub.create_watch.as_mut().unwrap().selected = 0;
            assert!(matches!(hub.key(KeyAction::Submit), Action::Load(_)));
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            history.receive(&mut hub, Some(&ticket), Ok(flows()));
            hub.key(KeyAction::Submit); // Same source retains the request.
            assert_eq!(
                hub.create_watch.as_mut().unwrap().prepare().unwrap(),
                subscription
            );
            hub.key(KeyAction::Tab);
            hub.key(KeyAction::Char('x'));
            let edited = hub.create_watch.as_mut().unwrap().prepare().unwrap();
            assert_ne!(edited.id, subscription.id);
            hub.create_watch.as_mut().unwrap().selected = FIELDS;
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            let create = hub.create_watch.as_ref().unwrap().buttons[0];
            assert_eq!(create.height, 3);
            assert!(matches!(
                hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: create.x + 1,
                    row: create.y,
                    modifiers: crossterm::event::KeyModifiers::NONE
                }),
                Action::RegisterWatcher { .. }
            ));
            let stale = ticket.clone();
            ticket.request_id = Uuid::now_v7();
            hub.pending = Some(ticket.clone());
            history.receive(&mut hub, Some(&stale), Ok(watchers()));
            assert!(hub.create_watch.is_some());
            history.receive(&mut hub, Some(&ticket), Ok(watchers()));
            assert!(hub.create_watch.is_none());
            assert!(hub.pending.is_none());
            assert!(
                history
                    .pages
                    .iter()
                    .any(|saved| saved.query == "Independent browsing")
            );
        }
    }
}
