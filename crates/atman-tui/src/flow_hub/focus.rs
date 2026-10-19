//! View-local neighborhood snapshots and shared directory actions.

use atman_runtime::flow_store::FlowRef;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::{Clear, Paragraph},
};

use super::{
    Action, FlowHubPage, RelationRange,
    page::{Endpoint, Row, RowKey},
};

pub(crate) struct NeighborSnapshot {
    owner: FlowRef,
    title: &'static str,
    rows: Vec<Row>,
}

impl NeighborSnapshot {
    pub(super) fn from_page(page: &FlowHubPage, owner: &FlowRef) -> Option<Self> {
        let FlowHubPage::Relations {
            focus: Some(focus),
            range,
            progress,
            ..
        } = page
        else {
            return None;
        };
        if focus != owner {
            return None;
        }
        Some(Self {
            owner: owner.clone(),
            title: match range {
                RelationRange::Session => "Session snapshot",
                RelationRange::Catalog { .. }
                    if progress
                        .as_ref()
                        .is_some_and(|p| p.next.is_some() || !p.errors.is_empty()) =>
                {
                    "Partial global"
                }
                RelationRange::Catalog { .. } => "Global snapshot",
            },
            rows: page
                .rows(None, Endpoint::Neighbor)
                .into_iter()
                .filter(|row| matches!(row.key, RowKey::Relation { .. }))
                .collect(),
        })
    }
}

#[derive(Default)]
pub(crate) struct FocusLinks {
    snapshot: Option<NeighborSnapshot>,
    owner: Option<FlowRef>,
    hits: Vec<(Rect, Action)>,
    hovered: Option<usize>,
}

impl FocusLinks {
    pub fn height(&self, owner: &FlowRef, limit: u16) -> u16 {
        if limit < 7 {
            return 0;
        }
        let rows = self
            .snapshot
            .as_ref()
            .filter(|snapshot| &snapshot.owner == owner)
            .map_or(0, |snapshot| snapshot.rows.len());
        7 + rows.min(3).min(usize::from((limit - 7) / 4)) as u16 * 4
    }
    pub fn update(&mut self, snapshot: NeighborSnapshot) {
        self.snapshot = Some(snapshot);
    }

    pub fn clear_hits(&mut self) {
        self.hits.clear();
        self.owner = None;
    }

    pub fn mouse(&mut self, mouse: MouseEvent, owner: &FlowRef) -> Option<Action> {
        if self.owner.as_ref() != Some(owner) {
            return None;
        }
        self.hovered = self
            .hits
            .iter()
            .position(|(rect, _)| rect.contains((mouse.column, mouse.row).into()));
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return self.hovered.map(|index| self.hits[index].1.clone());
        }
        None
    }

    /// Return the remaining sidebar area for ordinary task activity.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, owner: &FlowRef) -> Rect {
        self.clear_hits();
        if area.width < 24 || area.height < 7 {
            return area;
        }
        self.owner = Some(owner.clone());
        let snapshot = self
            .snapshot
            .as_ref()
            .filter(|snapshot| &snapshot.owner == owner);
        let count = snapshot
            .map_or(0, |snapshot| snapshot.rows.len())
            .min(3)
            .min(usize::from(area.height.saturating_sub(7) / 4));
        let height = 7 + count as u16 * 4;
        let theme = crate::theme::theme();
        let panel = Rect::new(area.x, area.y, area.width, height);
        crate::sanitize_widget_edges(frame, panel);
        frame.render_widget(Clear, panel);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme.modal_bg.into())),
            panel,
        );
        frame.render_widget(
            Paragraph::new(snapshot.map_or_else(
                || "Neighbors".to_owned(),
                |snapshot| format!("Neighbors ({}) · {}", snapshot.rows.len(), snapshot.title),
            ))
            .style(Style::default().fg(theme.meta_fg.into())),
            Rect::new(area.x + 2, area.y + 1, area.width - 4, 1),
        );
        if let Some(snapshot) = snapshot {
            for (index, row) in snapshot.rows.iter().take(count).enumerate() {
                let rect = Rect::new(area.x + 1, area.y + 2 + index as u16 * 4, area.width - 2, 4);
                let bg = if self.hovered == Some(index) {
                    theme.work_bg.lerp(theme.work_hover_bg, 0.5)
                } else {
                    theme.work_bg.into_inner()
                };
                frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), rect);
                frame.render_widget(
                    Paragraph::new(row.title.as_str())
                        .style(Style::default().fg(theme.heading.into()).bg(bg)),
                    Rect::new(rect.x + 2, rect.y + 1, rect.width - 4, 1),
                );
                frame.render_widget(
                    Paragraph::new(row.detail.as_str())
                        .style(Style::default().fg(theme.meta_fg.into()).bg(bg)),
                    Rect::new(rect.x + 2, rect.y + 2, rect.width - 4, 1),
                );
                self.hits.push((rect, row.action.clone()));
            }
        }
        let button = Rect::new(area.x + 1, area.y + 3 + count as u16 * 4, area.width - 2, 3);
        let bg = if self.hovered == Some(self.hits.len()) {
            theme.work_bg.lerp(theme.work_hover_bg, 0.5)
        } else {
            theme.work_bg.into_inner()
        };
        frame.render_widget(
            Paragraph::new("\nLinks Ctrl+Shift+G")
                .alignment(Alignment::Center)
                .style(Style::default().fg(theme.heading.into()).bg(bg)),
            button,
        );
        self.hits.push((
            button,
            Action::Load(super::HubRequest::Relations {
                session: owner.session_id.0,
                focus: Some(owner.clone()),
                range: RelationRange::Session,
            }),
        ));
        Rect::new(area.x, area.y + height, area.width, area.height - height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::flow_store::{
        access::AccessTarget,
        relation::{FlowRelation, RelationKind},
    };
    use crossterm::event::KeyModifiers;
    use uuid::Uuid;

    #[test]
    fn neighborhood_views_share_targets_and_bind_hits_to_the_full_address() {
        let owner = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        let neighbor = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: owner.instance_id.clone(),
        };
        let mut page = FlowHubPage::Relations {
            session: owner.session_id.0,
            title: "Links".into(),
            focus: Some(owner.clone()),
            range: RelationRange::Session,
            progress: None,
            relations: vec![FlowRelation {
                source: neighbor.clone(),
                target: AccessTarget::Flow(owner.clone()),
                kind: RelationKind::Watch,
                id: Uuid::now_v7(),
                active: true,
            }],
        };
        assert!(NeighborSnapshot::from_page(&page, &neighbor).is_none());
        // Placement changes geometry, never routing or execution ownership.
        for area in [Rect::new(0, 0, 40, 30), Rect::new(7, 6, 40, 20)] {
            let mut links = FocusLinks::default();
            links.update(NeighborSnapshot::from_page(&page, &owner).unwrap());
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 40)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(vec![ratatui::text::Line::from("X".repeat(80)); 40]),
                        frame.area(),
                    );
                    let rest = links.render(frame, area, &owner);
                    assert_eq!(rest.bottom(), area.bottom());
                    assert!(rest.height >= 6);
                })
                .unwrap();
            assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), " ");
            assert_eq!(links.hits.len(), 2);
            for (index, rect) in links
                .hits
                .iter()
                .map(|(rect, _)| *rect)
                .collect::<Vec<_>>()
                .into_iter()
                .enumerate()
            {
                assert_eq!(rect.intersection(area), rect);
                for row in rect.y..rect.bottom() {
                    for column in rect.x..rect.right() {
                        let mouse = MouseEvent {
                            kind: MouseEventKind::Down(MouseButton::Left),
                            column,
                            row,
                            modifiers: KeyModifiers::NONE,
                        };
                        assert!(links.mouse(mouse, &neighbor).is_none());
                        let action = links.mouse(mouse, &owner).unwrap();
                        if index == 0 {
                            assert!(
                                matches!(action, Action::Focus(address) if address.instance() == Some(&neighbor))
                            );
                        } else {
                            assert!(
                                matches!(action, Action::Load(super::super::HubRequest::Relations { focus: Some(target), .. }) if target == owner)
                            );
                        }
                    }
                }
            }
            terminal
                .draw(|frame| {
                    links.render(frame, Rect::new(0, 0, 5, 10), &owner);
                })
                .unwrap();
            assert!(links.hits.is_empty());
            assert!(links.owner.is_none());
        }
        let FlowHubPage::Relations {
            range, progress, ..
        } = &mut page
        else {
            unreachable!()
        };
        *range = RelationRange::Catalog { before: None };
        *progress = Some(super::super::RelationProgress {
            scanned: 2,
            total: 2,
            next: None,
            errors: vec![(Uuid::now_v7(), "unreadable".into())],
        });
        let snapshot = NeighborSnapshot::from_page(&page, &owner).unwrap();
        assert_eq!(snapshot.title, "Partial global");
        assert_eq!(
            snapshot.rows.len(),
            1,
            "unreadable sessions are not neighbors"
        );
    }
}
