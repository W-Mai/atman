//! Watcher directory details use the same flow addresses and navigation actions.

use atman_runtime::{
    event_log::journal::CommitReceipt,
    flow_store::{
        FlowRef,
        watch_view::{WatchRecord, WatchRegistration},
    },
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};
use uuid::Uuid;

use super::{
    Action,
    page::{Row, RowKey},
};
use crate::keys::KeyAction;

pub struct WatchDirectory {
    pub owner: FlowRef,
    pub title: String,
    pub cursor: CommitReceipt,
    pub trailing_bytes: u64,
    pub records: Vec<WatchRecord>,
}

impl WatchDirectory {
    pub(super) fn rows(&self) -> Vec<Row> {
        self.records.iter().map(|record| {
            let (kind, status, once, condition) = summary(record);
            let p = &record.progress;
            Row {
                key: RowKey::Watcher { owner: self.owner.clone(), id: record.registration.id() },
                title: format!("{kind} · {status} · {}", if once { "once" } else { "persistent" }),
                detail: format!("{condition} · {} matched · {} pending · {} input committed · {} cancelled · {}", p.matched, p.pending, p.input_committed, p.cancelled, record.registration.id()),
                action: Action::None,
            }
        }).collect()
    }
}

fn summary(record: &WatchRecord) -> (&'static str, String, bool, String) {
    use atman_runtime::flow_store::{
        process_watch::ProcessWatchStatus,
        subscription::{FlowEventFilter, SubscriptionStatus},
    };
    match &record.registration {
        WatchRegistration::Flow(record) => (
            "Flow",
            match record.status {
                SubscriptionStatus::Active => "Active",
                SubscriptionStatus::Matched => "Matched",
                SubscriptionStatus::Cancelled => "Cancelled",
                SubscriptionStatus::Expired => "Expired",
                SubscriptionStatus::SourceClosed => "Source closed",
            }
            .into(),
            record.subscription.once,
            match &record.subscription.filter {
                FlowEventFilter::FinalAnswers => "Final answers".into(),
                FlowEventFilter::AnswerContains(pattern) => format!("Answer contains: {pattern}"),
                FlowEventFilter::AnswersExceptTurn(turn) => {
                    format!("Final answers except turn {turn}")
                }
                FlowEventFilter::TurnCompleted { turn } => turn.as_ref().map_or_else(
                    || "Turn completed".into(),
                    |turn| format!("Turn completed: {turn}"),
                ),
                FlowEventFilter::InstanceClosed => "Flow closed".into(),
            },
        ),
        WatchRegistration::Process(record) => (
            "Process",
            match record.status {
                ProcessWatchStatus::Active => "Active",
                ProcessWatchStatus::Matched => "Matched",
                ProcessWatchStatus::Cancelled => "Cancelled",
                ProcessWatchStatus::Exited => "Exited",
                ProcessWatchStatus::SourceLost => "Source lost",
                ProcessWatchStatus::HistoryGap => "History gap",
                ProcessWatchStatus::Expired => "Expired",
            }
            .into(),
            record.watch.once,
            record.watch.pattern.clone(),
        ),
    }
}

#[derive(Default)]
pub(super) struct Details {
    pub id: Option<Uuid>,
    scroll: usize,
    selected: usize,
    targets: Vec<FlowRef>,
    hits: Vec<(Rect, usize)>,
    back: Rect,
    back_hovered: bool,
    hovered: Option<usize>,
}

impl Details {
    pub fn open(&mut self, id: Uuid) {
        *self = Self {
            id: Some(id),
            ..Self::default()
        };
    }

    pub fn key(&mut self, key: KeyAction) -> Action {
        match key {
            KeyAction::Escape => self.id = None,
            KeyAction::HistoryDown => self.scroll = self.scroll.saturating_add(1),
            KeyAction::HistoryUp => self.scroll = self.scroll.saturating_sub(1),
            KeyAction::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyAction::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyAction::Tab | KeyAction::BackTab => {
                self.selected = (self.selected + 1) % self.targets.len().max(1)
            }
            KeyAction::Submit | KeyAction::ToggleLastTool => {
                if let Some(target) = self.targets.get(self.selected) {
                    return if key == KeyAction::Submit {
                        Action::Focus(target.clone().into())
                    } else {
                        Action::Float(target.clone().into())
                    };
                }
            }
            _ => {}
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Action {
        let point = (event.column, event.row).into();
        self.back_hovered = self.back.contains(point);
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.back.contains(point) => {
                self.key(KeyAction::Escape)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.hovered {
                    self.selected = index;
                    self.key(KeyAction::Submit)
                } else {
                    Action::None
                }
            }
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp),
            _ => Action::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, directory: &WatchDirectory) {
        self.targets.clear();
        self.hits.clear();
        self.back = Rect::default();
        let Some(record) = directory
            .records
            .iter()
            .find(|record| Some(record.registration.id()) == self.id)
        else {
            self.id = None;
            return;
        };
        if area.width < 40 || area.height < 16 {
            frame.render_widget(Paragraph::new("WatchHub needs 40×16 · Esc back"), area);
            return;
        }
        let t = crate::theme::theme();
        let width = area.width.saturating_sub(4).min(100);
        let left = area.x + (area.width - width) / 2;
        let top = area.y + 1;
        let bottom = area.bottom() - 1;
        let fg = Style::default().fg(t.heading.into());
        let muted = Style::default().fg(t.meta_fg.into());
        frame.render_widget(
            Paragraph::new("WatchHub / Details").style(fg),
            Rect::new(left, top, width - 12, 1),
        );
        self.back = Rect::new(left + width - 10, top, 10, 3);
        frame.render_widget(
            Paragraph::new("\nBack Esc")
                .alignment(Alignment::Center)
                .style(muted.bg(if self.back_hovered {
                    t.work_bg.lerp(t.work_hover_bg, 0.5)
                } else {
                    t.work_bg.into_inner()
                })),
            self.back,
        );
        self.targets.push(directory.owner.clone());
        let (kind, status, once, condition) = summary(record);
        let mut text = vec![
            format!(
                "{kind} · {status} · {}",
                if once { "once" } else { "persistent" }
            ),
            String::new(),
            format!("Subscription: {}", record.registration.id()),
            format!(
                "Recipient: {}/{}",
                directory.owner.session_id, directory.owner.instance_id
            ),
            format!("Condition: {condition}"),
        ];
        let (registered, expiry, position) = match &record.registration {
            WatchRegistration::Flow(record) => {
                let source = &record.subscription.source;
                text.push(format!(
                    "Source: {}/{}",
                    source.session_id, source.instance_id
                ));
                self.targets.push(source.clone());
                (
                    record.registered_at,
                    record.subscription.expires_at,
                    format!("Source cursor: {}", record.cursor),
                )
            }
            WatchRegistration::Process(record) => {
                use atman_runtime::flow_store::process_watch::ProcessSource;
                let (kind, handle, incarnation) = match &record.watch.source {
                    ProcessSource::Bash {
                        handle,
                        incarnation,
                    } => ("Bash", handle, incarnation),
                    ProcessSource::Terminal {
                        handle,
                        incarnation,
                    } => ("Terminal", handle, incarnation),
                };
                text.push(format!("Source: {kind} / {handle}"));
                text.push(format!("Process incarnation: {incarnation}"));
                (
                    record.registered_at,
                    record.watch.expires_at,
                    format!("Registered after source position: {}", record.started_after),
                )
            }
        };
        let local = |value: chrono::DateTime<chrono::Utc>| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        };
        text.extend([
            format!("Registered: {}", local(registered)),
            format!(
                "Expires: {}",
                expiry.map_or_else(|| "No deadline".into(), local)
            ),
            position,
            String::new(),
        ]);
        let p = &record.progress;
        text.extend([
            format!("Matches: {}", p.matched),
            format!("Pending: {}", p.pending),
            format!("Input committed: {}", p.input_committed),
            format!("Cancelled: {}", p.cancelled),
            "Input commitment does not mean processing completed.".into(),
            String::new(),
        ]);
        if let Some(last) = &p.last_delivery {
            text.push(format!(
                "Last notification: {} · {}",
                last.event,
                local(last.accepted_at)
            ));
            text.push(format!("Message: {}", last.message_id));
        }
        text.push(format!(
            "Recorded snapshot · journal {} · commit {} · trailing bytes {}",
            directory.cursor.journal_id, directory.cursor.sequence, directory.trailing_bytes
        ));
        text.push("Reading does not start execution or consume notifications.".into());
        let lines: Vec<_> = text
            .iter()
            .flat_map(|line| crate::width::word_wrap(line, width.saturating_sub(4) as usize))
            .collect();
        let body = Rect::new(left, top + 4, width, bottom.saturating_sub(top + 9));
        let inner = Rect::new(
            body.x + 2,
            body.y + 1,
            width - 4,
            body.height.saturating_sub(2),
        );
        self.scroll = self
            .scroll
            .min(lines.len().saturating_sub(inner.height as usize));
        frame.render_widget(Paragraph::new("").style(muted.bg(t.work_bg.into())), body);
        frame.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .skip(self.scroll)
                    .take(inner.height as usize)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .style(
                Style::default()
                    .fg(t.work_title_fg.into())
                    .bg(t.work_bg.into()),
            ),
            inner,
        );
        let count = self.targets.len();
        self.selected = self.selected.min(count - 1);
        let button_w = (width - (count - 1) as u16 * 2) / count as u16;
        for index in 0..count {
            let rect = Rect::new(
                left + index as u16 * (button_w + 2),
                bottom - 4,
                button_w,
                3,
            );
            let selected = index == self.selected;
            let bg = if selected {
                t.work_bg.lerp(t.accent, 0.15)
            } else if self.hovered == Some(index) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            let label = if index == 0 { "Recipient" } else { "Source" };
            frame.render_widget(
                Paragraph::new(format!("\n{label}  Enter"))
                    .alignment(Alignment::Center)
                    .style(fg.bg(bg)),
                rect,
            );
            if selected {
                frame.render_widget(
                    Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, 3),
                );
            }
            self.hits.push((rect, index));
        }
        frame.render_widget(
            Paragraph::new(if width < 60 {
                "↑↓ scroll · Tab target · Enter · ^O"
            } else {
                "↑↓ scroll · Tab target · Enter open · Ctrl+O window"
            })
            .style(muted),
            Rect::new(left, bottom, width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, HubRequest},
    };
    use atman_runtime::flow_store::{
        FlowInstanceId,
        subscription::{
            FlowEventFilter, FlowSubscription, SubscriptionPurpose, SubscriptionRecord,
            SubscriptionStatus,
        },
        watch_view::WatchProgress,
    };
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn watcher_directory_reuses_identity_navigation_and_scrolls_details_without_consuming() {
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
            sequence: 9,
            digest: "test".into(),
        };
        let records: Vec<_> = (0..65)
            .map(|index| WatchRecord {
                registration: WatchRegistration::Flow(SubscriptionRecord {
                    subscription: FlowSubscription {
                        id: Uuid::from_u128(index + 1),
                        source: source.clone(),
                        filter: FlowEventFilter::AnswerContains(format!(
                            "ready-{index} {}",
                            "long condition ".repeat(200)
                        )),
                        once: false,
                        purpose: SubscriptionPurpose::Watcher,
                        expires_at: None,
                        reply: None,
                    },
                    registered_at: chrono::Utc::now(),
                    started_after: receipt.clone(),
                    cursor: 8,
                    status: SubscriptionStatus::Active,
                }),
                progress: WatchProgress {
                    matched: 3,
                    pending: 1,
                    input_committed: 2,
                    ..Default::default()
                },
            })
            .collect();
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: owner.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Watchers(WatchDirectory {
                owner: owner.clone(),
                title: "WatchHub".into(),
                cursor: receipt.clone(),
                trailing_bytes: 0,
                records: records.clone(),
            }))
        };
        for (width, height) in [(40, 24), (120, 45)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut hub = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            hub.receive(Some(&ticket), Ok(page()));
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            assert!(!hub.hits.is_empty());
            hub.key(KeyAction::PageDown);
            assert!(
                hub.selected > 0,
                "record pages must scroll without an external cursor"
            );
            hub.key(KeyAction::Char('6'));
            let selected_id = match hub.rows[hub.filtered()[hub.selected]].key {
                RowKey::Watcher { id, .. } => id,
                _ => panic!(),
            };
            hub.key(KeyAction::Submit);
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            assert_eq!(hub.watch_details.id, Some(selected_id));
            assert!(
                matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&owner))
            );
            hub.key(KeyAction::Tab);
            assert!(
                matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&source))
            );
            for (rect, index) in hub.watch_details.hits.clone() {
                for row in rect.y..rect.bottom() {
                    let action = hub.mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        row,
                        column: rect.x + 1,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    });
                    let expected = if index == 0 { &owner } else { &source };
                    assert!(
                        matches!(action, Action::Focus(address) if address.instance() == Some(expected))
                    );
                }
            }
            hub.key(KeyAction::PageDown);
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            assert!(hub.watch_details.scroll > 0);
            hub.key(KeyAction::Escape);
            assert!(hub.watch_details.id.is_none());
            assert_eq!(hub.query, "6");
            assert!(
                matches!(hub.key(KeyAction::RedirectPrefill), Action::Load(HubRequest::Watchers { owner: target }) if target == owner)
            );
            hub.pending = Some(ticket.clone());
            hub.receive(Some(&ticket), Err("read failed".into()));
            assert_eq!(hub.rows.len(), 65);
            assert_eq!(hub.query, "6");
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            let hit = hub.hits[0].0;
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                row: hit.y,
                column: hit.x + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            assert_eq!(hub.watch_details.id, Some(selected_id));
            let FlowHubPage::Watchers(directory) = hub.page.as_ref().unwrap() else {
                panic!()
            };
            assert!(
                directory
                    .records
                    .iter()
                    .all(|record| record.progress.pending == 1)
            );
        }
    }
}
