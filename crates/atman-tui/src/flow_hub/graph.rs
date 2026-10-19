//! Stable page-local node positions. Only selected incident edges are drawn.

use std::collections::{BTreeMap, HashMap};

use atman_runtime::flow_store::{
    access::AccessTarget,
    relation::{FlowRelation, RelationKind},
};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};

use super::{
    Action, FlowHub,
    page::{Row, RowKey},
};

#[derive(Default)]
pub(super) struct Layout {
    slots: Vec<NodeKey>,
    index: HashMap<NodeKey, usize>,
    top: usize,
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum NodeKey {
    Flow(crate::control::ViewAddress),
    UnavailableSession(uuid::Uuid),
}

impl NodeKey {
    fn from_row(row: &Row) -> Self {
        match &row.key {
            RowKey::Flow(address) => Self::Flow(address.clone()),
            RowKey::OverviewFailure(session) => Self::UnavailableSession(*session),
            _ => unreachable!("graph rows must identify a Flow or unavailable Session"),
        }
    }
}

#[derive(Default)]
pub(super) struct GroupedCards {
    pub cards: Vec<(usize, Rect)>,
    pub headings: Vec<(uuid::Uuid, Rect)>,
}

impl Layout {
    /// Session bands follow stable slots; appended nodes never move existing cards.
    pub fn grouped_cards(
        &mut self,
        area: Rect,
        rows: &[Row],
        filtered: &[usize],
        selected: usize,
        graph: bool,
    ) -> GroupedCards {
        if area.height < 5 || area.width < 8 {
            return GroupedCards::default();
        }
        let columns = if graph && area.width >= 88 { 2 } else { 1 };
        let heading = (area.height - 4).min(2);
        let card_height = usize::from((area.height - heading).min(6));
        let step = card_height + 2;
        let height = usize::from(area.height - heading);
        let width = if columns == 2 {
            (area.width - 12) / 2
        } else {
            area.width - 4
        };
        let mut positions = Vec::new();
        let mut previous = None;
        let (mut line, mut column) = (0usize, 0usize);
        for key in &self.slots {
            let session = match key {
                NodeKey::Flow(address) => address.instance().map(|flow| flow.session_id.0),
                NodeKey::UnavailableSession(session) => Some(*session),
            };
            if session != previous && previous.is_some() {
                if column != 0 {
                    line += step;
                    column = 0;
                }
                line += 2;
            }
            positions.push((line, column, session));
            previous = session;
            column += 1;
            if column == columns {
                column = 0;
                line += step;
            }
        }
        let slot = |row: &Row| self.index[&NodeKey::from_row(row)];
        if let Some(index) = filtered.get(selected) {
            let y = positions[slot(&rows[*index])].0;
            if y < self.top {
                self.top = y;
            }
            if y + card_height > self.top + height {
                self.top = y + card_height - height;
            }
        }
        let mut cards = Vec::new();
        let mut groups = Vec::new();
        let mut last_group = None;
        let visible: HashMap<_, _> = filtered
            .iter()
            .enumerate()
            .map(|(pos, index)| (NodeKey::from_row(&rows[*index]), pos))
            .collect();
        // Traverse slots, preserving spatial order even when directory refresh reorders rows.
        for (index, &(y, x, session)) in positions.iter().enumerate() {
            if y < self.top || y + card_height > self.top + height {
                continue;
            }
            let Some(&pos) = visible.get(&self.slots[index]) else {
                continue;
            };
            let rect = Rect::new(
                area.x + 2 + x as u16 * (width + 8),
                area.y + heading + (y - self.top) as u16,
                width,
                card_height as u16,
            );
            if session != last_group {
                if let Some(session) = session {
                    groups.push((
                        session,
                        Rect::new(
                            area.x + 2,
                            if groups.is_empty() {
                                area.y
                            } else {
                                rect.y.saturating_sub(2)
                            },
                            area.width - 4,
                            1,
                        ),
                    ));
                }
                last_group = session;
            }
            cards.push((pos, rect));
        }
        GroupedCards {
            cards,
            headings: groups,
        }
    }

    pub fn sync(&mut self, rows: &[Row]) {
        for row in rows {
            let key = NodeKey::from_row(row);
            if let std::collections::hash_map::Entry::Vacant(entry) = self.index.entry(key.clone())
            {
                entry.insert(self.slots.len());
                self.slots.push(key);
            }
        }
    }

    pub fn cards(
        &mut self,
        area: Rect,
        rows: &[Row],
        filtered: &[usize],
        selected: usize,
    ) -> Vec<(usize, Rect)> {
        let columns = if area.width >= 88 { 2 } else { 1 };
        let height = usize::from(area.height / 6).max(1);
        let slot = |row: &Row| self.index[&NodeKey::from_row(row)];
        if let Some(index) = filtered.get(selected) {
            let y = slot(&rows[*index]) / columns;
            if y < self.top {
                self.top = y;
            }
            if y >= self.top + height {
                self.top = y + 1 - height;
            }
        }
        if area.height < 4 {
            return Vec::new();
        }
        let width = if columns == 2 {
            (area.width - 12) / 2
        } else {
            area.width - 4
        };
        filtered
            .iter()
            .enumerate()
            .filter_map(|(pos, index)| {
                let slot = slot(&rows[*index]);
                let y = slot / columns;
                if y < self.top || y >= self.top + height {
                    return None;
                }
                Some((
                    pos,
                    Rect::new(
                        area.x + 2 + (slot % columns) as u16 * (width + 8),
                        area.y + ((y - self.top) * 6) as u16,
                        width,
                        5,
                    ),
                ))
            })
            .collect()
    }
}

fn address(row: &Row) -> Option<&atman_runtime::flow_store::FlowRef> {
    match &row.action {
        Action::Focus(address) => address.instance(),
        _ => None,
    }
}

pub(super) fn compact_detail(row: &Row) -> String {
    match address(row) {
        Some(flow) => {
            let id = flow.instance_id.to_string();
            row.detail
                .strip_prefix(&id)
                .map(|suffix| format!("{}{suffix}", &id[id.len() - 8..]))
                .unwrap_or_else(|| row.detail.clone())
        }
        None => row.detail.clone(),
    }
}

fn kind_name(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Created => "Created",
        RelationKind::Read => "Read",
        RelationKind::Watch => "Watch",
        RelationKind::Interject => "Interject",
    }
}

pub(super) fn has_link(row: &Row, relations: &[FlowRelation], kind: &str) -> bool {
    if let RowKey::Relation {
        kind: relation_kind,
        ..
    } = row.key
    {
        return kind_name(relation_kind).eq_ignore_ascii_case(kind);
    }
    let Some(flow) = address(row) else {
        return false;
    };
    relations.iter().any(|relation| {
        kind_name(relation.kind).eq_ignore_ascii_case(kind) && super::page::incident(relation, flow)
    })
}

pub(super) fn draw_links(
    frame: &mut Frame,
    cards: &[(usize, Rect)],
    hub: &FlowHub,
    filtered: &[usize],
    caption: Rect,
) {
    let Some(page) = &hub.page else {
        return;
    };
    let rows = &hub.rows;
    let theme = crate::theme::theme();
    let style = Style::default().fg(theme.meta_fg.into());
    let relations = match page.links() {
        Ok(relations) => relations,
        Err(error) => {
            frame.render_widget(
                Paragraph::new(format!("Links unavailable: {error}")).style(style),
                caption,
            );
            return;
        }
    };
    let Some(selected) = filtered
        .get(hub.selected)
        .and_then(|index| address(&rows[*index]))
    else {
        return;
    };
    let find = |flow: &atman_runtime::flow_store::FlowRef| {
        cards
            .iter()
            .find_map(|(pos, rect)| (address(&rows[filtered[*pos]]) == Some(flow)).then_some(*rect))
    };
    let mut links = BTreeMap::new();
    let mut arrows = Vec::new();
    let mut labels = Vec::new();
    let query = hub.query.to_lowercase();
    let filters: Vec<_> = query
        .split_whitespace()
        .filter_map(|term| term.strip_prefix("link:"))
        .collect();
    for relation in relations {
        if !filters.is_empty()
            && !filters
                .iter()
                .any(|kind| kind_name(relation.kind).eq_ignore_ascii_case(kind))
        {
            continue;
        }
        let target = match &relation.target {
            AccessTarget::Flow(flow) => Some(flow),
            _ => None,
        };
        let outgoing = &relation.source == selected;
        if !super::page::incident(relation, selected) {
            continue;
        }
        let kind = kind_name(relation.kind);
        let other = if outgoing {
            target
        } else {
            Some(&relation.source)
        };
        let name = other
            .map(|flow| {
                rows.iter()
                    .find(|row| address(row) == Some(flow))
                    .map(|row| row.title.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "{}/{}",
                            &flow.session_id.to_string()[..8],
                            &flow.instance_id.to_string()[28..]
                        )
                    })
            })
            .unwrap_or_else(|| match &relation.target {
                AccessTarget::Session(id) => format!("session {}", &id.to_string()[..8]),
                _ => "all sessions".into(),
            });
        labels.push(format!(
            "{kind} {} {name}{}",
            if outgoing { "→" } else { "←" },
            if relation.active { "" } else { " (inactive)" }
        ));
        if !relation.active {
            continue;
        }
        let (Some(source), Some(target)) = (find(&relation.source), target.and_then(find)) else {
            continue;
        };
        if source == target {
            continue;
        }
        let (sx, tx, lane, arrow) = if source.x < target.x {
            (source.right(), target.x - 1, source.right() + 3, "→")
        } else if source.x > target.x {
            (source.x - 1, target.right(), target.right() + 3, "←")
        } else {
            (source.x - 1, target.x - 1, source.x - 2, "→")
        };
        let sy = source.y + 2;
        let ty = target.y + 2;
        path(&mut links, (sx, sy), (lane, sy));
        path(&mut links, (lane, sy), (lane, ty));
        path(&mut links, (lane, ty), (tx, ty));
        arrows.push((tx, ty, arrow));
    }
    let buffer = frame.buffer_mut();
    for ((x, y), mask) in links {
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol(match mask {
                3 => "│",
                12 => "─",
                5 => "┘",
                9 => "└",
                6 => "┐",
                10 => "┌",
                7 => "┤",
                11 => "├",
                13 => "┴",
                14 => "┬",
                15 => "┼",
                1 | 2 => "│",
                _ => "─",
            })
            .set_style(style);
        }
    }
    for (x, y, arrow) in arrows {
        if let Some(cell) = buffer.cell_mut((x, y)) {
            cell.set_symbol(arrow).set_style(style);
        }
    }
    let text = if labels.is_empty() {
        "No recorded links for this selection".into()
    } else {
        labels.join(" · ")
    };
    frame.render_widget(Paragraph::new(text).style(style), caption);
}

// N=1, S=2, W=4, E=8; joins are accumulated before drawing.
fn path(cells: &mut BTreeMap<(u16, u16), u8>, from: (u16, u16), to: (u16, u16)) {
    if from.1 == to.1 {
        for x in from.0.min(to.0)..from.0.max(to.0) {
            *cells.entry((x, from.1)).or_default() |= 8;
            *cells.entry((x + 1, from.1)).or_default() |= 4;
        }
    } else {
        for y in from.1.min(to.1)..from.1.max(to.1) {
            *cells.entry((from.0, y)).or_default() |= 2;
            *cells.entry((from.0, y + 1)).or_default() |= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_hub::FlowHubPage;
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_runtime::access::InstancePage,
        flow_store::{FlowInstanceId, FlowRef},
    };
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use uuid::Uuid;

    fn fixture() -> FlowHub {
        let session = Uuid::now_v7();
        let rows: Vec<_> = (0..4)
            .map(|i| {
                let flow = FlowRef {
                    session_id: session.into(),
                    instance_id: FlowInstanceId::now(),
                };
                Row {
                    key: RowKey::Flow(flow.clone().into()),
                    title: format!("Worker {i}"),
                    detail: format!("{} · Idle", flow.instance_id),
                    action: Action::Focus(flow.into()),
                }
            })
            .collect();
        let relation = FlowRelation {
            source: address(&rows[0]).unwrap().clone(),
            target: AccessTarget::Flow(address(&rows[1]).unwrap().clone()),
            kind: RelationKind::Watch,
            active: true,
            id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            page: Some(FlowHubPage::Flows {
                session,
                title: "Example".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![],
                    next: None,
                    total: rows.len(),
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![relation]),
            }),
            rows,
            ..Default::default()
        };
        hub.graph.sync(&hub.rows);
        hub
    }

    #[test]
    fn node_identity_preserves_positions_through_reorder_filter_and_append() {
        let mut hub = fixture();
        let area = Rect::new(2, 12, 112, 30);
        let original = hub.graph.cards(area, &hub.rows, &[0, 1, 2, 3], 0);
        hub.rows.swap(0, 3);
        hub.graph.sync(&hub.rows);
        let reordered = hub.graph.cards(area, &hub.rows, &[0, 1, 2, 3], 0);
        assert_eq!(reordered[3].1, original[0].1);
        let filtered = hub.graph.cards(area, &hub.rows, &[3], 0);
        assert_eq!(filtered[0].1, original[0].1);
        hub.rows.extend(fixture().rows);
        hub.graph.sync(&hub.rows);
        let appended = hub.graph.cards(area, &hub.rows, &[3, 4], 0);
        assert_eq!(appended[0].1, original[0].1);
        assert!(appended[1].1.y > appended[0].1.y);
        for width in [36, 56, 88, 112] {
            let area = Rect::new(2, 12, width, 5);
            let visible = hub.graph.cards(area, &hub.rows, &[0, 1, 2, 3], 2);
            assert!(visible.iter().any(|(pos, _)| *pos == 2));
            assert!(
                visible
                    .iter()
                    .all(|(_, rect)| area.contains((rect.x, rect.y).into())
                        && rect.right() <= area.right()
                        && rect.bottom() <= area.bottom())
            );
        }
    }

    #[test]
    fn graph_and_list_share_targets_selection_and_directed_link_filtering() {
        use crate::keys::KeyAction;
        let mut hub = fixture();
        let focus = address(&hub.rows[0]).unwrap().clone().into();
        let target = address(&hub.rows[1]).unwrap().clone();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        let render =
            |hub: &mut FlowHub, terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>| {
                terminal.draw(|frame| hub.render(frame, &focus)).unwrap();
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            };
        render(&mut hub, &mut terminal);
        hub.key(KeyAction::HistoryDown);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(flow) if flow.instance() == Some(&target))
        );
        hub.key(KeyAction::NudgePrefill);
        let text = render(&mut hub, &mut terminal);
        assert!(text.contains("Watch ← Worker 0"));
        assert_eq!(hub.hits.len(), 4); // Unrelated flows remain visible.
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(flow) if flow.instance() == Some(&target))
        );
        let rect = hub.hits.iter().find(|(_, pos)| *pos == 1).unwrap().0;
        for row in rect.y..rect.bottom() {
            assert!(matches!(hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1, row, modifiers: KeyModifiers::NONE,
            }), Action::Focus(flow) if flow.instance() == Some(&target)));
        }
        let toggle = hub.graph_button;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: toggle.x + 1,
            row: toggle.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!hub.graph_visible);
        assert_eq!(hub.selected, 1);
        hub.query = "LINK:WATCH Worker".into();
        assert_eq!(hub.filtered(), vec![0, 1]);
        hub.key(KeyAction::NudgePrefill);
        assert!(render(&mut hub, &mut terminal).contains("Watch ← Worker 0"));
        let Some(FlowHubPage::Flows { relations, .. }) = &mut hub.page else {
            unreachable!()
        };
        relations.as_mut().unwrap()[0].active = false;
        assert!(render(&mut hub, &mut terminal).contains("(inactive)"));
        let Some(FlowHubPage::Flows { relations, .. }) = &mut hub.page else {
            unreachable!()
        };
        *relations = Err("storage unavailable".into());
        hub.query.clear();
        assert!(render(&mut hub, &mut terminal).contains("Links unavailable: storage unavailable"));
        assert_eq!(hub.hits.len(), 4);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(flow) if flow.instance() == Some(&target))
        );
    }
}
