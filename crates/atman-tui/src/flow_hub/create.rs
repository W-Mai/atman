//! A watcher draft retains its registration identity across failed host acknowledgements.

use atman_runtime::flow_store::{
    FlowRef,
    process_watch::{ProcessSource, ProcessWatch},
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchRegistration {
    Flow(FlowSubscription),
    Process(ProcessWatch),
}

#[derive(Clone, PartialEq, Eq)]
enum Source {
    Flow(FlowRef),
    Process(ProcessSource),
}

pub(super) struct Draft {
    pub owner: FlowRef,
    pub picking: bool,
    source: Option<(Source, String)>,
    event: usize,
    pattern: InputEditor,
    turn: InputEditor,
    expiry: InputEditor,
    once: bool,
    selected: usize,
    request: Option<WatchRegistration>,
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
        self.choose_source(Source::Flow(source), title);
    }

    pub fn choose_process(&mut self, source: ProcessSource, title: String) {
        self.choose_source(Source::Process(source), title);
    }

    fn choose_source(&mut self, source: Source, title: String) {
        if self.source.as_ref().map(|(flow, _)| flow) != Some(&source) {
            self.changed();
        }
        self.source = Some((source, title));
        self.picking = false;
        self.selected = if self.is_process() { 2 } else { 1 };
    }

    fn is_process(&self) -> bool {
        matches!(self.source, Some((Source::Process(_), _)))
    }

    pub fn source_directory(&self) -> HubRequest {
        if self.is_process() {
            return HubRequest::ProcessSources {
                owner: self.owner.clone(),
            };
        }
        HubRequest::Flows {
            session: self.source.as_ref().map_or(
                self.owner.session_id.0,
                |(source, _)| match source {
                    Source::Flow(flow) => flow.session_id.0,
                    Source::Process(_) => self.owner.session_id.0,
                },
            ),
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
            2 if self.event == 0 || self.is_process() => Some(&mut self.pattern),
            3 if self.event == 1 && !self.is_process() => Some(&mut self.turn),
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

    fn prepare(&mut self) -> Result<WatchRegistration, String> {
        if let Some(request) = &self.request {
            return Ok(request.clone());
        }
        let source = self
            .source
            .as_ref()
            .ok_or("Select an observation source")?
            .0
            .clone();
        let filter = match if self.is_process() { 0 } else { self.event } {
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
        let request = match source {
            Source::Flow(source) => WatchRegistration::Flow(FlowSubscription {
                id: Uuid::now_v7(),
                source,
                filter,
                once: self.once,
                purpose: SubscriptionPurpose::Watcher,
                expires_at,
                reply: None,
            }),
            Source::Process(source) => {
                if self.pattern.buf().is_empty() {
                    return Err("Enter text to match in future process output".into());
                }
                WatchRegistration::Process(ProcessWatch {
                    id: Uuid::now_v7(),
                    source,
                    pattern: self.pattern.buf().to_owned(),
                    once: self.once,
                    expires_at,
                })
            }
        };
        self.request = Some(request.clone());
        Ok(request)
    }

    fn submit(&mut self) -> Action {
        match self.prepare() {
            Ok(registration) => Action::RegisterWatcher {
                owner: self.owner.clone(),
                registration,
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
                    1 if !self.is_process() => {
                        self.event =
                            (self.event + if key == KeyAction::CursorLeft { 2 } else { 1 }) % 3;
                    }
                    4 => self.once = !self.once,
                    _ => return Action::None,
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
            Paragraph::new(if self.is_process() {
                "WatchHub / New process watcher"
            } else {
                "WatchHub / New Flow watcher"
            })
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
            .map(|(source, title)| match source {
                Source::Flow(source) => format!(
                    "{title} · {} · Session {}",
                    source.instance_id, source.session_id
                ),
                Source::Process(source) => format!("{title} · {}", source.handle()),
            })
            .unwrap_or_default();
        let process = match &self.source {
            Some((Source::Process(ProcessSource::Bash { incarnation, .. }), _)) => {
                Some(("Bash", incarnation.to_string()))
            }
            Some((Source::Process(ProcessSource::Terminal { incarnation, .. }), _)) => {
                Some(("Terminal", incarnation.to_string()))
            }
            _ => None,
        };
        for (index, rect) in layout.fields {
            let (label, value, placeholder, cursor, selector, locked) = match index {
                0 => (
                    if process.is_some() {
                        "Source process"
                    } else {
                        "Source Flow"
                    },
                    source.as_str(),
                    "Enter to select",
                    None,
                    true,
                    false,
                ),
                1 if process.is_some() => (
                    "Source kind",
                    process.as_ref().unwrap().0,
                    "",
                    None,
                    false,
                    true,
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
                    if process.is_some() {
                        "Required · future output only"
                    } else {
                        "Blank matches any final answer"
                    },
                    Some(self.pattern.cursor_display_col()),
                    false,
                    process.is_none() && self.event != 0,
                ),
                3 if process.is_some() => (
                    "Process incarnation",
                    process.as_ref().unwrap().1.as_str(),
                    "",
                    None,
                    false,
                    true,
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
            self.error.as_deref().or(error.filter(|_| self.request.is_some())).unwrap_or(if self.is_process() {
                "Observes future output from this process lifetime. Registration does not resume the recipient."
            } else {
                "Observes future events. Existing Watch permissions apply; registration does not resume either Flow."
            })
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
    fn process_picker_preserves_incarnation_retry_and_view_ownership() {
        use atman_runtime::{flow_runtime::ProcessWatchSource, task_registry::TaskStatus};
        let owner = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        for (width, height) in [(40, 24), (120, 45)] {
            for source in [
                ProcessSource::Bash {
                    incarnation: Uuid::now_v7(),
                    handle: "process_handle".into(),
                },
                ProcessSource::Terminal {
                    incarnation: Uuid::now_v7(),
                    handle: "process_handle".into(),
                },
            ] {
                let mut ticket = ControlTicket {
                    binding: ViewBinding {
                        view_id: Uuid::now_v7(),
                        revision: 1,
                        flow: owner.clone().into(),
                    },
                    request_id: Uuid::now_v7(),
                };
                let mut history = History::default();
                let mut hub = FlowHub {
                    create_watch: Some(Draft::new(owner.clone())),
                    ..Default::default()
                };
                assert!(
                    matches!(hub.key(KeyAction::CoursePrefill), Action::Load(HubRequest::ProcessSources { owner: selected }) if selected == owner)
                );
                let page = || {
                    Box::new(FlowHubPage::ProcessSources {
                        owner: owner.clone(),
                        sources: vec![ProcessWatchSource {
                            source: source.clone(),
                            label: "Watch target".into(),
                            status: TaskStatus::Running,
                        }],
                    })
                };
                hub.pending = Some(ticket.clone());
                history.receive(&mut hub, Some(&ticket), Ok(page()));
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| hub.render(frame, &owner.clone().into()))
                    .unwrap();
                let button = hub.graph_button;
                assert_eq!(button.height, 3);
                assert!(matches!(
                    hub.mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: button.x + 1,
                        row: button.y + 1,
                        modifiers: crossterm::event::KeyModifiers::NONE
                    }),
                    Action::Load(HubRequest::Flows { .. })
                ));
                // Switching directories does not change the recipient or select a process implicitly.
                assert!(hub.create_watch.as_ref().unwrap().source.is_none());
                hub.paste("target");
                let row = hub.hits[0].0;
                hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: row.x + 2,
                    row: row.y + 1,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                });
                assert!(!hub.create_watch.as_ref().unwrap().picking);
                assert!(matches!(hub.key(KeyAction::Save), Action::None));
                assert!(
                    hub.create_watch
                        .as_ref()
                        .unwrap()
                        .error
                        .as_ref()
                        .unwrap()
                        .contains("Enter text")
                );
                hub.paste("中🙂needle");
                hub.create_watch.as_mut().unwrap().selected = 5;
                hub.paste("3600");
                let Action::RegisterWatcher {
                    owner: recipient,
                    registration: WatchRegistration::Process(watch),
                } = hub.key(KeyAction::Save)
                else {
                    panic!("missing process registration")
                };
                assert_eq!(recipient, owner);
                assert_eq!(watch.source, source);
                assert_eq!(watch.pattern, "中🙂needle");
                assert!(watch.expires_at.is_some());
                ticket.request_id = Uuid::now_v7();
                hub.pending = Some(ticket.clone());
                hub.paste("ignored");
                assert!(matches!(hub.key(KeyAction::Save), Action::None));
                history.receive(
                    &mut hub,
                    Some(&ticket),
                    Err("acknowledgement unavailable".into()),
                );
                hub.key(KeyAction::CursorLeft);
                let draft = hub.create_watch.as_mut().unwrap();
                assert_eq!(
                    draft.prepare().unwrap(),
                    WatchRegistration::Process(watch.clone())
                );
                // A process never parses a leftover Flow-only turn filter or changes its kind.
                draft.event = 1;
                draft.turn.paste_single_line("not a turn UUID");
                draft.selected = 1;
                draft.key(KeyAction::CursorRight, false);
                assert_eq!(
                    draft.prepare().unwrap(),
                    WatchRegistration::Process(watch.clone())
                );
                draft.selected = 0;
                assert!(
                    matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::ProcessSources { owner: selected }) if selected == owner)
                );
                ticket.request_id = Uuid::now_v7();
                hub.pending = Some(ticket.clone());
                history.receive(&mut hub, Some(&ticket), Ok(page()));
                hub.key(KeyAction::Submit);
                assert_eq!(
                    hub.create_watch.as_mut().unwrap().prepare().unwrap(),
                    WatchRegistration::Process(watch.clone())
                );
                hub.key(KeyAction::Char('!'));
                let WatchRegistration::Process(edited) =
                    hub.create_watch.as_mut().unwrap().prepare().unwrap()
                else {
                    panic!("expected process")
                };
                assert_ne!(watch.id, edited.id);
                terminal
                    .draw(|frame| hub.render(frame, &owner.clone().into()))
                    .unwrap();
                let create = hub.create_watch.as_ref().unwrap().buttons[0];
                assert_eq!(create.height, 3);
                assert!(matches!(
                    hub.mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: create.x + 1,
                        row: create.y + 1,
                        modifiers: crossterm::event::KeyModifiers::NONE
                    }),
                    Action::RegisterWatcher {
                        registration: WatchRegistration::Process(_),
                        ..
                    }
                ));
                // Esc leaves accepted work alone; an old receipt cannot replace the new directory.
                hub.pending = Some(ticket.clone());
                assert!(
                    matches!(hub.key(KeyAction::Escape), Action::Load(HubRequest::Watchers { owner: selected }) if selected == owner)
                );
                let stale = ticket.clone();
                ticket.request_id = Uuid::now_v7();
                hub.pending = Some(ticket.clone());
                history.receive(&mut hub, Some(&stale), Ok(page()));
                assert_eq!(hub.pending.as_ref(), Some(&ticket));
            }
        }
    }

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
            Box::new(FlowHubPage::Watchers(WatchDirectory::flow(
                owner.clone(),
                atman_runtime::flow_runtime::catalog::WatchCatalogSnapshot {
                    title: "WatchHub".into(),
                    cursor: receipt.clone(),
                    trailing_bytes: 0,
                    records: vec![],
                },
            )))
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
                registration: WatchRegistration::Flow(subscription),
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
                registration: WatchRegistration::Flow(retry),
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
                WatchRegistration::Flow(subscription.clone())
            );
            hub.key(KeyAction::Tab);
            hub.key(KeyAction::Char('x'));
            let WatchRegistration::Flow(edited) =
                hub.create_watch.as_mut().unwrap().prepare().unwrap()
            else {
                panic!("expected Flow watcher")
            };
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
