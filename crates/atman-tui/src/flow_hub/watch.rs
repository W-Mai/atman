//! Watcher directory details use the same flow addresses and navigation actions.

use atman_runtime::{
    flow_runtime::catalog::{
        WatchCatalogCursor, WatchCatalogEntry, WatchCatalogPage, WatchCatalogSnapshot,
    },
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
    scope: Scope,
}

enum Scope {
    Flow(WatchCatalogSnapshot),
    Catalog {
        after: Option<WatchCatalogCursor>,
        page: WatchCatalogPage,
    },
}

impl WatchDirectory {
    pub fn flow(owner: FlowRef, snapshot: WatchCatalogSnapshot) -> Self {
        Self {
            owner,
            title: format!("WatchHub / {}", snapshot.title),
            scope: Scope::Flow(snapshot),
        }
    }

    pub fn catalog(
        owner: FlowRef,
        after: Option<WatchCatalogCursor>,
        page: WatchCatalogPage,
    ) -> Self {
        Self {
            owner,
            title: "WatchHub / All sessions".into(),
            scope: Scope::Catalog { after, page },
        }
    }

    pub(super) fn global(&self) -> bool {
        matches!(self.scope, Scope::Catalog { .. })
    }

    pub(super) fn first(&self) -> super::HubRequest {
        if self.global() {
            super::HubRequest::WatchCatalog {
                owner: self.owner.clone(),
                after: None,
            }
        } else {
            super::HubRequest::Watchers {
                owner: self.owner.clone(),
            }
        }
    }

    pub(super) fn next(&self) -> Option<super::HubRequest> {
        match &self.scope {
            Scope::Catalog { page, .. } => {
                page.next
                    .clone()
                    .map(|after| super::HubRequest::WatchCatalog {
                        owner: self.owner.clone(),
                        after: Some(after),
                    })
            }
            Scope::Flow(_) => None,
        }
    }

    pub(super) fn append(&mut self, previous: Option<&mut Self>) -> Result<(), String> {
        let Scope::Catalog {
            after: Some(after),
            page,
        } = &mut self.scope
        else {
            return Ok(());
        };
        let Some(Self {
            owner,
            scope: Scope::Catalog { page: old, .. },
            ..
        }) = previous
        else {
            return Err("Watcher catalog changed; refresh to restart".into());
        };
        if owner != &self.owner || old.next.as_ref() != Some(after) {
            return Err("Watcher catalog cursor changed; refresh to restart".into());
        }
        let batch = std::mem::replace(&mut page.entries, std::mem::take(&mut old.entries));
        page.entries.extend(batch);
        page.inspected += old.inspected;
        Ok(())
    }

    pub fn snapshot(&self, owner: &FlowRef) -> Option<&WatchCatalogSnapshot> {
        match &self.scope {
            Scope::Flow(snapshot) => (owner == &self.owner).then_some(snapshot),
            Scope::Catalog { page, .. } => page.entries.iter().find_map(|entry| match entry {
                WatchCatalogEntry::Flow {
                    owner: actual,
                    snapshot: Ok(snapshot),
                    ..
                } if actual == owner => Some(snapshot),
                _ => None,
            }),
        }
    }

    pub(super) fn refresh_owner(&mut self, incoming: &mut Self) -> Result<(), String> {
        let Scope::Catalog { page, .. } = &mut self.scope else {
            return Err("Watcher catalog is not open".into());
        };
        let Scope::Flow(fresh) = &mut incoming.scope else {
            return Err("Expected an owning Flow snapshot".into());
        };
        let entry = page
            .entries
            .iter_mut()
            .find_map(|entry| match entry {
                WatchCatalogEntry::Flow {
                    owner, snapshot, ..
                } if owner == &incoming.owner => Some(snapshot),
                _ => None,
            })
            .ok_or_else(|| "Watcher owner is no longer in this catalog".to_string())?;
        let Ok(old) = entry else {
            return Err("Watcher owner snapshot is unavailable; refresh to restart".into());
        };
        std::mem::swap(old, fresh);
        Ok(())
    }

    pub(super) fn total(&self) -> usize {
        match &self.scope {
            Scope::Flow(snapshot) => snapshot.records.len(),
            Scope::Catalog { page, .. } => page
                .entries
                .iter()
                .map(|entry| match entry {
                    WatchCatalogEntry::Flow {
                        snapshot: Ok(snapshot),
                        ..
                    } => snapshot.records.len(),
                    _ => 0,
                })
                .sum(),
        }
    }

    pub(super) fn status(&self, matches: usize, compact: bool) -> String {
        match &self.scope {
            Scope::Flow(_) if compact => format!("{} subs · {matches} matches", self.total()),
            Scope::Flow(_) => format!(
                "Recorded snapshot · {} subscriptions · {matches} matches",
                self.total()
            ),
            Scope::Catalog { page, .. } => {
                let errors = page
                    .entries
                    .iter()
                    .filter(|entry| {
                        !matches!(
                            entry,
                            WatchCatalogEntry::Flow {
                                snapshot: Ok(_),
                                ..
                            }
                        )
                    })
                    .count();
                let remaining = if page.next.is_some() {
                    "More PgDn"
                } else {
                    "End"
                };
                let summary = format!("{remaining} · {} subs · {errors} errors", self.total());
                if compact {
                    summary
                } else {
                    format!(
                        "{summary} · {matches} matches · {} checked / {} sessions",
                        page.inspected, page.total_sessions
                    )
                }
            }
        }
    }

    pub(super) fn rows(&self) -> Vec<Row> {
        match &self.scope {
            Scope::Flow(snapshot) => rows(&self.owner, snapshot, None),
            Scope::Catalog { page, .. } => page
                .entries
                .iter()
                .flat_map(|entry| match entry {
                    WatchCatalogEntry::Flow {
                        owner,
                        session_title,
                        snapshot: Ok(snapshot),
                    } => rows(
                        owner,
                        snapshot,
                        Some(session_title.as_deref().unwrap_or("Untitled session")),
                    ),
                    WatchCatalogEntry::Flow {
                        owner,
                        snapshot: Err(error),
                        ..
                    } => vec![Row {
                        key: RowKey::WatchFailure {
                            session: owner.session_id.0,
                            flow: Some(owner.instance_id.clone()),
                        },
                        title: "Unavailable Flow".into(),
                        detail: format!("{error} · {}/{}", owner.session_id, owner.instance_id),
                        action: Action::None,
                    }],
                    WatchCatalogEntry::SessionUnavailable { session_id, error } => vec![Row {
                        key: RowKey::WatchFailure {
                            session: *session_id,
                            flow: None,
                        },
                        title: "Unavailable session".into(),
                        detail: format!("{error} · {session_id}"),
                        action: Action::None,
                    }],
                })
                .collect(),
        }
    }
}

fn rows(owner: &FlowRef, snapshot: &WatchCatalogSnapshot, session_title: Option<&str>) -> Vec<Row> {
    snapshot.records.iter().map(|record| {
            let (kind, status, once, condition) = summary(record);
            let p = &record.progress;
            let source = match &record.registration {
                WatchRegistration::Flow(record) => format!("{}/{}", record.subscription.source.session_id, record.subscription.source.instance_id),
                WatchRegistration::Process(record) => record.watch.source.handle().into(),
            };
            Row {
                key: RowKey::Watcher { owner: owner.clone(), id: record.registration.id() },
                title: format!("{}{kind} · {status} · {}", session_title.map(|title| format!("{title} / {} · ", snapshot.title)).unwrap_or_default(), if once { "once" } else { "persistent" }),
                detail: format!("{condition} · {} matched · {} pending · {} input committed · {} cancelled · from {source} · to {}/{} · {}", p.matched, p.pending, p.input_committed, p.cancelled, owner.session_id, owner.instance_id, record.registration.id()),
                action: Action::None,
            }
        }).collect()
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
    pub owner: Option<FlowRef>,
    scroll: usize,
    selected: usize,
    targets: Vec<(&'static str, Action)>,
    hits: Vec<(Rect, usize)>,
    back: Rect,
    back_hovered: bool,
    hovered: Option<usize>,
}

impl Details {
    pub fn feedback(&mut self) {
        self.scroll = 0;
    }
    pub fn open(&mut self, owner: FlowRef, id: Uuid) {
        *self = Self {
            id: Some(id),
            owner: Some(owner),
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
            KeyAction::Tab => self.selected = (self.selected + 1) % self.targets.len().max(1),
            KeyAction::BackTab => {
                let count = self.targets.len().max(1);
                self.selected = (self.selected + count - 1) % count;
            }
            KeyAction::Delete => {
                if let Some((_, action)) = self
                    .targets
                    .iter()
                    .find(|(_, action)| matches!(action, Action::CancelWatcher { .. }))
                {
                    return action.clone();
                }
            }
            KeyAction::Char('e') => {
                if let Some((_, action)) = self
                    .targets
                    .iter()
                    .find(|(_, action)| matches!(action, Action::EditWatcher { .. }))
                {
                    return action.clone();
                }
            }
            KeyAction::Submit | KeyAction::ToggleLastTool => {
                if let Some((_, action)) = self.targets.get(self.selected) {
                    return if key == KeyAction::Submit {
                        action.clone()
                    } else {
                        match action {
                            Action::Focus(target) => Action::Float(target.clone()),
                            _ => Action::None,
                        }
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

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        directory: &WatchDirectory,
        pending: bool,
        error: Option<&str>,
    ) {
        self.targets.clear();
        self.hits.clear();
        self.back = Rect::default();
        let Some(owner) = &self.owner else {
            self.id = None;
            return;
        };
        let Some(snapshot) = directory.snapshot(owner) else {
            self.id = None;
            return;
        };
        let Some(record) = snapshot
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
        self.targets
            .push(("Recipient", Action::Focus(owner.clone().into())));
        let (kind, status, once, condition) = summary(record);
        let mut text = vec![
            format!(
                "{kind} · {status} · {}",
                if once { "once" } else { "persistent" }
            ),
            String::new(),
            format!("Subscription: {}", record.registration.id()),
            format!("Recipient: {}/{}", owner.session_id, owner.instance_id),
            format!("Condition: {condition}"),
        ];
        let (registered, expiry, position) = match &record.registration {
            WatchRegistration::Flow(record) => {
                let source = &record.subscription.source;
                text.push(format!(
                    "Source: {}/{}",
                    source.session_id, source.instance_id
                ));
                self.targets
                    .push(("Source", Action::Focus(source.clone().into())));
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
        let active = match &record.registration {
            WatchRegistration::Flow(record) => {
                record.status == atman_runtime::flow_store::subscription::SubscriptionStatus::Active
            }
            WatchRegistration::Process(record) => {
                record.status
                    == atman_runtime::flow_store::process_watch::ProcessWatchStatus::Active
            }
        };
        if active {
            self.targets.push((
                "Edit E",
                if pending {
                    Action::None
                } else {
                    Action::EditWatcher {
                        owner: owner.clone(),
                        expected: (&record.registration).into(),
                    }
                },
            ));
            self.targets.push((
                "Cancel Del",
                if pending {
                    Action::None
                } else {
                    Action::CancelWatcher {
                        owner: owner.clone(),
                        id: record.registration.id(),
                    }
                },
            ));
        }
        if let Some(error) = error {
            text.insert(0, format!("Failed: {error}"));
        } else if pending {
            text.insert(0, "Waiting for host acknowledgement…".into());
        }
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
            snapshot.cursor.journal_id, snapshot.cursor.sequence, snapshot.trailing_bytes
        ));
        text.push("Reading does not start execution or consume notifications.".into());
        let lines: Vec<_> = text
            .iter()
            .flat_map(|line| crate::width::word_wrap(line, width.saturating_sub(4) as usize))
            .collect();
        let count = self.targets.len();
        let columns = if width < 70 { count.min(2) } else { count };
        let actions_height = (count.div_ceil(columns) * 4 - 1) as u16;
        let body = Rect::new(
            left,
            top + 4,
            width,
            bottom.saturating_sub(top + 6 + actions_height),
        );
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
        self.selected = self.selected.min(count - 1);
        let button_w = (width - (columns - 1) as u16 * 2) / columns as u16;
        for index in 0..count {
            let rect = Rect::new(
                left + (index % columns) as u16 * (button_w + 2),
                bottom - actions_height - 1 + (index / columns) as u16 * 4,
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
            let label = self.targets[index].0;
            let label = if button_w < 12 {
                match label {
                    "Recipient" => "Target",
                    "Cancel Del" => "Cancel",
                    _ => label,
                }
            } else {
                label
            };
            frame.render_widget(
                Paragraph::new(format!("\n{label}"))
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
            Paragraph::new(if !active {
                if width < 60 {
                    "↑↓ scroll · Tab · Enter · Ctrl+O"
                } else {
                    "↑↓ scroll · Tab target · Enter open · Ctrl+O window"
                }
            } else if width < 60 {
                "↑↓ scroll · Tab · E edit · Del cancel"
            } else {
                "↑↓ scroll · Tab · Enter select · E edit · Del cancel"
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
    use atman_runtime::event_log::journal::CommitReceipt;
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
    fn global_watchers_share_details_and_preserve_owner_and_acknowledged_updates() {
        let first = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let second = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: first.instance_id.clone(),
        };
        let id = Uuid::now_v7();
        let receipt = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 2,
            digest: "test".into(),
        };
        let snapshot = |status| WatchCatalogSnapshot {
            title: "Observer".into(),
            cursor: receipt.clone(),
            trailing_bytes: 0,
            records: vec![WatchRecord {
                registration: WatchRegistration::Flow(SubscriptionRecord {
                    subscription: FlowSubscription {
                        id,
                        source: first.clone(),
                        filter: FlowEventFilter::FinalAnswers,
                        once: false,
                        purpose: SubscriptionPurpose::Watcher,
                        expires_at: None,
                        reply: None,
                    },
                    registered_at: chrono::Utc::now(),
                    started_after: receipt.clone(),
                    cursor: 1,
                    status,
                }),
                progress: WatchProgress {
                    matched: 1,
                    pending: 1,
                    ..Default::default()
                },
            }],
        };
        let next = WatchCatalogCursor {
            session_id: second.session_id.0,
            before: None,
        };
        let page = |after: Option<WatchCatalogCursor>| {
            Box::new(FlowHubPage::Watchers(WatchDirectory::catalog(
                first.clone(),
                after.clone(),
                WatchCatalogPage {
                    entries: if after.is_none() {
                        vec![WatchCatalogEntry::Flow {
                            owner: first.clone(),
                            session_title: Some("Alpha".into()),
                            snapshot: Ok(snapshot(SubscriptionStatus::Active)),
                        }]
                    } else {
                        vec![
                            WatchCatalogEntry::Flow {
                                owner: second.clone(),
                                session_title: Some("Beta".into()),
                                snapshot: Ok(snapshot(SubscriptionStatus::Active)),
                            },
                            WatchCatalogEntry::SessionUnavailable {
                                session_id: Uuid::now_v7(),
                                error: "Unreadable journal".into(),
                            },
                        ]
                    },
                    next: after.is_none().then(|| next.clone()),
                    total_sessions: 3,
                    inspected: if after.is_none() { 1 } else { 2 },
                },
            )))
        };
        for (width, height) in [(40, 24), (120, 45)] {
            let ticket = ControlTicket {
                binding: ViewBinding {
                    view_id: Uuid::now_v7(),
                    revision: 1,
                    flow: first.clone().into(),
                },
                request_id: Uuid::now_v7(),
            };
            let newer = ControlTicket {
                request_id: Uuid::now_v7(),
                ..ticket.clone()
            };
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut hub = FlowHub {
                pending: Some(ticket.clone()),
                ..Default::default()
            };
            let mut history = super::super::History::default();
            history.receive(&mut hub, Some(&ticket), Ok(page(None)));
            terminal
                .draw(|frame| hub.render(frame, &first.clone().into()))
                .unwrap();
            assert!(
                matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::WatchCatalog { after: Some(ref cursor), .. }) if cursor == &next)
            );
            hub.pending = Some(newer.clone());
            history.receive(&mut hub, Some(&ticket), Ok(page(Some(next.clone()))));
            assert_eq!(hub.rows.len(), 1);
            history.receive(&mut hub, Some(&newer), Err("Temporary read error".into()));
            assert_eq!(hub.rows.len(), 1);
            hub.pending = Some(newer.clone());
            history.receive(&mut hub, Some(&newer), Ok(page(Some(next.clone()))));
            assert_eq!(hub.rows.len(), 3);
            assert!(
                matches!(&hub.rows[0].key, RowKey::Watcher { owner, id: actual } if owner == &first && actual == &id)
            );
            assert!(
                matches!(&hub.rows[1].key, RowKey::Watcher { owner, id: actual } if owner == &second && actual == &id)
            );
            hub.paste("Beta");
            terminal
                .draw(|frame| hub.render(frame, &first.clone().into()))
                .unwrap();
            assert_eq!(hub.filtered().len(), 1);
            let hit = hub.hits[0].0;
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x + 1,
                row: hit.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            terminal
                .draw(|frame| hub.render(frame, &first.clone().into()))
                .unwrap();
            assert_eq!(hub.watch_details.owner.as_ref(), Some(&second));
            assert!(
                matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&second))
            );
            hub.key(KeyAction::Tab);
            assert!(
                matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&first))
            );
            assert!(
                matches!(hub.key(KeyAction::Delete), Action::CancelWatcher { owner, id: actual } if owner == second && actual == id)
            );
            hub.pending = Some(ticket.clone());
            assert!(matches!(hub.key(KeyAction::Delete), Action::None));
            let updated = || {
                Box::new(FlowHubPage::Watchers(WatchDirectory::flow(
                    second.clone(),
                    snapshot(SubscriptionStatus::Cancelled),
                )))
            };
            history.receive(&mut hub, Some(&newer), Ok(updated()));
            assert!(hub.pending.is_some());
            history.receive(&mut hub, Some(&ticket), Ok(updated()));
            let Some(FlowHubPage::Watchers(directory)) = &hub.page else {
                panic!()
            };
            assert!(directory.global());
            assert_eq!(directory.total(), 2);
            assert_eq!(hub.query, "Beta");
            assert_eq!(hub.watch_details.owner.as_ref(), Some(&second));
            for (owner, expected) in [
                (&first, SubscriptionStatus::Active),
                (&second, SubscriptionStatus::Cancelled),
            ] {
                let WatchRegistration::Flow(record) =
                    &directory.snapshot(owner).unwrap().records[0].registration
                else {
                    panic!()
                };
                assert_eq!(record.status, expected);
                assert_eq!(
                    directory.snapshot(owner).unwrap().records[0]
                        .progress
                        .pending,
                    1
                );
            }
            terminal
                .draw(|frame| hub.render(frame, &first.clone().into()))
                .unwrap();
            assert!(matches!(hub.key(KeyAction::Delete), Action::None));
            hub.key(KeyAction::Escape);
            terminal
                .draw(|frame| hub.render(frame, &first.clone().into()))
                .unwrap();
            assert!(
                matches!(hub.key(KeyAction::NudgePrefill), Action::Load(HubRequest::Watchers { owner }) if owner == first)
            );
            let scope = hub.relations;
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: scope.x + 1, row: scope.y + 2, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Load(HubRequest::Watchers { owner }) if owner == first)
            );
            assert!(matches!(
                hub.key(KeyAction::CoursePrefill),
                Action::Load(HubRequest::Flows { .. })
            ));
            assert_eq!(hub.create_watch.as_ref().unwrap().owner, first);
        }
        let mut initial = match *page(None) {
            FlowHubPage::Watchers(directory) => directory,
            _ => unreachable!(),
        };
        let mut wrong = match *page(Some(WatchCatalogCursor {
            session_id: Uuid::nil(),
            before: None,
        })) {
            FlowHubPage::Watchers(directory) => directory,
            _ => unreachable!(),
        };
        assert!(wrong.append(Some(&mut initial)).is_err());
        assert_eq!(initial.total(), 1);
    }

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
            Box::new(FlowHubPage::Watchers(WatchDirectory::flow(
                owner.clone(),
                atman_runtime::flow_runtime::catalog::WatchCatalogSnapshot {
                    title: "WatchHub".into(),
                    cursor: receipt.clone(),
                    trailing_bytes: 0,
                    records: records.clone(),
                },
            )))
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
                    let event = MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        row,
                        column: rect.x + 1,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    };
                    let action = if index == 2 {
                        hub.watch_details.mouse(event)
                    } else {
                        hub.mouse(event)
                    };
                    let expected = if index == 0 { &owner } else { &source };
                    if index == 2 {
                        assert!(
                            matches!(action, Action::EditWatcher { owner: target, expected } if target == owner && expected.id() == selected_id)
                        );
                    } else if index == 3 {
                        assert!(
                            matches!(action, Action::CancelWatcher { owner: target, id } if target == owner && id == selected_id)
                        );
                    } else {
                        assert!(
                            matches!(action, Action::Focus(address) if address.instance() == Some(expected))
                        );
                    }
                }
            }
            assert!(
                matches!(hub.key(KeyAction::Delete), Action::CancelWatcher { owner: target, id } if target == owner && id == selected_id)
            );
            hub.pending = Some(ticket.clone());
            assert!(matches!(hub.key(KeyAction::Delete), Action::None));
            hub.failed("permission denied".into());
            terminal
                .draw(|frame| hub.render(frame, &owner.clone().into()))
                .unwrap();
            assert!(matches!(
                hub.key(KeyAction::Delete),
                Action::CancelWatcher { .. }
            ));
            // A late response cannot replace this directory's selected subscription.
            let stale = ControlTicket {
                request_id: Uuid::now_v7(),
                ..ticket.clone()
            };
            hub.pending = Some(ticket.clone());
            hub.receive(Some(&stale), Err("stale".into()));
            assert_eq!(hub.pending.as_ref(), Some(&ticket));
            hub.receive(Some(&ticket), Ok(page()));
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
                    .snapshot(&owner)
                    .unwrap()
                    .records
                    .iter()
                    .all(|record| record.progress.pending == 1)
            );
        }
    }
}
