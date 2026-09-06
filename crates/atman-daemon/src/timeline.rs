use std::collections::{HashMap, HashSet};

use atman_proto::{
    EventCursor, FlowRunId, MessagePart, Revision, RunLifecycle, SessionId, SessionProjection,
    SessionTimelineBudget, SessionTimelinePage, TimelineCursor, TimelineDetailRef, TimelineItem,
    TimelineItemId, TimelineItemKind, TimelineLiveState, TimelineRemaining, TimelineSegment,
    TimelineSegmentId, TimelineSessionSegment, TimelineTurnSegment, TimelineTurnState,
    TranscriptItem, TurnId,
};

const DEFAULT_TURN_BUDGET: usize = 12;
const DEFAULT_BYTE_BUDGET: usize = 256 * 1024;
const MAX_PREVIEW_TEXT_BYTES: usize = 8 * 1024;

pub(crate) struct TimelineCatalog<'a> {
    session_id: SessionId,
    cursor: EventCursor,
    projection_revision: Revision,
    projection: &'a SessionProjection,
    segments: Vec<SegmentRecord>,
    live: TimelineLiveState,
}

enum Window {
    Tail,
    Before(TimelineCursor),
    After(TimelineCursor),
    Around(TimelineItemId),
}

impl<'a> TimelineCatalog<'a> {
    pub(crate) fn from_projection(cursor: EventCursor, projection: &'a SessionProjection) -> Self {
        let run_turns = projection
            .runs
            .iter()
            .filter_map(|run| run.turn_id.clone().map(|turn_id| (run.id.clone(), turn_id)))
            .collect::<HashMap<_, _>>();
        let active_turns = projection
            .runs
            .iter()
            .filter(|run| !run_is_terminal(run.state))
            .filter_map(|run| run.turn_id.clone())
            .collect::<HashSet<_>>();
        let workflows = projection
            .workflows
            .iter()
            .enumerate()
            .map(|(index, workflow)| (workflow.turn_id.clone(), index))
            .collect::<HashMap<_, _>>();

        let mut turns = HashMap::<TurnId, TurnBuilder>::new();
        let mut session_segments = Vec::new();
        for (item_index, item) in projection.transcript.iter().enumerate() {
            let turn_id = item_turn_id(item, &run_turns);
            if let Some(turn_id) = turn_id {
                turns
                    .entry(turn_id.clone())
                    .or_insert_with(|| TurnBuilder::new(turn_id, item.seq()))
                    .push(item_index, item);
            } else {
                session_segments.push(SegmentRecord {
                    id: TimelineSegmentId(format!("session:{}", item.seq())),
                    turn_id: None,
                    start_seq: item.seq(),
                    latest_seq: item.seq(),
                    state: TimelineTurnState::Complete,
                    item_indices: vec![item_index],
                    workflow_index: None,
                    estimated_bytes: estimated_item_bytes(item),
                });
            }
        }

        let mut segments = turns
            .into_values()
            .map(|builder| {
                let workflow_index = workflows.get(&builder.turn_id).copied();
                let state = if active_turns.contains(&builder.turn_id) {
                    TimelineTurnState::Active
                } else {
                    TimelineTurnState::Complete
                };
                builder.finish(state, workflow_index, projection)
            })
            .chain(session_segments)
            .collect::<Vec<_>>();
        segments.sort_by(|left, right| {
            left.start_seq
                .cmp(&right.start_seq)
                .then_with(|| left.id.0.cmp(&right.id.0))
        });
        let mut active_turn_ids = active_turns.into_iter().collect::<Vec<_>>();
        active_turn_ids.sort_by_key(|turn_id| turn_id.0);

        Self {
            session_id: projection.metadata.id.clone(),
            cursor,
            projection_revision: projection.revision,
            projection,
            segments,
            live: TimelineLiveState {
                lifecycle: projection.lifecycle,
                active_turns: active_turn_ids,
                interactions: projection.interactions.clone(),
                resources: projection.resources.clone(),
            },
        }
    }

    pub(crate) fn tail(&self, budget: &SessionTimelineBudget) -> SessionTimelinePage {
        self.page(Window::Tail, budget, true)
    }

    pub(crate) fn before(
        &self,
        cursor: TimelineCursor,
        budget: &SessionTimelineBudget,
    ) -> SessionTimelinePage {
        self.page(Window::Before(cursor), budget, false)
    }

    pub(crate) fn after(
        &self,
        cursor: TimelineCursor,
        budget: &SessionTimelineBudget,
    ) -> SessionTimelinePage {
        self.page(Window::After(cursor), budget, true)
    }

    pub(crate) fn around(
        &self,
        item_id: TimelineItemId,
        budget: &SessionTimelineBudget,
    ) -> anyhow::Result<SessionTimelinePage> {
        anyhow::ensure!(
            self.detail(&item_id).is_some(),
            "timeline item not found: {}",
            item_id.0
        );
        Ok(self.page(Window::Around(item_id), budget, true))
    }

    pub(crate) fn detail(&self, item_id: &TimelineItemId) -> Option<&TranscriptItem> {
        self.projection
            .transcript
            .iter()
            .find(|item| item_id_for(item) == *item_id)
    }

    fn page(
        &self,
        window: Window,
        budget: &SessionTimelineBudget,
        include_live: bool,
    ) -> SessionTimelinePage {
        let anchor = match &window {
            Window::Tail => self.segments.len(),
            Window::Before(cursor) | Window::After(cursor) => {
                self.segment_index(&cursor.item_id, cursor.seq)
            }
            Window::Around(item_id) => self.segment_index(item_id, 0),
        };
        let available = match window {
            Window::Tail => (0, self.segments.len(), Direction::Backward),
            Window::Before(_) => (0, anchor, Direction::Backward),
            Window::After(_) => (
                anchor.saturating_add(1).min(self.segments.len()),
                self.segments.len(),
                Direction::Forward,
            ),
            Window::Around(_) => (0, self.segments.len(), Direction::Around(anchor)),
        };
        let (start, end) = select_range(&self.segments, available, budget);
        let segments = self.segments[start..end]
            .iter()
            .map(|segment| self.materialize_segment(segment))
            .collect::<Vec<_>>();
        let serialized_bytes = segments
            .iter()
            .map(|segment| serde_json::to_vec(segment).map_or(0, |bytes| bytes.len() as u64))
            .sum();
        SessionTimelinePage {
            session_id: self.session_id.clone(),
            as_of_cursor: self.cursor,
            projection_revision: self.projection_revision,
            segments,
            older: TimelineRemaining {
                has_more: start > 0,
                estimated_segments: (start > 0).then_some(start as u64),
            },
            newer: TimelineRemaining {
                has_more: end < self.segments.len(),
                estimated_segments: (end < self.segments.len())
                    .then_some((self.segments.len() - end) as u64),
            },
            serialized_bytes,
            live: include_live.then(|| self.live.clone()),
        }
    }

    fn segment_index(&self, item_id: &TimelineItemId, seq: u64) -> usize {
        self.segments
            .iter()
            .position(|segment| {
                segment
                    .item_indices
                    .iter()
                    .any(|index| item_id_for(&self.projection.transcript[*index]) == *item_id)
            })
            .or_else(|| {
                self.segments
                    .iter()
                    .position(|segment| segment.latest_seq >= seq)
            })
            .unwrap_or(self.segments.len())
    }

    fn materialize_segment(&self, record: &SegmentRecord) -> TimelineSegment {
        let items = record
            .item_indices
            .iter()
            .map(|index| {
                let source = &self.projection.transcript[*index];
                preview_item(source, item_id_for(source), record.turn_id.clone())
            })
            .collect();
        match &record.turn_id {
            Some(turn_id) => TimelineSegment::Turn {
                segment: TimelineTurnSegment {
                    id: record.id.clone(),
                    turn_id: turn_id.clone(),
                    start_seq: record.start_seq,
                    latest_seq: record.latest_seq,
                    revision: Revision(record.latest_seq),
                    state: record.state,
                    items,
                    workflow: record
                        .workflow_index
                        .map(|index| self.projection.workflows[index].clone()),
                },
            },
            None => TimelineSegment::Session {
                segment: TimelineSessionSegment {
                    id: record.id.clone(),
                    start_seq: record.start_seq,
                    latest_seq: record.latest_seq,
                    revision: Revision(record.latest_seq),
                    items,
                },
            },
        }
    }
}

struct SegmentRecord {
    id: TimelineSegmentId,
    turn_id: Option<TurnId>,
    start_seq: u64,
    latest_seq: u64,
    state: TimelineTurnState,
    item_indices: Vec<usize>,
    workflow_index: Option<usize>,
    estimated_bytes: usize,
}

struct TurnBuilder {
    turn_id: TurnId,
    start_seq: u64,
    latest_seq: u64,
    item_indices: Vec<usize>,
    estimated_bytes: usize,
}

impl TurnBuilder {
    fn new(turn_id: TurnId, seq: u64) -> Self {
        Self {
            turn_id,
            start_seq: seq,
            latest_seq: seq,
            item_indices: Vec::new(),
            estimated_bytes: 0,
        }
    }

    fn push(&mut self, item_index: usize, item: &TranscriptItem) {
        self.start_seq = self.start_seq.min(item.seq());
        self.latest_seq = self.latest_seq.max(item.seq());
        self.item_indices.push(item_index);
        self.estimated_bytes = self
            .estimated_bytes
            .saturating_add(estimated_item_bytes(item));
    }

    fn finish(
        self,
        state: TimelineTurnState,
        workflow_index: Option<usize>,
        projection: &SessionProjection,
    ) -> SegmentRecord {
        let workflow_bytes = workflow_index
            .and_then(|index| serde_json::to_vec(&projection.workflows[index]).ok())
            .map_or(0, |bytes| bytes.len());
        SegmentRecord {
            id: TimelineSegmentId(format!("turn:{}", self.turn_id.0)),
            turn_id: Some(self.turn_id),
            start_seq: self.start_seq,
            latest_seq: self.latest_seq,
            state,
            item_indices: self.item_indices,
            workflow_index,
            estimated_bytes: self.estimated_bytes.saturating_add(workflow_bytes),
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
    Around(usize),
}

fn select_range(
    segments: &[SegmentRecord],
    (available_start, available_end, direction): (usize, usize, Direction),
    budget: &SessionTimelineBudget,
) -> (usize, usize) {
    if available_start >= available_end {
        return (available_start, available_start);
    }
    let turn_budget = budget
        .turn_budget
        .map_or(DEFAULT_TURN_BUDGET, |value| value.max(1) as usize);
    let byte_budget = budget
        .byte_budget
        .map_or(DEFAULT_BYTE_BUDGET, |value| value.max(1) as usize);
    match direction {
        Direction::Forward => grow_forward(
            segments,
            available_start,
            available_end,
            turn_budget,
            byte_budget,
        ),
        Direction::Backward => grow_backward(
            segments,
            available_start,
            available_end,
            turn_budget,
            byte_budget,
        ),
        Direction::Around(anchor) => grow_around(
            segments,
            available_start,
            available_end,
            anchor.min(available_end - 1),
            turn_budget,
            byte_budget,
        ),
    }
}

fn grow_forward(
    segments: &[SegmentRecord],
    start: usize,
    limit: usize,
    turn_budget: usize,
    byte_budget: usize,
) -> (usize, usize) {
    let mut end = start;
    let mut turns = 0;
    let mut bytes = 0;
    while end < limit {
        let next_turns = turns + usize::from(segments[end].turn_id.is_some());
        let next_bytes = bytes + segments[end].estimated_bytes;
        if end > start && (next_turns > turn_budget || next_bytes > byte_budget) {
            break;
        }
        turns = next_turns;
        bytes = next_bytes;
        end += 1;
    }
    (start, end)
}

fn grow_backward(
    segments: &[SegmentRecord],
    limit: usize,
    end: usize,
    turn_budget: usize,
    byte_budget: usize,
) -> (usize, usize) {
    let mut start = end;
    let mut turns = 0;
    let mut bytes = 0;
    while start > limit {
        let candidate = start - 1;
        let next_turns = turns + usize::from(segments[candidate].turn_id.is_some());
        let next_bytes = bytes + segments[candidate].estimated_bytes;
        if start < end && (next_turns > turn_budget || next_bytes > byte_budget) {
            break;
        }
        turns = next_turns;
        bytes = next_bytes;
        start = candidate;
    }
    (start, end)
}

fn grow_around(
    segments: &[SegmentRecord],
    limit: usize,
    end_limit: usize,
    anchor: usize,
    turn_budget: usize,
    byte_budget: usize,
) -> (usize, usize) {
    let (mut start, mut end) = (anchor, anchor + 1);
    let mut turns = usize::from(segments[anchor].turn_id.is_some());
    let mut bytes = segments[anchor].estimated_bytes;
    let mut take_left = true;
    loop {
        let candidate = match (start > limit, end < end_limit) {
            (true, true) => {
                let candidate = if take_left { start - 1 } else { end };
                take_left = !take_left;
                Some(candidate)
            }
            (true, false) => Some(start - 1),
            (false, true) => Some(end),
            (false, false) => None,
        };
        let Some(candidate) = candidate else { break };
        let next_turns = turns + usize::from(segments[candidate].turn_id.is_some());
        let next_bytes = bytes + segments[candidate].estimated_bytes;
        if next_turns > turn_budget || next_bytes > byte_budget {
            break;
        }
        turns = next_turns;
        bytes = next_bytes;
        if candidate < start {
            start = candidate;
        } else {
            end += 1;
        }
    }
    (start, end)
}

fn preview_item(
    source: &TranscriptItem,
    id: TimelineItemId,
    turn_id: Option<TurnId>,
) -> TimelineItem {
    let mut preview = source.clone();
    let has_detail = truncate_transcript_item(&mut preview);
    TimelineItem {
        id: id.clone(),
        seq: source.seq(),
        turn_id,
        run_id: item_run_id(source),
        kind: item_kind(source),
        preview,
        detail: has_detail.then_some(TimelineDetailRef { item_id: id }),
    }
}

fn truncate_transcript_item(item: &mut TranscriptItem) -> bool {
    let mut changed = false;
    match item {
        TranscriptItem::Message { message, .. } => {
            for part in &mut message.parts {
                let text = match part {
                    MessagePart::ContextRecord { content, .. } => Some(content),
                    MessagePart::CompactSummary { summary, .. } => Some(summary),
                    MessagePart::Text { text } => Some(text),
                    MessagePart::Thinking { thinking } => Some(thinking),
                    MessagePart::ToolResult { content, .. } => Some(content),
                    MessagePart::Image { .. } | MessagePart::ToolUse { .. } => None,
                };
                changed |= text.is_some_and(truncate_text);
            }
        }
        TranscriptItem::Diff {
            old_content,
            new_content,
            unified_diff,
            ..
        } => {
            changed |= old_content.as_mut().is_some_and(truncate_text);
            changed |= new_content.as_mut().is_some_and(truncate_text);
            changed |= unified_diff.as_mut().is_some_and(truncate_text);
        }
        TranscriptItem::Compaction { summary, .. } => changed |= truncate_text(summary),
        TranscriptItem::Mermaid { source, .. } => changed |= truncate_text(source),
        TranscriptItem::Notice { text, .. } => changed |= truncate_text(text),
        TranscriptItem::Extension { payload, .. } => {
            if serde_json::to_vec(payload).is_ok_and(|bytes| bytes.len() > MAX_PREVIEW_TEXT_BYTES) {
                *payload = serde_json::json!({"truncated": true});
                changed = true;
            }
        }
        TranscriptItem::FileEdit { .. } | TranscriptItem::ActivitySummary { .. } => {}
    }
    changed
}

fn truncate_text(text: &mut String) -> bool {
    if text.len() <= MAX_PREVIEW_TEXT_BYTES {
        return false;
    }
    let mut end = MAX_PREVIEW_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n…");
    true
}

fn item_id_for(item: &TranscriptItem) -> TimelineItemId {
    let suffix = match item {
        TranscriptItem::Message {
            checkpoint_index, ..
        } => checkpoint_index.map_or_else(String::new, |index| format!(":{index}")),
        _ => String::new(),
    };
    TimelineItemId(format!("{}:{}{suffix}", item_kind_name(item), item.seq()))
}

fn item_turn_id(item: &TranscriptItem, run_turns: &HashMap<FlowRunId, TurnId>) -> Option<TurnId> {
    match item {
        TranscriptItem::Message { message, .. } => Some(message.turn_id.clone()),
        TranscriptItem::FileEdit { turn_id, .. } => turn_id.clone(),
        TranscriptItem::ActivitySummary { turn_id, .. } => Some(turn_id.clone()),
        _ => item_run_id(item).and_then(|run_id| run_turns.get(&run_id).cloned()),
    }
}

fn item_run_id(item: &TranscriptItem) -> Option<FlowRunId> {
    match item {
        TranscriptItem::Message { run_id, .. }
        | TranscriptItem::Diff { run_id, .. }
        | TranscriptItem::FileEdit { run_id, .. }
        | TranscriptItem::Compaction { run_id, .. } => run_id.clone(),
        TranscriptItem::ActivitySummary { .. }
        | TranscriptItem::Mermaid { .. }
        | TranscriptItem::Notice { .. }
        | TranscriptItem::Extension { .. } => None,
    }
}

fn item_kind(item: &TranscriptItem) -> TimelineItemKind {
    match item {
        TranscriptItem::Message { .. } => TimelineItemKind::Message,
        TranscriptItem::Diff { .. } => TimelineItemKind::Diff,
        TranscriptItem::FileEdit { .. } => TimelineItemKind::FileEdit,
        TranscriptItem::ActivitySummary { .. } => TimelineItemKind::ActivitySummary,
        TranscriptItem::Compaction { .. } => TimelineItemKind::Compaction,
        TranscriptItem::Mermaid { .. } => TimelineItemKind::Mermaid,
        TranscriptItem::Notice { .. } => TimelineItemKind::Notice,
        TranscriptItem::Extension { .. } => TimelineItemKind::Extension,
    }
}

fn item_kind_name(item: &TranscriptItem) -> &'static str {
    match item_kind(item) {
        TimelineItemKind::Message => "message",
        TimelineItemKind::Diff => "diff",
        TimelineItemKind::FileEdit => "file_edit",
        TimelineItemKind::ActivitySummary => "activity",
        TimelineItemKind::Compaction => "compaction",
        TimelineItemKind::Mermaid => "mermaid",
        TimelineItemKind::Notice => "notice",
        TimelineItemKind::Extension => "extension",
    }
}

fn run_is_terminal(state: RunLifecycle) -> bool {
    matches!(
        state,
        RunLifecycle::Cancelled
            | RunLifecycle::Succeeded
            | RunLifecycle::Failed
            | RunLifecycle::Lost
    )
}

#[cfg(test)]
fn segment_start_seq(segment: &TimelineSegment) -> u64 {
    match segment {
        TimelineSegment::Turn { segment } => segment.start_seq,
        TimelineSegment::Session { segment } => segment.start_seq,
    }
}

#[cfg(test)]
fn segment_items(segment: &TimelineSegment) -> &[TimelineItem] {
    match segment {
        TimelineSegment::Turn { segment } => &segment.items,
        TimelineSegment::Session { segment } => &segment.items,
    }
}

fn estimated_item_bytes(item: &TranscriptItem) -> usize {
    let mut preview = item.clone();
    truncate_transcript_item(&mut preview);
    serde_json::to_vec(&preview)
        .map_or(0, |bytes| bytes.len())
        .saturating_add(256)
}

#[cfg(test)]
mod tests {
    use atman_proto::{
        MessageOrigin, MessageProjection, MessageRole, SessionLifecycle, SessionTimelineBudget,
        TimelineSegment, TranscriptItem,
    };

    use super::*;

    fn turn(value: u128) -> TurnId {
        TurnId(uuid::Uuid::from_u128(value))
    }

    fn message(seq: u64, turn_id: TurnId, text: &str) -> TranscriptItem {
        TranscriptItem::Message {
            seq,
            ts: chrono::Utc::now(),
            run_id: None,
            context_id: None,
            checkpoint_index: None,
            message: MessageProjection {
                role: MessageRole::User,
                origin: MessageOrigin::User,
                turn_id,
                parts: vec![MessagePart::Text { text: text.into() }],
            },
        }
    }

    fn projection(items: Vec<TranscriptItem>) -> SessionProjection {
        let session_id = SessionId(uuid::Uuid::from_u128(99));
        let mut projection = crate::projection::SessionProjector::new(session_id, None).snapshot();
        projection.lifecycle = SessionLifecycle::Idle;
        projection.revision = Revision(42);
        projection.transcript = items;
        projection
    }

    #[test]
    fn groups_interleaved_items_by_explicit_turn_identity() {
        let first = turn(1);
        let second = turn(2);
        let projection = projection(vec![
            message(1, first.clone(), "first"),
            message(2, second.clone(), "second"),
            message(3, first.clone(), "late first"),
        ]);

        let catalog = TimelineCatalog::from_projection(EventCursor(42), &projection);
        let page = catalog.tail(&SessionTimelineBudget {
            turn_budget: None,
            byte_budget: None,
        });

        assert_eq!(page.segments.len(), 2);
        let TimelineSegment::Turn { segment } = &page.segments[0] else {
            panic!("expected a turn segment");
        };
        assert_eq!(segment.turn_id, first);
        assert_eq!(segment.latest_seq, 3);
        assert_eq!(segment.items.len(), 2);
    }

    #[test]
    fn tail_keeps_turns_whole_and_reports_older_segments() {
        let projection = projection(vec![
            message(1, turn(1), "one"),
            message(2, turn(1), "still one"),
            message(3, turn(2), "two"),
            message(4, turn(3), "three"),
        ]);
        let catalog = TimelineCatalog::from_projection(EventCursor(42), &projection);

        let page = catalog.tail(&SessionTimelineBudget {
            turn_budget: Some(2),
            byte_budget: None,
        });

        assert_eq!(page.segments.len(), 2);
        assert!(page.older.has_more);
        assert!(!page.newer.has_more);
        assert_eq!(segment_start_seq(&page.segments[0]), 3);
    }

    #[test]
    fn oversized_text_uses_a_stable_detail_reference() {
        let original = "界".repeat(MAX_PREVIEW_TEXT_BYTES);
        let projection = projection(vec![message(1, turn(1), &original)]);
        let catalog = TimelineCatalog::from_projection(EventCursor(42), &projection);
        let page = catalog.tail(&SessionTimelineBudget {
            turn_budget: None,
            byte_budget: Some(1),
        });
        let item = &segment_items(&page.segments[0])[0];

        assert!(item.detail.is_some());
        assert!(catalog.detail(&item.id).is_some());
        assert!(page.serialized_bytes > 0);
        assert_eq!(page.segments.len(), 1);
    }

    #[test]
    fn directional_pages_are_exclusive_and_around_is_anchored() {
        let projection = projection(vec![
            message(1, turn(1), "one"),
            message(2, turn(2), "two"),
            message(3, turn(3), "three"),
        ]);
        let catalog = TimelineCatalog::from_projection(EventCursor(42), &projection);
        let all = catalog.tail(&SessionTimelineBudget {
            turn_budget: None,
            byte_budget: None,
        });
        let cursors = all
            .segments
            .iter()
            .map(|segment| {
                let item = &segment_items(segment)[0];
                TimelineCursor {
                    seq: item.seq,
                    item_id: item.id.clone(),
                }
            })
            .collect::<Vec<_>>();
        let one_turn = SessionTimelineBudget {
            turn_budget: Some(1),
            byte_budget: None,
        };

        let before = catalog.before(cursors[2].clone(), &one_turn);
        let after = catalog.after(cursors[0].clone(), &one_turn);
        let around = catalog
            .around(cursors[1].item_id.clone(), &one_turn)
            .unwrap();

        assert_eq!(segment_start_seq(&before.segments[0]), 2);
        assert_eq!(segment_start_seq(&after.segments[0]), 2);
        assert_eq!(segment_start_seq(&around.segments[0]), 2);
        assert!(
            catalog
                .around(TimelineItemId("missing".into()), &one_turn)
                .is_err()
        );
    }
}
