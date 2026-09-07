use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use atman_proto::{
    DaemonGeneration, EventCursor, FlowRunId, MessagePart, Revision, RunLifecycle, SessionId,
    SessionProjection, SessionTimelineBudget, SessionTimelineItemDetail, SessionTimelinePage,
    TimelineCursor, TimelineDetailRef, TimelineItem, TimelineItemId, TimelineItemKind,
    TimelineLiveState, TimelineRemaining, TimelineSegment, TimelineSegmentId,
    TimelineSessionSegment, TimelineTurnSegment, TimelineTurnState, TranscriptItem, TurnId,
};
use atman_runtime::projection::message_window::FlowOwnership;

const DEFAULT_TURN_BUDGET: usize = 12;
const DEFAULT_BYTE_BUDGET: usize = 256 * 1024;
const MAX_PREVIEW_TEXT_BYTES: usize = 8 * 1024;

pub(crate) struct TimelineCatalog<'a> {
    session_id: SessionId,
    daemon_generation: DaemonGeneration,
    cursor: EventCursor,
    projection_revision: Revision,
    projection: &'a SessionProjection,
    segments: Vec<SegmentRecord>,
    live: TimelineLiveState,
}

enum Window {
    Head,
    Tail,
    Before(TimelineCursor),
    After(TimelineCursor),
    Around(TimelineCursor),
}

impl<'a> TimelineCatalog<'a> {
    pub(crate) fn from_projection(
        daemon_generation: DaemonGeneration,
        cursor: EventCursor,
        projection: &'a SessionProjection,
    ) -> Self {
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
        let active_workflows = projection
            .workflows
            .iter()
            .filter(|workflow| active_turn_ids.contains(&workflow.turn_id))
            .cloned()
            .collect();

        Self {
            session_id: projection.metadata.id.clone(),
            daemon_generation,
            cursor,
            projection_revision: projection.revision,
            projection,
            segments,
            live: TimelineLiveState {
                head_complete: true,
                metadata: projection.metadata.clone(),
                lifecycle: projection.lifecycle,
                runs: projection.runs.clone(),
                active_turns: active_turn_ids,
                active_workflows,
                compactions: projection.compactions.clone(),
                goal: projection.goal.clone(),
                todos: projection.todos.clone(),
                plans: projection.plans.clone(),
                context: projection.context.clone(),
                trust: projection.trust.clone(),
                interactions: projection.interactions.clone(),
                resources: projection.resources.clone(),
                usage: projection.usage.clone(),
            },
        }
    }

    pub(crate) fn tail(&self, budget: &SessionTimelineBudget) -> SessionTimelinePage {
        self.page(Window::Tail, budget, true)
    }

    pub(crate) fn head(&self, budget: &SessionTimelineBudget) -> SessionTimelinePage {
        self.page(Window::Head, budget, false)
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
        anchor: TimelineCursor,
        budget: &SessionTimelineBudget,
    ) -> SessionTimelinePage {
        self.page(Window::Around(anchor), budget, true)
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
            Window::Head => 0,
            Window::Tail => self.segments.len(),
            Window::Before(cursor) => self.segment_index(&cursor.item_id, cursor.seq),
            Window::After(cursor) => self.segment_index(&cursor.item_id, cursor.seq),
            Window::Around(cursor) => self.segment_index(&cursor.item_id, cursor.seq),
        };
        let available = match window {
            Window::Head => (0, self.segments.len(), Direction::Forward),
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
            daemon_generation: self.daemon_generation.clone(),
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

pub(crate) enum IndexedPageWindow {
    Tail,
    Before(TimelineCursor),
    After(TimelineCursor),
    Around(TimelineCursor),
}

pub(crate) fn jsonl_page(
    source: &crate::run::JsonlTimelineSource,
    session_id: &SessionId,
    daemon_generation: DaemonGeneration,
    window: IndexedPageWindow,
    budget: &SessionTimelineBudget,
    include_live: bool,
) -> anyhow::Result<Option<SessionTimelinePage>> {
    let (before_seq, skip_current_turn, marks_newer) = match window {
        IndexedPageWindow::Tail => (None, false, false),
        IndexedPageWindow::Before(cursor) => (Some(cursor.seq), true, true),
        IndexedPageWindow::After(_) => anyhow::bail!("reverse JSONL after pages are unsupported"),
        IndexedPageWindow::Around(cursor) => (Some(cursor.seq.saturating_add(1)), false, true),
    };
    let turn_budget = budget
        .turn_budget
        .map_or(DEFAULT_TURN_BUDGET, |value| value.max(1) as usize);
    let byte_budget = budget
        .byte_budget
        .map_or(DEFAULT_BYTE_BUDGET, |value| value.max(1) as usize);
    let read = read_jsonl_turn_tail(
        &source.events_path,
        before_seq,
        skip_current_turn,
        turn_budget,
        byte_budget,
    )?;
    if read.events.is_empty() {
        return Ok(None);
    }
    let mut projector = crate::projection::SessionProjector::from_events(
        session_id.clone(),
        source.metadata.clone(),
        &read.events,
    );
    projector.set_trust(source.trust.clone());
    let projection = projector.snapshot();
    let mut catalog = TimelineCatalog::from_projection(
        daemon_generation,
        EventCursor(read.source_seq),
        &projection,
    );
    catalog.live.head_complete = false;
    let mut page = catalog.tail(budget);
    page.projection_revision = Revision(read.source_seq);
    page.older.has_more |= read.has_older;
    if page.older.has_more {
        page.older.estimated_segments = None;
    }
    if marks_newer {
        page.newer.has_more = true;
        page.newer.estimated_segments = None;
    }
    if !include_live {
        page.live = None;
    }
    Ok(Some(page))
}

struct JsonlTailRead {
    events: Vec<atman_runtime::event::EventEnvelope>,
    source_seq: u64,
    has_older: bool,
}

fn read_jsonl_turn_tail(
    path: &std::path::Path,
    before_seq: Option<u64>,
    mut skip_current_turn: bool,
    turn_budget: usize,
    byte_budget: usize,
) -> anyhow::Result<JsonlTailRead> {
    const BLOCK_BYTES: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path)?;
    let mut position = file.metadata()?.len();
    let mut carry = Vec::new();
    let mut reversed = Vec::new();
    let mut source_seq = 0;
    let mut turn_starts = 0;
    let mut selected_bytes = 0_usize;
    let mut has_older = false;

    'blocks: while position > 0 {
        let start = position.saturating_sub(BLOCK_BYTES);
        let mut block = vec![0; usize::try_from(position - start)?];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block)?;
        block.extend_from_slice(&carry);
        position = start;

        let complete_start = if start == 0 {
            0
        } else if let Some(newline) = block.iter().position(|byte| *byte == b'\n') {
            newline + 1
        } else {
            carry = block;
            continue;
        };
        let mut end = block.len();
        while end > complete_start {
            while end > complete_start && block[end - 1] == b'\n' {
                end -= 1;
            }
            if end == complete_start {
                break;
            }
            let line_start = block[complete_start..end]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(complete_start, |offset| complete_start + offset + 1);
            let line = std::str::from_utf8(&block[line_start..end])?.trim();
            end = line_start.saturating_sub(1);
            if line.is_empty() {
                continue;
            }
            let Ok(envelope) = serde_json::from_str::<atman_runtime::event::EventEnvelope>(line)
            else {
                continue;
            };
            source_seq = source_seq.max(envelope.seq);
            if before_seq.is_some_and(|before| envelope.seq >= before) {
                continue;
            }
            let starts_turn = matches!(
                envelope.event,
                atman_runtime::event::Event::TurnStart { .. }
            );
            if skip_current_turn {
                if starts_turn {
                    skip_current_turn = false;
                }
                continue;
            }
            selected_bytes = selected_bytes.saturating_add(line.len());
            reversed.push(envelope);
            if starts_turn {
                turn_starts += 1;
                if turn_starts >= turn_budget || selected_bytes >= byte_budget {
                    has_older = reversed.last().is_some_and(|event| event.seq > 1);
                    break 'blocks;
                }
            }
        }
        carry = block[..complete_start].to_vec();
    }

    reversed.reverse();
    Ok(JsonlTailRead {
        events: reversed,
        source_seq,
        has_older,
    })
}

pub(crate) fn indexed_page(
    source: &crate::run::IndexedTimelineSource,
    session_id: &SessionId,
    daemon_generation: DaemonGeneration,
    window: IndexedPageWindow,
    budget: &SessionTimelineBudget,
    include_live: bool,
) -> anyhow::Result<Option<SessionTimelinePage>> {
    let session_key = session_id.to_string();
    let turn_budget = budget
        .turn_budget
        .map_or(DEFAULT_TURN_BUDGET, |value| value.max(1) as usize);
    let (mut turns, mut has_older, mut has_newer, include_unowned_through) = match &window {
        IndexedPageWindow::Tail => (
            source
                .index
                .read_turns_before(&session_key, None, turn_budget.saturating_add(1))?,
            false,
            false,
            source.coverage.seq,
        ),
        IndexedPageWindow::Before(cursor) => {
            let start = source
                .index
                .turn_start_for_event(&session_key, cursor.seq)?
                .unwrap_or(cursor.seq);
            let turns = source.index.read_turns_before(
                &session_key,
                Some(start),
                turn_budget.saturating_add(1),
            )?;
            (turns, false, true, start.saturating_sub(1))
        }
        IndexedPageWindow::After(cursor) => {
            let start = source
                .index
                .turn_start_for_event(&session_key, cursor.seq)?
                .unwrap_or(cursor.seq);
            let turns = source.index.read_turns_after(
                &session_key,
                start,
                turn_budget.saturating_add(1),
            )?;
            let through = turns
                .iter()
                .map(|turn| turn.latest_seq)
                .max()
                .unwrap_or(start);
            (turns, true, false, through)
        }
        IndexedPageWindow::Around(cursor) => {
            let start = source
                .index
                .turn_start_for_event(&session_key, cursor.seq)?
                .unwrap_or(cursor.seq);
            let turns = source.index.read_turns_before(
                &session_key,
                Some(start.saturating_add(1)),
                turn_budget.saturating_add(1),
            )?;
            let has_newer = !source
                .index
                .read_turns_after(&session_key, start, 1)?
                .is_empty();
            (turns, false, has_newer, cursor.seq)
        }
    };
    has_older |= !matches!(window, IndexedPageWindow::After(_))
        && (turns.len() > turn_budget || source.coverage.start_seq > 1);
    has_newer |= matches!(window, IndexedPageWindow::After(_)) && turns.len() > turn_budget;
    turns.retain(|turn| turn.start_seq >= source.coverage.start_seq);
    turns.truncate(turn_budget);
    let byte_budget = budget
        .byte_budget
        .map_or(DEFAULT_BYTE_BUDGET as u64, |value| value.max(1));
    let latest_selected_seq = turns.iter().map(|turn| turn.latest_seq).max();
    let mut selected_bytes = 0_u64;
    let mut selected_turns = 0;
    for turn in &turns {
        let through_seq = if Some(turn.latest_seq) == latest_selected_seq {
            include_unowned_through
        } else {
            turn.latest_seq
        };
        let turn_bytes = source
            .index
            .estimate_turn_event_bytes(&session_key, turn, through_seq)?;
        if selected_turns > 0 && selected_bytes.saturating_add(turn_bytes) > byte_budget {
            break;
        }
        selected_bytes = selected_bytes.saturating_add(turn_bytes);
        selected_turns += 1;
    }
    if selected_turns < turns.len() {
        if matches!(window, IndexedPageWindow::After(_)) {
            has_newer = true;
        } else {
            has_older = true;
        }
        turns.truncate(selected_turns.max(1));
    }
    if turns.is_empty() {
        return Ok(None);
    }
    let oldest_selected_turn = turns.iter().map(|turn| turn.start_seq).min();
    let rows =
        source
            .index
            .read_events_for_turns(&session_key, &turns, Some(include_unowned_through))?;
    let events = rows
        .into_iter()
        .map(|row| {
            let event = serde_json::from_str::<atman_runtime::event::EventEnvelope>(&row.payload)?;
            anyhow::ensure!(
                event.seq == row.seq,
                "timeline index sequence mismatch for session {session_id}: row {} != payload {}",
                row.seq,
                event.seq
            );
            Ok(event)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let ownership = indexed_flow_ownership_before(
        source,
        &session_key,
        events.first().map_or(0, |event| event.seq),
    )?;
    let mut projector = crate::projection::SessionProjector::from_events_with_ownership(
        session_id.clone(),
        source.metadata.clone(),
        &events,
        ownership,
    );
    projector.set_trust(source.trust.clone());
    let projection = projector.snapshot();
    let mut catalog = TimelineCatalog::from_projection(
        daemon_generation,
        EventCursor(source.coverage.seq),
        &projection,
    );
    catalog.live.head_complete = false;
    let mut page = if matches!(window, IndexedPageWindow::After(_)) {
        catalog.head(budget)
    } else {
        catalog.tail(budget)
    };
    page.projection_revision = Revision(source.coverage.seq);
    page.older.has_more |= has_older;
    if page.older.has_more {
        page.older.estimated_segments = if source.coverage.start_seq == 1 {
            let indexed_before = source
                .index
                .count_turns_before(&session_key, oldest_selected_turn)?;
            Some(indexed_before.saturating_add(page.older.estimated_segments.unwrap_or_default()))
        } else {
            None
        };
    }
    page.newer.has_more |= has_newer;
    if page.newer.has_more {
        page.newer.estimated_segments = None;
    }
    if !include_live {
        page.live = None;
    }
    Ok(Some(page))
}

pub(crate) fn indexed_detail(
    source: &crate::run::IndexedTimelineSource,
    session_id: &SessionId,
    item_id: &TimelineItemId,
) -> anyhow::Result<Option<SessionTimelineItemDetail>> {
    let Some(seq) = item_seq(item_id) else {
        return Ok(None);
    };
    let Some(row) = source
        .index
        .read_event_at_seq(&session_id.to_string(), seq)?
    else {
        return Ok(None);
    };
    let event = serde_json::from_str::<atman_runtime::event::EventEnvelope>(&row.payload)?;
    let ownership = indexed_flow_ownership_before(source, &session_id.to_string(), seq)?;
    let mut projector = crate::projection::SessionProjector::from_events_with_ownership(
        session_id.clone(),
        source.metadata.clone(),
        &[event],
        ownership,
    );
    projector.set_trust(source.trust.clone());
    let projection = projector.snapshot();
    let catalog = TimelineCatalog::from_projection(
        DaemonGeneration(String::new()),
        EventCursor(source.coverage.seq),
        &projection,
    );
    Ok(catalog
        .detail(item_id)
        .cloned()
        .map(|item| SessionTimelineItemDetail {
            session_id: session_id.clone(),
            item_id: item_id.clone(),
            item,
        }))
}

pub(crate) fn jsonl_detail(
    source: &crate::run::JsonlTimelineSource,
    session_id: &SessionId,
    item_id: &TimelineItemId,
) -> anyhow::Result<Option<SessionTimelineItemDetail>> {
    let Some(seq) = item_seq(item_id) else {
        return Ok(None);
    };
    let read = read_jsonl_turn_tail(
        &source.events_path,
        Some(seq.saturating_add(1)),
        false,
        1,
        usize::MAX,
    )?;
    let mut projector = crate::projection::SessionProjector::from_events(
        session_id.clone(),
        source.metadata.clone(),
        &read.events,
    );
    projector.set_trust(source.trust.clone());
    let projection = projector.snapshot();
    let catalog = TimelineCatalog::from_projection(
        DaemonGeneration(String::new()),
        EventCursor(read.source_seq),
        &projection,
    );
    Ok(catalog
        .detail(item_id)
        .cloned()
        .map(|item| SessionTimelineItemDetail {
            session_id: session_id.clone(),
            item_id: item_id.clone(),
            item,
        }))
}

fn item_seq(item_id: &TimelineItemId) -> Option<u64> {
    item_id.0.split(':').nth(1)?.parse().ok()
}

fn indexed_flow_ownership_before(
    source: &crate::run::IndexedTimelineSource,
    session_id: &str,
    through_seq: u64,
) -> anyhow::Result<FlowOwnership> {
    const PAGE_SIZE: usize = 256;
    let mut ownership = FlowOwnership::default();
    let mut before = Some(through_seq);
    loop {
        let rows = source.index.read_events_before_descending(
            session_id,
            before,
            PAGE_SIZE,
            atman_runtime::index::EventFilter::Kinds(&["flow_start"]),
        )?;
        if rows.is_empty() {
            break;
        }
        before = rows.last().map(|row| row.seq);
        let page_len = rows.len();
        for row in rows {
            let event = serde_json::from_str::<atman_runtime::event::EventEnvelope>(&row.payload)?;
            ownership.observe(&event.event);
        }
        if page_len < PAGE_SIZE {
            break;
        }
    }
    Ok(ownership)
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
    let text_bytes = match item {
        TranscriptItem::Message { message, .. } => message
            .parts
            .iter()
            .map(|part| match part {
                MessagePart::ContextRecord { key, content, .. } => key
                    .len()
                    .saturating_add(content.len().min(MAX_PREVIEW_TEXT_BYTES)),
                MessagePart::CompactSummary { summary, .. } => {
                    summary.len().min(MAX_PREVIEW_TEXT_BYTES)
                }
                MessagePart::Text { text } => text.len().min(MAX_PREVIEW_TEXT_BYTES),
                MessagePart::Thinking { thinking } => thinking.len().min(MAX_PREVIEW_TEXT_BYTES),
                MessagePart::Image {
                    media_type,
                    artifact_id,
                    name,
                    ..
                } => {
                    media_type.len()
                        + artifact_id.as_ref().map_or(0, String::len)
                        + name.as_ref().map_or(0, String::len)
                }
                MessagePart::ToolUse {
                    id, name, intent, ..
                } => id.len() + name.len() + intent.as_ref().map_or(0, String::len) + 512,
                MessagePart::ToolResult { content, .. } => {
                    content.len().min(MAX_PREVIEW_TEXT_BYTES)
                }
            })
            .sum(),
        TranscriptItem::Diff {
            title,
            old_content,
            new_content,
            unified_diff,
            ..
        } => {
            title.len()
                + old_content
                    .as_ref()
                    .map_or(0, |text| text.len().min(MAX_PREVIEW_TEXT_BYTES))
                + new_content
                    .as_ref()
                    .map_or(0, |text| text.len().min(MAX_PREVIEW_TEXT_BYTES))
                + unified_diff
                    .as_ref()
                    .map_or(0, |text| text.len().min(MAX_PREVIEW_TEXT_BYTES))
        }
        TranscriptItem::FileEdit {
            path, tool_name, ..
        } => path.len() + tool_name.len(),
        TranscriptItem::ActivitySummary {
            turn_files,
            session_files,
            ..
        } => turn_files
            .iter()
            .chain(session_files)
            .map(String::len)
            .sum(),
        TranscriptItem::Compaction { summary, .. } => summary.len().min(MAX_PREVIEW_TEXT_BYTES),
        TranscriptItem::Mermaid { source, .. } => source.len().min(MAX_PREVIEW_TEXT_BYTES),
        TranscriptItem::Notice { text, .. } => text.len().min(MAX_PREVIEW_TEXT_BYTES),
        TranscriptItem::Extension { kind, .. } => kind.len() + 512,
    };
    text_bytes.saturating_add(256)
}

#[cfg(test)]
mod tests {
    use atman_proto::{
        MessageOrigin, MessageProjection, MessageRole, SessionLifecycle, SessionTimelineBudget,
        TimelineSegment, TranscriptItem,
    };
    use atman_runtime::index::{AnchorIndex, EventIndexCoverage, ProjectEventInsert};

    use super::*;

    fn turn(value: u128) -> TurnId {
        TurnId(uuid::Uuid::from_u128(value))
    }

    fn message(seq: u64, turn_id: TurnId, text: &str) -> TranscriptItem {
        TranscriptItem::Message {
            seq,
            ts: chrono::Utc::now(),
            run_id: None,
            context_run_id: None,
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

    fn indexed_source(
        events: &[(atman_runtime::event::EventEnvelope, TurnId)],
    ) -> (tempfile::TempDir, crate::run::IndexedTimelineSource) {
        let dir = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(AnchorIndex::open_project(dir.path()).unwrap());
        for (event, turn_id) in events {
            let payload = serde_json::to_string(event).unwrap();
            let kind = match &event.event {
                atman_runtime::event::Event::FlowStart { .. } => "flow_start",
                atman_runtime::event::Event::AssistantMsg { .. } => "assistant_msg",
                _ => "user_msg",
            };
            index
                .insert_project_event_raw(ProjectEventInsert {
                    session_id: "00000000-0000-0000-0000-000000000063",
                    seq: i64::try_from(event.seq).unwrap(),
                    ts: &event.ts.to_rfc3339(),
                    kind,
                    turn_id: Some(&turn_id.0.to_string()),
                    flow_run_id: None,
                    text_content: "",
                    payload_json: &payload,
                })
                .unwrap();
        }
        index
            .materialize_timeline_session("00000000-0000-0000-0000-000000000063")
            .unwrap();
        let coverage = EventIndexCoverage {
            start_seq: 1,
            seq: events.last().unwrap().0.seq,
            line_start: 0,
            line_end: 0,
            log_offset: 0,
            line_digest: String::new(),
        };
        (
            dir,
            crate::run::IndexedTimelineSource {
                index,
                metadata: None,
                trust: atman_runtime::trust::TrustConfig::default(),
                coverage,
            },
        )
    }

    fn user_event(seq: u64, turn_id: &TurnId, text: &str) -> atman_runtime::event::EventEnvelope {
        let runtime_turn_id = atman_runtime::event::TurnId(turn_id.0);
        atman_runtime::event::EventEnvelope::new(
            seq,
            atman_runtime::event::Event::UserMsg {
                turn_id: runtime_turn_id.clone(),
                flow_run_id: None,
                message: atman_runtime::message::Message::user_text(runtime_turn_id, text),
            },
        )
    }

    #[test]
    fn reverse_jsonl_reader_keeps_recent_turns_without_parsing_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let turns = [turn(1), turn(2), turn(3)];
        let mut events = Vec::new();
        for (index, turn_id) in turns.iter().enumerate() {
            let start_seq = index as u64 * 2 + 1;
            events.push(atman_runtime::event::EventEnvelope::new(
                start_seq,
                atman_runtime::event::Event::TurnStart {
                    turn_id: atman_runtime::event::TurnId(turn_id.0),
                },
            ));
            events.push(user_event(start_seq + 1, turn_id, "message"));
        }
        let mut bytes = vec![b'x'; 128 * 1024];
        bytes.push(b'\n');
        for event in &events {
            bytes.extend_from_slice(serde_json::to_string(event).unwrap().as_bytes());
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(b"{\"type\":\"partial");
        std::fs::write(&path, bytes).unwrap();

        let read = read_jsonl_turn_tail(&path, None, false, 2, usize::MAX).unwrap();

        assert_eq!(read.source_seq, 6);
        assert!(read.has_older);
        assert_eq!(
            read.events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [3, 4, 5, 6]
        );

        let previous = read_jsonl_turn_tail(&path, Some(6), true, 1, usize::MAX).unwrap();
        assert_eq!(
            previous
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [3, 4]
        );

        let byte_limited = read_jsonl_turn_tail(&path, None, false, 3, 1).unwrap();
        assert!(byte_limited.has_older);
        assert_eq!(
            byte_limited
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [5, 6]
        );
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

        let catalog = TimelineCatalog::from_projection(
            DaemonGeneration("test".into()),
            EventCursor(42),
            &projection,
        );
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
    fn indexed_pages_are_provisional_and_page_by_complete_turn() {
        let session_id = SessionId(uuid::Uuid::from_u128(99));
        let first = turn(1);
        let second = turn(2);
        let events = vec![
            (user_event(1, &first, "first"), first),
            (user_event(2, &second, "second"), second),
        ];
        let (_dir, source) = indexed_source(&events);
        let budget = SessionTimelineBudget {
            turn_budget: Some(1),
            byte_budget: None,
        };

        let tail = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Tail,
            &budget,
            true,
        )
        .unwrap()
        .unwrap();
        assert!(!tail.live.as_ref().unwrap().head_complete);
        assert_eq!(tail.as_of_cursor, EventCursor(source.coverage.seq));
        assert!(tail.older.has_more);
        assert_eq!(tail.older.estimated_segments, Some(1));
        assert_eq!(segment_start_seq(&tail.segments[0]), 2);

        let newest = &segment_items(&tail.segments[0])[0];
        let before = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Before(TimelineCursor {
                seq: newest.seq,
                item_id: newest.id.clone(),
            }),
            &budget,
            false,
        )
        .unwrap()
        .unwrap();
        assert!(before.live.is_none());
        assert!(!before.older.has_more);
        assert_eq!(before.older.estimated_segments, None);
        assert_eq!(segment_start_seq(&before.segments[0]), 1);

        let oldest = &segment_items(&before.segments[0])[0];
        let around = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Around(TimelineCursor {
                seq: oldest.seq,
                item_id: TimelineItemId(format!("search:{}", oldest.seq)),
            }),
            &budget,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(segment_start_seq(&around.segments[0]), 1);
        assert!(around.newer.has_more);
        assert!(around.live.is_some());

        let after = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::After(TimelineCursor {
                seq: oldest.seq,
                item_id: oldest.id.clone(),
            }),
            &budget,
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(segment_start_seq(&after.segments[0]), 2);
        assert!(after.older.has_more);
        assert!(!after.newer.has_more);

        let detail = indexed_detail(&source, &session_id, &newest.id)
            .unwrap()
            .unwrap();
        assert_eq!(detail.item_id, newest.id);
        assert_eq!(detail.item, newest.preview);

        let byte_limited = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Tail,
            &SessionTimelineBudget {
                turn_budget: Some(2),
                byte_budget: Some(1),
            },
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(byte_limited.segments.len(), 1);
        assert_eq!(segment_start_seq(&byte_limited.segments[0]), 2);
        assert!(byte_limited.older.has_more);
    }

    #[test]
    fn indexed_before_excludes_the_turn_containing_an_unowned_checkpoint_cursor() {
        let session_id = SessionId(uuid::Uuid::from_u128(99));
        let first = turn(1);
        let second = turn(2);
        let events = vec![
            (user_event(1, &first, "first"), first),
            (user_event(2, &second, "second"), second.clone()),
        ];
        let (_dir, mut source) = indexed_source(&events);
        let runtime_turn_id = atman_runtime::event::TurnId(second.0);
        let checkpoint = atman_runtime::event::EventEnvelope::new(
            3,
            atman_runtime::event::Event::Checkpoint {
                session_id: session_id.to_string(),
                flow_run_id: None,
                messages: vec![atman_runtime::message::Message::user_text(
                    runtime_turn_id.clone(),
                    "checkpoint second",
                )],
                window_tokens: 4,
            },
        );
        let turn_end = atman_runtime::event::EventEnvelope::new(
            4,
            atman_runtime::event::Event::TurnEnd {
                turn_id: runtime_turn_id,
            },
        );
        for (event, kind, owner) in [
            (&checkpoint, "checkpoint", None),
            (&turn_end, "turn_end", Some(second.0.to_string())),
        ] {
            let payload = serde_json::to_string(event).unwrap();
            source
                .index
                .insert_project_event_raw(ProjectEventInsert {
                    session_id: &session_id.to_string(),
                    seq: i64::try_from(event.seq).unwrap(),
                    ts: &event.ts.to_rfc3339(),
                    kind,
                    turn_id: owner.as_deref(),
                    flow_run_id: None,
                    text_content: "",
                    payload_json: &payload,
                })
                .unwrap();
        }
        source
            .index
            .materialize_timeline_session(&session_id.to_string())
            .unwrap();
        source.coverage.seq = 4;
        let budget = SessionTimelineBudget {
            turn_budget: Some(1),
            byte_budget: Some(1),
        };

        let tail = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Tail,
            &budget,
            true,
        )
        .unwrap()
        .unwrap();
        let checkpoint_item = &segment_items(&tail.segments[0])[0];
        assert_eq!(checkpoint_item.seq, 3);

        let before = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Before(TimelineCursor {
                seq: checkpoint_item.seq,
                item_id: checkpoint_item.id.clone(),
            }),
            &budget,
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(segment_start_seq(&before.segments[0]), 1);
        assert!(
            before
                .segments
                .iter()
                .flat_map(segment_items)
                .all(|item| item.seq < checkpoint_item.seq)
        );
    }

    #[test]
    fn indexed_pages_exclude_turns_crossing_the_validated_suffix() {
        let session_id = SessionId(uuid::Uuid::from_u128(99));
        let incomplete = turn(1);
        let complete = turn(2);
        let events = vec![
            (user_event(1, &incomplete, "incomplete"), incomplete),
            (user_event(2, &complete, "complete"), complete),
        ];
        let (_dir, mut source) = indexed_source(&events);
        source.coverage.start_seq = 2;

        let page = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Tail,
            &SessionTimelineBudget {
                turn_budget: Some(2),
                byte_budget: None,
            },
            true,
        )
        .unwrap()
        .unwrap();
        assert!(page.older.has_more);
        assert_eq!(page.older.estimated_segments, None);
        assert_eq!(page.segments.len(), 1);
        assert_eq!(segment_start_seq(&page.segments[0]), 2);
    }

    #[test]
    fn indexed_pages_and_details_keep_spawned_context_identity() {
        let session_id = SessionId(uuid::Uuid::from_u128(99));
        let turn_id = turn(1);
        let child = atman_runtime::event::FlowRunId::now();
        let events = vec![
            (
                atman_runtime::event::EventEnvelope::new(
                    1,
                    atman_runtime::event::Event::FlowStart {
                        turn_id: Some(atman_runtime::event::TurnId(turn_id.0)),
                        run_id: child.clone(),
                        flow_name: "child".into(),
                        spawned: true,
                        parent_run_id: None,
                        parent_node_id: None,
                    },
                ),
                turn_id.clone(),
            ),
            (
                atman_runtime::event::EventEnvelope::new(
                    2,
                    atman_runtime::event::Event::AssistantMsg {
                        turn_id: atman_runtime::event::TurnId(turn_id.0),
                        flow_run_id: Some(child.clone()),
                        message: atman_runtime::message::Message::assistant_text(
                            atman_runtime::event::TurnId(turn_id.0),
                            "child output",
                        ),
                    },
                ),
                turn_id,
            ),
        ];
        let (_dir, source) = indexed_source(&events);
        let page = indexed_page(
            &source,
            &session_id,
            DaemonGeneration("test".into()),
            IndexedPageWindow::Tail,
            &SessionTimelineBudget {
                turn_budget: Some(1),
                byte_budget: None,
            },
            true,
        )
        .unwrap()
        .unwrap();
        let item = &segment_items(&page.segments[0])[0];
        let TranscriptItem::Message { context_run_id, .. } = &item.preview else {
            panic!("expected message");
        };
        assert_eq!(context_run_id.as_ref().map(|id| id.0), Some(child.0));

        let detail = indexed_detail(&source, &session_id, &item.id)
            .unwrap()
            .unwrap();
        let TranscriptItem::Message { context_run_id, .. } = detail.item else {
            panic!("expected message detail");
        };
        assert_eq!(context_run_id.map(|id| id.0), Some(child.0));
    }

    #[test]
    fn tail_keeps_turns_whole_and_reports_older_segments() {
        let projection = projection(vec![
            message(1, turn(1), "one"),
            message(2, turn(1), "still one"),
            message(3, turn(2), "two"),
            message(4, turn(3), "three"),
        ]);
        let catalog = TimelineCatalog::from_projection(
            DaemonGeneration("test".into()),
            EventCursor(42),
            &projection,
        );

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
        let catalog = TimelineCatalog::from_projection(
            DaemonGeneration("test".into()),
            EventCursor(42),
            &projection,
        );
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
        let catalog = TimelineCatalog::from_projection(
            DaemonGeneration("test".into()),
            EventCursor(42),
            &projection,
        );
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
        let around = catalog.around(cursors[1].clone(), &one_turn);
        let around_by_sequence = catalog.around(
            TimelineCursor {
                seq: cursors[1].seq,
                item_id: TimelineItemId(format!("search:{}", cursors[1].seq)),
            },
            &one_turn,
        );

        assert_eq!(segment_start_seq(&before.segments[0]), 2);
        assert_eq!(segment_start_seq(&after.segments[0]), 2);
        assert_eq!(segment_start_seq(&around.segments[0]), 2);
        assert_eq!(segment_start_seq(&around_by_sequence.segments[0]), 2);
    }
}
