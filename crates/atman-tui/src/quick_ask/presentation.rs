//! Fixed question chrome; document rendering and execution stay with the native view.

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::Paragraph,
};

use crate::{control::ViewBinding, flow_view::DocumentIdentity};

use super::Phase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Source,
    Refresh,
    Copy,
    Bring,
    Keep,
    NewAsk,
    Stop,
    Retry,
    Discard,
}

impl Action {
    pub const ALL: [Self; 9] = [
        Self::Source,
        Self::Refresh,
        Self::Copy,
        Self::Bring,
        Self::Keep,
        Self::NewAsk,
        Self::Stop,
        Self::Retry,
        Self::Discard,
    ];
    fn symbol(self) -> &'static str {
        match self {
            Self::Source => "↳",
            Self::Refresh => "↻",
            Self::Copy => "⧉",
            Self::Bring => "↩",
            Self::Keep => "◇",
            Self::NewAsk => "+",
            Self::Stop => "■",
            Self::Retry => "↻",
            Self::Discard => "⌫",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SourceState {
    #[default]
    Current,
    Updated,
    BranchChanged,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Retry,
    Launch,
    Refresh,
    Keep,
    Discard,
}

impl Operation {
    fn label(self) -> &'static str {
        match self {
            Self::Retry => "Retrying",
            Self::Launch => "Starting",
            Self::Refresh => "Refreshing",
            Self::Keep => "Keeping",
            Self::Discard => "Discarding",
        }
    }
}

/// A presentation snapshot contains no command sender or execution authority.
pub(crate) struct Snapshot<'a> {
    pub binding: &'a ViewBinding,
    pub document: DocumentIdentity,
    pub phase: &'a Phase,
    pub source: &'a str,
    pub source_detail: &'a str,
    pub captured: &'a str,
    pub source_state: SourceState,
    pub running: bool,
    pub has_answer: bool,
    pub retry: bool,
    pub frozen: bool,
    pub operation: Option<Operation>,
    pub frame: u32,
}

impl Snapshot<'_> {
    pub fn enabled(&self, action: Action) -> bool {
        let idle = !self.running && self.operation.is_none() && !self.retry && !self.frozen;
        let available = self.source_state != SourceState::Unavailable;
        match action {
            Action::Source => available,
            Action::Retry => self.retry,
            Action::Refresh => {
                idle && !matches!(self.phase, Phase::Draft)
                    && matches!(
                        self.source_state,
                        SourceState::Current | SourceState::Updated
                    )
            }
            Action::Copy => self.has_answer,
            Action::Bring => self.has_answer && available,
            Action::Keep => {
                !self.running
                    && self.operation.is_none()
                    && !self.retry
                    && matches!(self.phase, Phase::Transient(_))
            }
            Action::NewAsk => available && self.operation.is_none(),
            Action::Stop => {
                self.running
                    && !matches!(self.phase, Phase::Draft)
                    && !matches!(self.operation, Some(Operation::Keep | Operation::Discard))
            }
            Action::Discard => {
                !matches!(self.phase, Phase::Kept(_))
                    && (self.operation.is_none() || self.retry)
                    && !self.frozen
            }
        }
    }

    pub fn hint(&self, action: Action) -> &str {
        match action {
            Action::Source if !self.enabled(action) => "Source unavailable",
            Action::Source => self.source_detail,
            Action::Refresh if self.source_state == SourceState::BranchChanged => {
                "Source branch changed"
            }
            Action::Refresh if self.source_state == SourceState::Unavailable => {
                "Open source to refresh"
            }
            Action::Refresh if self.running => "Refresh after this response",
            Action::Refresh if matches!(self.phase, Phase::Draft) => "Context is captured on Send",
            Action::Refresh => "Refresh source context",
            Action::Copy => "Copy answer",
            Action::Bring if self.source_state == SourceState::BranchChanged => {
                "Review the source branch before bringing"
            }
            Action::Bring if self.source_state == SourceState::Unavailable => {
                "Source unavailable; Copy remains available"
            }
            Action::Bring => "Append answer to source draft",
            Action::Keep if matches!(self.phase, Phase::Kept(_)) => "Kept in Flow panel",
            Action::Keep if self.frozen => "Retry pending Keep",
            Action::Keep if self.running => "Keep after this response",
            Action::Keep if matches!(self.phase, Phase::Draft) => "Send a question before Keep",
            Action::Keep => "Keep in Flow panel",
            Action::NewAsk => "New question",
            Action::Stop => "Stop this response",
            Action::Retry => "Retry the accepted operation",
            Action::Discard if matches!(self.phase, Phase::Kept(_)) => {
                "Manage kept history in Flow panel"
            }
            Action::Discard => "Discard this temporary question",
        }
    }
}

/// Header and footer reservations never depend on running, loading or answer state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Geometry {
    pub header: Rect,
    pub body: Rect,
    pub footer: Rect,
}

impl Geometry {
    pub fn new(content: Rect) -> Self {
        let header = Rect::new(
            content.x,
            content.y,
            content.width,
            u16::from(content.height > 0),
        );
        let footer = Rect::new(
            content.x,
            content.bottom().saturating_sub(1),
            content.width,
            u16::from(content.height > 1),
        );
        let body = Rect::new(
            content.x,
            header.bottom(),
            content.width,
            content.height.saturating_sub(header.height + footer.height),
        );
        Self {
            header,
            body,
            footer,
        }
    }
}

struct Painted {
    binding: ViewBinding,
    document: DocumentIdentity,
    geometry: Geometry,
    controls: Vec<(Action, Rect)>,
}

/// Only the most recently painted owner and document can receive a chrome action.
#[derive(Default)]
pub(crate) struct Chrome {
    painted: Option<Painted>,
}

impl Chrome {
    pub fn contains(
        &self,
        binding: &ViewBinding,
        document: &DocumentIdentity,
        point: Position,
    ) -> bool {
        self.painted.as_ref().is_some_and(|painted| {
            &painted.binding == binding
                && &painted.document == document
                && (painted.geometry.header.contains(point)
                    || painted.geometry.footer.contains(point))
        })
    }
    pub fn clear(&mut self) {
        self.painted = None;
    }

    pub fn action_at(
        &self,
        binding: &ViewBinding,
        document: &DocumentIdentity,
        point: Position,
    ) -> Option<Action> {
        let painted = self.painted.as_ref()?;
        if &painted.binding != binding || &painted.document != document {
            return None;
        }
        painted
            .controls
            .iter()
            .find(|(_, area)| area.contains(point))
            .map(|(action, _)| *action)
    }

    /// The caller renders the ordinary document into `body` and applies its existing modal gates.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        content: Rect,
        snapshot: &Snapshot<'_>,
        pointer: Option<Position>,
        focused: Option<Action>,
    ) -> Geometry {
        let geometry = Geometry::new(content);
        let theme = crate::theme::theme();
        let mut controls = Vec::new();
        let mut hovered = None;
        let mut timestamp = None;
        let mut paint = |action: Action, area: Rect, label: &str| {
            if area.is_empty() {
                return;
            }
            let enabled = snapshot.enabled(action);
            let hover = pointer.is_some_and(|point| area.contains(point));
            let focus = focused == Some(action);
            let foreground = if !enabled {
                theme.subtle_fg
            } else if action == Action::Stop {
                theme.error
            } else if action == Action::Keep && snapshot.frozen {
                theme.warn
            } else if action == Action::Refresh && snapshot.source_state == SourceState::Updated {
                theme.accent
            } else if action == Action::Source {
                theme.work_meta_fg
            } else {
                theme.work_action_fg
            };
            let mut style =
                Style::default()
                    .fg(foreground.into())
                    .bg(if enabled && (hover || focus) {
                        theme.work_hover_bg.into()
                    } else {
                        Color::Reset
                    });
            if !enabled {
                style = style.add_modifier(Modifier::DIM);
            }
            frame.render_widget(Paragraph::new(label.to_owned()).style(style), area);
            if enabled {
                controls.push((action, area));
            }
            if hover || focus {
                hovered = Some(action);
            }
        };
        let header = geometry.header;
        if header.width >= 9 && header.height > 0 {
            let refresh = Rect::new(header.right() - 3, header.y, 3, 1);
            let stamp_width = (crate::width::width(snapshot.captured).min(8) as u16)
                .min(header.width.saturating_sub(9));
            let stamp = Rect::new(
                refresh.x.saturating_sub(stamp_width + 1).max(header.x),
                header.y,
                stamp_width,
                1,
            );
            let source_width = stamp.x.saturating_sub(header.x + 1);
            let source = Rect::new(header.x, header.y, source_width, 1);
            let marker = match snapshot.source_state {
                SourceState::Updated => "•",
                SourceState::BranchChanged | SourceState::Unavailable => "!",
                SourceState::Current => "↳",
            };
            let label = format!(
                " {marker} {} ",
                crate::width::middle_truncate(
                    snapshot.source,
                    source_width.saturating_sub(4) as usize
                )
            );
            paint(Action::Source, source, &label);
            paint(Action::Refresh, refresh, " ↻ ");
            timestamp = Some(stamp);
        }
        let footer = geometry.footer;
        if footer.width >= 27 && footer.height > 0 {
            for (index, action) in [Action::Copy, Action::Bring].into_iter().enumerate() {
                paint(
                    action,
                    Rect::new(footer.x + index as u16 * 4, footer.y, 3, 1),
                    &format!(" {} ", action.symbol()),
                );
            }
            for (index, action) in [
                Action::Keep,
                Action::NewAsk,
                if snapshot.retry {
                    Action::Retry
                } else {
                    Action::Stop
                },
                Action::Discard,
            ]
            .into_iter()
            .enumerate()
            {
                paint(
                    action,
                    Rect::new(footer.right() - 15 + index as u16 * 4, footer.y, 3, 1),
                    &format!(" {} ", action.symbol()),
                );
            }
        }
        if let Some(stamp) = timestamp {
            frame.render_widget(
                Paragraph::new(snapshot.captured)
                    .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
                stamp,
            );
        }
        let status = if let Some(action) = hovered {
            snapshot.hint(action).to_owned()
        } else if let Some(operation) = snapshot.operation {
            format!(
                "{} {}",
                crate::loading::glyph(snapshot.frame),
                operation.label()
            )
        } else {
            String::new()
        };
        if footer.width > 25 && footer.height > 0 {
            let status_area = Rect::new(footer.x + 8, footer.y, footer.width - 25, 1);
            frame.render_widget(
                Paragraph::new(crate::width::truncate(&status, status_area.width as usize))
                    .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
                status_area,
            );
        }
        self.painted = Some(Painted {
            binding: snapshot.binding.clone(),
            document: snapshot.document.clone(),
            geometry,
            controls,
        });
        geometry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ViewAddress;
    use uuid::Uuid;

    #[test]
    fn question_retry_states_keep_the_recovery_action_reachable() {
        let question = crate::quick_ask::tests::question();
        let flow = atman_runtime::flow_store::FlowRef {
            session_id: question.source.flow.session_id,
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let binding = ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 2,
            flow: flow.clone().into(),
        };
        let phase = Phase::Transient(flow);
        let mut snapshot = Snapshot {
            binding: &binding,
            document: DocumentIdentity::default(),
            phase: &phase,
            source: "source",
            source_detail: "source",
            captured: "",
            source_state: SourceState::Current,
            running: false,
            has_answer: true,
            retry: false,
            frozen: true,
            operation: None,
            frame: 0,
        };
        assert!(snapshot.enabled(Action::Keep));
        assert!(!snapshot.enabled(Action::Refresh));
        assert!(!snapshot.enabled(Action::Discard));
        assert!(snapshot.enabled(Action::Copy));
        snapshot.retry = true;
        assert!(snapshot.enabled(Action::Retry));
        assert!(!snapshot.enabled(Action::Keep));
        snapshot.frozen = false;
        assert!(snapshot.enabled(Action::Discard));
    }

    #[test]
    fn chrome_reservations_are_disjoint_and_bounded_at_every_size() {
        for width in 0..90 {
            for height in 0..25 {
                let content = Rect::new(3, 4, width, height);
                let geometry = Geometry::new(content);
                assert_eq!(
                    geometry.header.height + geometry.body.height + geometry.footer.height,
                    height
                );
                for rect in [geometry.header, geometry.body, geometry.footer] {
                    if !rect.is_empty() {
                        assert_eq!(rect.intersection(content), rect);
                    }
                }
                assert!(geometry.header.intersection(geometry.body).is_empty());
                assert!(geometry.footer.intersection(geometry.body).is_empty());
            }
        }
    }

    #[test]
    fn old_chrome_cannot_target_a_rebound_view_or_replaced_document() {
        let binding = ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: ViewAddress::QuestionDraft {
                session: Uuid::new_v4(),
                id: Uuid::new_v4(),
            },
        };
        let document = DocumentIdentity::default();
        let chrome = Chrome {
            painted: Some(Painted {
                binding: binding.clone(),
                document: document.clone(),
                geometry: Geometry::new(Rect::new(1, 2, 8, 5)),
                controls: vec![(Action::Keep, Rect::new(1, 2, 3, 1))],
            }),
        };
        assert_eq!(
            chrome.action_at(&binding, &document, (2, 2).into()),
            Some(Action::Keep)
        );
        assert!(
            chrome.contains(&binding, &document, (7, 2).into()),
            "chrome whitespace is not transcript input"
        );
        assert!(!chrome.contains(&binding, &document, (7, 4).into()));
        assert_eq!(
            chrome.action_at(
                &ViewBinding {
                    revision: 2,
                    ..binding.clone()
                },
                &document,
                (2, 2).into()
            ),
            None
        );
        assert_eq!(
            chrome.action_at(&binding, &DocumentIdentity::default(), (2, 2).into()),
            None
        );
    }
}
