//! Bounded body preparation; fold envelopes remain owned by the UI cache.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use super::*;
use crate::{
    app::OutputRevision,
    flow_view::{DocumentIdentity, OutputSnapshot},
    render_preferences::RenderPreferences,
    theme::Theme,
};

#[derive(Clone, PartialEq, Eq)]
struct Key {
    item_index: usize,
    source: DocumentIdentity,
    revision: OutputRevision,
    width: u16,
    theme: Theme,
    preferences: RenderPreferences,
    retain: bool,
    thinking_hover: bool,
    output_hover: Option<String>,
    expanded: Arc<HashSet<String>>,
    min_workflow_rows: usize,
    bash_range: Option<std::ops::Range<usize>>,
}

impl Key {
    fn compatible(&self, other: &Self) -> bool {
        self.source == other.source
            && self.item_index == other.item_index
            && self.revision.id == other.revision.id
            && self.revision.source_generation == other.revision.source_generation
            && self.width == other.width
            && self.theme == other.theme
            && self.preferences == other.preferences
            && self.retain == other.retain
            && self.thinking_hover == other.thinking_hover
            && self.output_hover == other.output_hover
            && self.expanded == other.expanded
            && self.min_workflow_rows == other.min_workflow_rows
    }
}

struct Ready {
    key: Key,
    body: ItemCacheEntry,
    workflow_rows: usize,
}

struct Pending {
    key: Key,
    receiver: Option<mpsc::Receiver<Result<Ready, String>>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        crate::render_work::discard(self.receiver.take());
    }
}

#[derive(Default)]
struct Slot {
    pending: Option<Pending>,
    ready: Option<Ready>,
    failed: Option<(Key, String)>,
}

pub(super) struct Preparation {
    pool: futures::executor::ThreadPool,
    slots: HashMap<u64, Slot>,
    pub(super) frame_started: Instant,
    pub(super) deferred: bool,
    pub(super) viewport: LayoutRequest,
}

impl Default for Preparation {
    fn default() -> Self {
        Self {
            pool: crate::render_work::workers(),
            slots: HashMap::new(),
            frame_started: Instant::now(),
            deferred: false,
            viewport: LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 0,
                follow_tail_rows: None,
            },
        }
    }
}

impl Preparation {
    const MAX_PENDING: usize = 64;
    const UI_BUDGET: Duration = Duration::from_millis(4);

    pub(super) fn clear(&mut self) {
        for slot in self.slots.values() {
            if let Some(pending) = &slot.pending {
                pending.cancelled.store(true, Ordering::Relaxed);
            }
        }
        crate::render_work::discard(std::mem::take(&mut self.slots));
        self.deferred = false;
    }

    pub(super) fn retire(&mut self, id: u64) {
        let slot = self.slots.remove(&id);
        if let Some(pending) = slot.as_ref().and_then(|slot| slot.pending.as_ref()) {
            pending.cancelled.store(true, Ordering::Relaxed);
        }
        crate::render_work::discard(slot);
    }

    pub(super) fn retain_ids(&mut self, live: &HashSet<u64>) {
        let removed: Vec<_> = self
            .slots
            .keys()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        for id in removed {
            self.retire(id);
        }
    }

    fn is_pending(&self) -> bool {
        self.deferred || self.slots.values().any(|slot| slot.pending.is_some())
    }

    fn error(&self) -> Option<&str> {
        self.slots
            .values()
            .find_map(|slot| slot.failed.as_ref().map(|(_, error)| error.as_str()))
    }

    fn retry(&mut self) {
        for slot in self.slots.values_mut() {
            slot.failed = None;
        }
    }

    fn update(
        &mut self,
        key: Key,
        snapshot: impl FnOnce() -> OutputSnapshot,
    ) -> (Option<(ItemCacheEntry, usize)>, bool) {
        let pending_count = self
            .slots
            .values()
            .filter(|slot| slot.pending.is_some())
            .count();
        let slot = self.slots.entry(key.revision.id).or_default();
        if slot
            .ready
            .as_ref()
            .is_some_and(|ready| !ready.key.compatible(&key))
        {
            crate::render_work::discard(slot.ready.take());
        }
        if slot
            .failed
            .as_ref()
            .is_some_and(|(failed, _)| failed != &key)
        {
            slot.failed = None;
        }
        if let Some(pending) = &mut slot.pending {
            let result = match pending
                .receiver
                .as_ref()
                .expect("render receiver")
                .try_recv()
            {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Document preparation stopped".into()))
                }
            };
            if let Some(result) = result {
                let pending_key = pending.key.clone();
                slot.pending = None;
                match result {
                    Ok(ready) if ready.key.compatible(&key) => {
                        // A source-preserving prefix is still usable while newer tokens prepare.
                        crate::render_work::discard(slot.ready.replace(ready));
                    }
                    Ok(ready) => crate::render_work::discard(ready),
                    Err(error) if pending_key == key => slot.failed = Some((key.clone(), error)),
                    Err(_) => {}
                }
            }
        }
        let current = slot.ready.as_ref().is_some_and(|ready| ready.key == key);
        if !current && slot.pending.is_none() && slot.failed.is_none() {
            if pending_count >= Self::MAX_PENDING {
                self.deferred = true;
            } else {
                let captured = snapshot();
                let previous = slot.ready.as_ref().map(|ready| ready.body.clone());
                let request = key.clone();
                let (sender, receiver) = mpsc::channel();
                let cancelled = Arc::new(AtomicBool::new(false));
                let stop = cancelled.clone();
                self.pool.spawn_ok(async move {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = atman_runtime::capture_blocking_panic(|| {
                        request.theme.with(|| {
                            request.preferences.with(|| {
                                let hover =
                                    request.output_hover.as_ref().map(|path| (0, path.clone()));
                                let context = RenderCtx {
                                    expanded_tools: &request.expanded,
                                    animation_frame: LAYOUT_ANIMATION_FRAME,
                                    panel_width: request.width,
                                    hovered_thinking_idx: request.thinking_hover.then_some(0),
                                    hovered_output_node: hover.as_ref(),
                                };
                                let mut renderer = LayoutCache {
                                    entries: vec![previous.unwrap_or_default()],
                                    ..Default::default()
                                };
                                renderer
                                    .workflow_body_rows
                                    .insert(request.revision.id, request.min_workflow_rows);
                                // The worker owns coalescing, so retained stream state can update now.
                                renderer.render_entry(
                                    0,
                                    captured.item(),
                                    request.revision,
                                    &context,
                                    request.retain,
                                    true,
                                );
                                let mut body = renderer.entries.pop().unwrap();
                                for region in Arc::make_mut(&mut body.regions) {
                                    region.panel_item_index = request.item_index;
                                }
                                if let (
                                    Some(range),
                                    Some(projection),
                                    OutputItem::Bash { output, .. },
                                ) = (
                                    &request.bash_range,
                                    body.bash_output.as_mut(),
                                    captured.item(),
                                ) {
                                    Arc::make_mut(projection).prepare_range(
                                        output,
                                        range.start,
                                        range.end,
                                    );
                                }
                                Ready {
                                    key: request.clone(),
                                    body,
                                    workflow_rows: renderer
                                        .workflow_body_rows
                                        .get(&request.revision.id)
                                        .copied()
                                        .unwrap_or(0),
                                }
                            })
                        })
                    })
                    .ok_or_else(|| "Document preparation failed".to_owned());
                    drop(captured);
                    if !stop.load(Ordering::Relaxed) {
                        let _ = sender.send(result);
                        crate::render_work::notify();
                    }
                });
                slot.pending = Some(Pending {
                    key: key.clone(),
                    receiver: Some(receiver),
                    cancelled,
                });
            }
        }
        (
            slot.ready
                .as_ref()
                .map(|ready| (ready.body.clone(), ready.workflow_rows)),
            !current && slot.failed.is_none(),
        )
    }
}

impl LayoutCache {
    pub(crate) fn enable_background(&mut self) {
        self.preparation.get_or_insert_with(Default::default);
    }

    pub(crate) fn preparing(&self) -> bool {
        self.preparation
            .as_ref()
            .is_some_and(Preparation::is_pending)
            || (self.preparation.is_some() && !self.pending_layout.is_empty())
    }

    pub(crate) fn preparation_error(&self) -> Option<&str> {
        self.preparation.as_ref().and_then(Preparation::error)
    }

    pub(crate) fn retry_preparation(&mut self) {
        if let Some(preparation) = &mut self.preparation {
            let failed: HashSet<_> = preparation
                .slots
                .iter()
                .filter_map(|(id, slot)| slot.failed.is_some().then_some(*id))
                .collect();
            self.pending_layout.extend(
                self.entries
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, entry)| failed.contains(&entry.revision.id).then_some(idx)),
            );
            preparation.retry();
        }
    }

    pub(super) fn prepare_entry(
        &mut self,
        idx: usize,
        items: &FlowDocument,
        ctx: &RenderCtx<'_>,
        retain: bool,
        force: bool,
    ) -> bool {
        let fold = self
            .work_folds
            .iter()
            .find(|fold| idx >= fold.start_index && idx <= fold.end_index);
        let hidden = fold.is_some_and(|fold| !fold.member_visible(idx));
        let item = &items[idx];
        let retain = retain || self.retention_protected.contains(&idx);
        let id = items.revisions()[idx].id;
        if matches!(item, OutputItem::WorkflowPanel { .. }) && retain && !hidden {
            let (values, running) = MetricSnapshot::item(item);
            self.metric_motion.entry(id).or_default().observe(
                values,
                running,
                RenderPreferences::current().metrics_animation,
                Instant::now(),
            );
        } else {
            self.metric_motion.remove(&id);
        }
        let Some(preparation) = self.preparation.as_mut() else {
            return self.render_entry(idx, &items[idx], items.revisions()[idx], ctx, retain, force);
        };
        if preparation.frame_started.elapsed() >= Preparation::UI_BUDGET {
            preparation.deferred = true;
            self.pending_layout.insert(idx);
            // Admission limits body preparation, not the current frame's fold geometry.
            // Its inexpensive envelope must reach the same projection as scrolling.
            self.entries[idx].defer_body(item, items.revisions()[idx], ctx.panel_width);
            apply_work_fold_to_entry(&mut self.entries[idx], fold, idx, ctx.panel_width);
            return true;
        }
        // Small text is a bounded synchronous unit; oversized parsing and highlighting
        // always leave the event thread, regardless of whether the source is streaming.
        let heavy = match item {
            OutputItem::AssistantMd { md, .. } => md.len() > 1024 || md.contains("```"),
            OutputItem::UserTurn { text, presentation }
            | OutputItem::Interjection { text, presentation } => {
                text.len() > 1024 || presentation.is_some()
            }
            OutputItem::Thinking { text, .. } | OutputItem::SystemNote { text, .. } => {
                text.len() > 1024 || text.contains("```")
            }
            OutputItem::ActivitySummary { .. }
            | OutputItem::WorkFoldMarker { .. }
            | OutputItem::Divider => false,
            _ => true,
        };
        if hidden || !heavy {
            preparation.retire(items.revisions()[idx].id);
            return self.render_entry(idx, item, items.revisions()[idx], ctx, retain, force);
        }
        let key = Key {
            item_index: idx,
            source: items.identity(),
            revision: items.revisions()[idx],
            width: if fold.is_some() {
                work_fold_content_width(ctx.panel_width)
            } else {
                ctx.panel_width
            },
            theme: crate::theme::theme(),
            preferences: RenderPreferences::current(),
            retain,
            thinking_hover: ctx.hovered_thinking_idx == Some(idx),
            output_hover: ctx
                .hovered_output_node
                .filter(|(index, _)| *index == idx)
                .map(|(_, path)| path.clone()),
            expanded: Arc::new(ctx.expanded_tools.clone()),
            min_workflow_rows: self
                .workflow_body_rows
                .get(&items.revisions()[idx].id)
                .copied()
                .unwrap_or(0),
            bash_range: matches!(item, OutputItem::Bash { .. }).then(|| {
                let start_row = idx
                    .checked_sub(1)
                    .map_or(0, |previous| self.row_ends[previous]);
                let prefix = self.entries[idx].prefix_lines.len().min(u32::MAX as usize) as u32;
                let start = preparation
                    .viewport
                    .scroll_offset
                    .saturating_sub(start_row)
                    .saturating_sub(prefix) as usize;
                start.saturating_sub(8)
                    ..start
                        .saturating_add(preparation.viewport.viewport_rows as usize)
                        .saturating_add(8)
            }),
        };
        let (body, pending) =
            preparation.update(key, || items.snapshot(idx).expect("existing document item"));
        if pending {
            self.pending_layout.insert(idx);
        }
        if let Some((mut entry, workflow_rows)) = body {
            if matches!(item, OutputItem::WorkflowPanel { .. }) {
                self.workflow_body_rows
                    .insert(entry.revision.id, workflow_rows);
            }
            self.access_clock = self.access_clock.wrapping_add(1);
            entry.last_used = self.access_clock;
            apply_work_fold_to_entry(&mut entry, fold, idx, ctx.panel_width);
            crate::render_work::discard(std::mem::replace(&mut self.entries[idx], entry));
            self.retention_dirty = true;
        } else {
            self.entries[idx].defer_body(item, items.revisions()[idx], ctx.panel_width);
            apply_work_fold_to_entry(&mut self.entries[idx], fold, idx, ctx.panel_width);
        }
        true
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, streaming: bool) -> OutputItem {
        OutputItem::AssistantMd {
            md: text.into(),
            streaming,
            retried: false,
        }
    }

    fn update(cache: &mut LayoutCache, document: &FlowDocument) {
        update_at(cache, document, 0);
    }

    fn update_at(cache: &mut LayoutCache, document: &FlowDocument, scroll: u32) {
        cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            document,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: scroll,
                viewport_rows: 30,
                follow_tail_rows: None,
            },
        );
    }

    fn settle(cache: &mut LayoutCache, document: &FlowDocument) {
        settle_at(cache, document, 0);
    }

    fn settle_at(cache: &mut LayoutCache, document: &FlowDocument, scroll: u32) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            update_at(cache, document, scroll);
            assert!(cache.preparation_error().is_none());
            if !cache.preparing() {
                break;
            }
            assert!(Instant::now() < until, "body worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn gated_pool() -> (futures::executor::ThreadPool, Vec<mpsc::Sender<()>>) {
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(2)
            .create()
            .unwrap();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (sender, receiver) = mpsc::channel();
            let (started, ready) = mpsc::channel();
            pool.spawn_ok(async move {
                started.send(()).unwrap();
                receiver.recv().unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            releases.push(sender);
        }
        (pool, releases)
    }

    fn tool_card() -> OutputItem {
        OutputItem::ToolDispatch {
            calls: vec![ToolCallView {
                id: "loading-tool".into(),
                tool: "bash.spawn".into(),
                intent: "Check output".into(),
                input: serde_json::json!({"command": "true"}),
                status: ToolCallStatus::Running,
                disclosure: Disclosure::Summary,
                detail: None,
                draft_index: Some(0),
                draft_preview: Default::default(),
                applied_edit: None,
                started_at: Instant::now(),
                ended_at: None,
            }],
        }
    }

    #[test]
    fn cold_tool_cards_keep_their_extent_through_loading_and_source_updates() {
        for framed in [false, true] {
            let mut document = crate::app::FlowViewModel::default();
            document.push(item(&"Previous output\n\n".repeat(50), false));
            let mut cache = LayoutCache::default();
            cache.enable_background();
            settle(&mut cache, &document);
            document.push(tool_card());
            if framed {
                document.upsert_work_fold(1..2, Some("Work"), false);
                let fold = &mut document.work_folds[0];
                fold.from_visible = 1.0;
                fold.target_visible = 1.0;
                fold.duration = Duration::ZERO;
                cache.set_work_folds(document.work_fold_projections(None));
            }
            let mut expected = LayoutCache::default();
            expected.set_work_folds(cache.work_folds.clone());
            update(&mut expected, &document);
            let (pool, releases) = gated_pool();
            cache.preparation.as_mut().unwrap().pool = pool;
            reset_perf_counters();
            for frame in 0..4 {
                let metrics = cache.update_dirty(
                    LayoutKey {
                        width: 80,
                        theme: crate::theme::current_mode(),
                    },
                    &document,
                    &RenderCtx::empty(),
                    LayoutRequest {
                        scroll_offset: 0,
                        viewport_rows: 30,
                        follow_tail_rows: Some(30),
                    },
                );
                assert!(cache.preparing());
                assert_eq!(
                    perf_counters().item_renders,
                    0,
                    "body work must stay off the UI"
                );
                assert_eq!(
                    cache.total_rows, expected.total_rows,
                    "cold loading changed card height"
                );
                assert_eq!(
                    metrics.scroll_offset,
                    expected.total_rows.saturating_sub(30)
                );
                let visible = cache.visible_slice(cache.row_start(1), 30, frame);
                let prefix = cache.entries[1].prefix_lines.len();
                assert!(
                    visible.lines[prefix + TOOL_DISPATCH_HEADER_ROWS / 2]
                        .to_string()
                        .contains('━')
                );
                if !framed {
                    assert!(
                        visible.lines[..TOOL_DISPATCH_HEADER_ROWS]
                            .iter()
                            .all(|line| line.spans.iter().all(|span| line
                                .style
                                .patch(span.style)
                                .bg
                                == Some(crate::theme::theme().work_bg.into())))
                    );
                    assert_eq!(visible.lines[TOOL_DISPATCH_HEADER_ROWS].style.bg, None);
                }
                assert!(visible.selection.surfaces.is_empty());
                assert!(
                    visible
                        .regions
                        .iter()
                        .all(|region| region.path_key.starts_with(WORK_FOLD_REGION_PREFIX))
                );
                if frame == 1 {
                    document.mutate(1, crate::flow_view::OutputMutation::Semantic, |item| {
                        let OutputItem::ToolDispatch { calls } = item else {
                            unreachable!()
                        };
                        calls[0].draft_index = None;
                        calls[0].intent = "Read output".into();
                        true
                    });
                }
            }
            for release in releases {
                release.send(()).unwrap();
            }
            settle(&mut cache, &document);
            assert_eq!(
                cache.total_rows, expected.total_rows,
                "installing ready paint shifted the tail"
            );
            document.mutate(1, crate::flow_view::OutputMutation::Semantic, |item| {
                let OutputItem::ToolDispatch { calls } = item else {
                    unreachable!()
                };
                calls[0].status = ToolCallStatus::Ok;
                calls[0].ended_at = Some(Instant::now());
                true
            });
            settle(&mut cache, &document);
            assert_eq!(cache.total_rows, expected.total_rows);
            assert_eq!(cache.entries[1].revision, document.revisions()[1]);
        }
    }

    #[test]
    fn cold_tool_card_geometry_precedes_body_admission() {
        let document = FlowDocument::from(vec![tool_card()]);
        let mut expected = LayoutCache::default();
        update(&mut expected, &document);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.entries.resize(1, ItemCacheEntry::default());
        cache.preparation.as_mut().unwrap().frame_started = Instant::now() - Duration::from_secs(1);
        reset_perf_counters();
        assert!(cache.prepare_entry(0, &document, &RenderCtx::empty(), true, false));
        assert_eq!(cache.entries[0].rows, expected.entries[0].rows);
        assert!(cache.preparing());
        assert!(cache.preparation.as_ref().unwrap().slots.is_empty());
        assert_eq!(perf_counters().item_renders, 0);
        settle(&mut cache, &document);
        assert_eq!(cache.total_rows, expected.total_rows);
    }

    #[test]
    fn history_reading_waits_for_current_body_geometry_without_accepting_foreign_documents() {
        let mut document = FlowDocument::from(vec![item(&"Original row\n\n".repeat(90), false)]);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        for revision in 0..2 {
            if revision != 0 {
                document.mutate(0, crate::flow_view::OutputMutation::Semantic, |value| {
                    *value = item(&"Replacement row\n\n".repeat(120), false);
                    true
                });
            }
            let (pool, releases) = gated_pool();
            cache.preparation.as_mut().unwrap().pool = pool;
            update(&mut cache, &document);
            assert!(!cache.history_reading_offset(&document, 0, 10).unwrap().1);
            let foreign = FlowDocument::from(vec![item(&"Original row\n\n".repeat(90), false)]);
            assert!(cache.history_reading_offset(&foreign, 0, 10).is_none());
            for release in releases {
                release.send(()).unwrap();
            }
            settle(&mut cache, &document);
            assert_eq!(
                cache.history_reading_offset(&document, 0, 10),
                Some((10, true))
            );
        }
    }

    #[test]
    fn viewport_preparation_keeps_the_layout_used_to_resolve_scroll() {
        for framed in [false, true] {
            let mut document = crate::app::FlowViewModel::default();
            document.push(item(&"Original row\n\n".repeat(90), false));
            document.push(item("TAIL-ANCHOR", false));
            let mut cache = LayoutCache::default();
            cache.enable_background();
            if framed {
                document.upsert_work_fold(0..1, Some("Work"), false);
                let fold = &mut document.work_folds[0];
                fold.from_visible = 1.0;
                fold.target_visible = 1.0;
                fold.duration = Duration::ZERO;
                cache.set_work_folds(document.work_fold_projections(None));
            }
            settle(&mut cache, &document);
            let (pool, releases) = gated_pool();
            cache.preparation.as_mut().unwrap().pool = pool;
            document.mutate(0, crate::flow_view::OutputMutation::Semantic, |value| {
                *value = item(&"Replacement row\n\n".repeat(140), false);
                true
            });
            update(&mut cache, &document);
            let rows = cache.total_rows;
            let offsets = cache.row_ends.clone();
            let scroll = rows.saturating_sub(30);
            for release in releases {
                release.send(()).unwrap();
            }
            // Deliver the worker result exactly after scroll resolution and before paint.
            let pending = cache
                .preparation
                .as_mut()
                .unwrap()
                .slots
                .get_mut(&document.revisions()[0].id)
                .unwrap()
                .pending
                .as_mut()
                .unwrap();
            let result = pending
                .receiver
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            let (sender, receiver) = mpsc::channel();
            sender.send(result).unwrap();
            pending.receiver = Some(receiver);
            cache.prepare_viewport(
                &document,
                &RenderCtx::empty(),
                LayoutRequest {
                    scroll_offset: scroll,
                    viewport_rows: 30,
                    follow_tail_rows: None,
                },
            );
            assert_eq!(
                cache.total_rows, rows,
                "viewport preparation changed resolved geometry"
            );
            assert_eq!(cache.row_ends, offsets);
            settle_at(&mut cache, &document, scroll);
            assert!(
                cache.total_rows > rows,
                "the following geometry phase must install the body"
            );
        }
    }

    #[test]
    fn resolved_viewport_schedules_evicted_content_without_changing_its_extent() {
        let mut items = vec![item(&"RESTORED-BODY\n\n".repeat(90), false)];
        items.extend((0..100).map(|_| item("Recent output", false)));
        let document = FlowDocument::from(items);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        settle_at(&mut cache, &document, u32::MAX);
        assert!(!cache.entries[0].has_retained_lines());
        let rows = cache.total_rows;
        let offsets = cache.row_ends.clone();
        cache.prepare_viewport(
            &document,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 30,
                follow_tail_rows: None,
            },
        );
        assert_eq!(cache.total_rows, rows);
        assert_eq!(cache.row_ends, offsets);
        assert!(
            cache.preparing(),
            "viewport work must retain its next-frame wakeup"
        );
        let pending = cache.visible_slice(0, 30, 0);
        assert_eq!(pending.lines.len(), 30);
        assert!(pending.selection.surfaces.is_empty());
        settle_at(&mut cache, &document, 0);
        assert_eq!(cache.total_rows, rows);
        assert!(
            cache
                .visible_slice(0, 30, 0)
                .lines
                .iter()
                .any(|line| { line.to_string().contains("RESTORED-BODY") })
        );
    }

    #[test]
    fn fenced_thinking_keeps_disclosure_preparation_off_the_event_thread() {
        let (pool, releases) = gated_pool();
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.preparation.as_mut().unwrap().pool = pool;
        let document = FlowDocument::from(vec![OutputItem::Thinking {
            text: "```rust\nlet value = 1;\n```".into(),
            done: true,
            retried: false,
            disclosure: Disclosure::Summary,
        }]);
        reset_perf_counters();
        update(&mut cache, &document);
        let renders = perf_counters().item_renders;
        let pending = cache.preparing();
        let transition = cache.next_markdown_disclosure(&document, 0);
        for release in releases {
            release.send(()).unwrap();
        }
        assert!(pending);
        assert_eq!(renders, 0);
        assert!(transition.is_none());
        settle(&mut cache, &document);
        assert_eq!(
            cache.next_markdown_disclosure(&document, 0),
            Some(Disclosure::Full)
        );
    }

    #[test]
    fn body_preparation_is_nonblocking_source_bound_and_preserves_actual_revisions() {
        let (pool, releases) = gated_pool();
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.preparation.as_mut().unwrap().pool = pool;
        let mut document = FlowDocument::from(vec![item(&"Old prefix 汉\n".repeat(400), true)]);
        let started = Instant::now();
        update(&mut cache, &document);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(cache.preparing());
        document.mutate(
            0,
            crate::flow_view::OutputMutation::SemanticPreserveSource,
            |item| {
                let OutputItem::AssistantMd { md, .. } = item else {
                    unreachable!()
                };
                md.push_str("Latest content");
                true
            },
        );
        update(&mut cache, &document);
        let id = document.revisions()[0].id;
        assert!(
            cache.preparation.as_ref().unwrap().slots[&id]
                .ready
                .is_none()
        );
        for release in releases {
            release.send(()).unwrap();
        }
        settle(&mut cache, &document);
        assert_eq!(cache.entries[0].revision, document.revisions()[0]);
        let tail = cache.visible_slice(cache.total_rows.saturating_sub(10), 10, 0);
        assert!(tail.lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Latest content")
        }));
        let payload = &document[0] as *const OutputItem;
        document.mutate(
            0,
            crate::flow_view::OutputMutation::SemanticPreserveSource,
            |item| {
                let OutputItem::AssistantMd { md, .. } = item else {
                    unreachable!()
                };
                md.push('!');
                true
            },
        );
        assert_eq!(
            payload, &document[0] as *const OutputItem,
            "ready bodies must not retain source payloads"
        );
        let replacement = FlowDocument::from(vec![item(&"Other source\n".repeat(300), false)]);
        settle(&mut cache, &replacement);
        assert_eq!(
            cache.source_identity.as_ref(),
            Some(&replacement.identity())
        );
        assert!(cache.visible_slice(0, 10, 0).lines.iter().any(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .contains("Other source")
        }));
    }

    #[test]
    fn work_geometry_finishes_even_when_the_body_budget_is_exhausted() {
        let mut document = crate::app::FlowViewModel::default();
        document.push(item(&"Existing body\n\n".repeat(90), false));
        document.upsert_work_fold(0..1, Some("Work"), false);
        let state = &mut document.work_folds[0];
        state.from_visible = 1.0;
        state.target_visible = 1.0;
        state.duration = Duration::ZERO;
        let mut fold = document.work_fold_projections(None).remove(0);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.set_work_folds(vec![fold.clone()]);
        settle(&mut cache, &document);
        for fraction in [u16::MAX / 2, 1, 0] {
            fold.visible_members = usize::from(fraction > 0);
            fold.boundary_member = (fraction > 0).then_some(0);
            fold.boundary_fraction = fraction;
            fold.animating = fraction > 0;
            fold.expanded = false;
            let mut expected = LayoutCache::default();
            expected.set_work_folds(vec![fold.clone()]);
            update(&mut expected, &document);
            cache.set_work_folds(vec![fold.clone()]);
            cache.preparation.as_mut().unwrap().frame_started =
                Instant::now() - Duration::from_secs(1);
            reset_perf_counters();
            assert!(cache.prepare_entry(0, &document, &RenderCtx::empty(), true, false));
            assert_eq!(
                cache.entries[0].rows, expected.entries[0].rows,
                "body admission must not defer current Work geometry"
            );
            assert_eq!(perf_counters().item_renders, 0);
        }
    }

    #[test]
    fn replacing_document_entries_retires_removed_body_tasks_and_feedback() {
        let (pool, releases) = gated_pool();
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.preparation.as_mut().unwrap().pool = pool;
        let mut document = FlowDocument::from(vec![item(&"queued body\n".repeat(300), false)]);
        update(&mut cache, &document);
        let id = document.revisions()[0].id;
        let cancelled = cache.preparation.as_ref().unwrap().slots[&id]
            .pending
            .as_ref()
            .unwrap()
            .cancelled
            .clone();
        assert!(cache.preparing());
        document.replace(vec![item("replacement", false)]);
        update(&mut cache, &document);
        let still_preparing = cache.preparing();
        let obsolete = cache.preparation.as_ref().unwrap().slots.contains_key(&id);
        for release in releases {
            release.send(()).unwrap();
        }
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(
            !obsolete,
            "removed entries must retire pending and prepared ownership"
        );
        assert!(
            !still_preparing,
            "removed work cannot keep loading feedback active"
        );
        assert_eq!(cache.entries[0].revision, document.revisions()[0]);
    }

    #[test]
    fn pending_source_replacement_preserves_extent_and_centers_feedback() {
        for framed in [false, true] {
            let mut cache = LayoutCache::default();
            cache.enable_background();
            let mut document = crate::app::FlowViewModel::default();
            document.push(item(&"Original body\n\n".repeat(60), false));
            if framed {
                document.upsert_work_fold(0..1, Some("Work"), false);
                let fold = &mut document.work_folds[0];
                fold.from_visible = 1.0;
                fold.target_visible = 1.0;
                fold.duration = Duration::ZERO;
                cache.set_work_folds(document.work_fold_projections(None));
            }
            settle(&mut cache, &document);
            let rows = cache.total_rows;
            let (pool, releases) = gated_pool();
            cache.preparation.as_mut().unwrap().pool = pool;
            document.mutate(0, crate::flow_view::OutputMutation::Semantic, |value| {
                *value = item(&"Replacement body\n\n".repeat(60), false);
                true
            });
            update(&mut cache, &document);
            assert!(cache.preparing());
            assert_eq!(
                cache.total_rows, rows,
                "a pending revision must preserve the occupied extent"
            );
            let visible = cache.visible_slice(0, 30, 1);
            let prefix = cache.entries[0].prefix_lines.len();
            let middle = prefix + (30 - prefix) / 2;
            assert_eq!(visible.lines.len(), 30);
            let feedback: Vec<_> = visible
                .lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.to_string().contains('━'))
                .collect();
            assert_eq!(feedback.len(), 1);
            assert_eq!(
                feedback[0].0, middle,
                "feedback belongs in the visible body's center"
            );
            assert!(
                feedback[0]
                    .1
                    .to_string()
                    .replace(['⠇', '⠸'], " ")
                    .starts_with(&" ".repeat(36))
            );
            assert!(
                visible
                    .regions
                    .iter()
                    .all(|region| region.path_key.starts_with(WORK_FOLD_REGION_PREFIX))
            );
            assert!(visible.selection.surfaces.is_empty());
            cache.update_dirty(
                LayoutKey {
                    width: 61,
                    theme: crate::theme::current_mode(),
                },
                &document,
                &RenderCtx {
                    panel_width: 61,
                    ..RenderCtx::empty()
                },
                LayoutRequest {
                    scroll_offset: 0,
                    viewport_rows: 30,
                    follow_tail_rows: None,
                },
            );
            assert_eq!(
                cache.total_rows, rows,
                "resize must retain geometry independently of invalid paint"
            );
            let visible = cache.visible_slice(0, 30, 2);
            assert_eq!(visible.lines.len(), 30);
            assert!(
                visible.lines[middle]
                    .to_string()
                    .replace(['⠇', '⠸'], " ")
                    .starts_with(&" ".repeat(26))
            );
            assert!(
                visible
                    .regions
                    .iter()
                    .all(|region| region.path_key.starts_with(WORK_FOLD_REGION_PREFIX))
            );
            assert!(visible.selection.surfaces.is_empty());
            let other = FlowDocument::from(vec![item(&"Foreign source\n".repeat(200), false)]);
            cache.set_work_folds(Vec::new());
            update(&mut cache, &other);
            assert_eq!(
                cache.total_rows, 1,
                "a foreign document cannot inherit the previous extent"
            );
            assert!(cache.visible_slice(0, 30, 3).selection.surfaces.is_empty());
            for release in releases {
                release.send(()).unwrap();
            }
            settle(&mut cache, &other);
        }
    }

    #[test]
    fn closing_queued_body_preparation_does_not_wait_for_render_workers() {
        let (pool, releases) = gated_pool();
        let mut cache = LayoutCache::default();
        cache.enable_background();
        cache.preparation.as_mut().unwrap().pool = pool;
        let document = FlowDocument::from(vec![item(&"queued body\n".repeat(300), false)]);
        update(&mut cache, &document);
        let started = Instant::now();
        drop(cache);
        assert!(started.elapsed() < Duration::from_millis(100));
        for release in releases {
            release.send(()).unwrap();
        }
    }

    #[test]
    fn prepared_text_fold_envelopes_are_idempotent_and_match_synchronous_geometry() {
        let document = FlowDocument::from(vec![item(
            &format!("```rust\n{}\n```", "let value = 123;\n".repeat(100)),
            false,
        )]);
        let mut asynchronous = LayoutCache::default();
        asynchronous.enable_background();
        let fold = WorkFoldProjection {
            key: document.revisions()[0].id,
            start_index: 0,
            end_index: 0,
            visible_members: 1,
            total_members: 1,
            boundary_member: Some(0),
            boundary_fraction: u16::MAX,
            boundary_level: 2,
            completed_steps: 1,
            total_steps: 1,
            expanded: true,
            animating: true,
            hovered: false,
            title: "Prepared body".into(),
            stats: String::new(),
        };
        asynchronous.set_work_folds(vec![fold.clone()]);
        settle(&mut asynchronous, &document);
        let mut synchronous = LayoutCache::default();
        synchronous.set_work_folds(vec![fold.clone()]);
        update(&mut synchronous, &document);
        assert_eq!(asynchronous.total_rows, synchronous.total_rows);
        assert_eq!(
            asynchronous.visible_slice(0, 30, 0).lines,
            synchronous.visible_slice(0, 30, 0).lines
        );
        let initial = asynchronous.total_rows;
        for hovered in [true, false, true, false] {
            let changed = WorkFoldProjection {
                hovered,
                ..fold.clone()
            };
            asynchronous.set_work_folds(vec![changed.clone()]);
            synchronous.set_work_folds(vec![changed]);
            settle(&mut asynchronous, &document);
            update(&mut synchronous, &document);
            assert_eq!(asynchronous.total_rows, initial);
            assert_eq!(
                asynchronous.visible_slice(0, 30, 0).lines,
                synchronous.visible_slice(0, 30, 0).lines
            );
        }
        asynchronous.set_work_folds(vec![WorkFoldProjection {
            visible_members: 0,
            expanded: false,
            ..fold
        }]);
        update(&mut asynchronous, &document);
        assert!(!asynchronous.preparing());
        assert!(asynchronous.entries[0].content_hidden);
        assert!(asynchronous.preparation.as_ref().unwrap().slots.is_empty());
    }

    #[test]
    fn work_fold_boundary_clips_prepared_paint_hits_and_selection_together() {
        let document = FlowDocument::from(vec![item(
            &format!(
                "first paragraph\n\n```rust\n{}\n```\n\nlast paragraph",
                "let value = 123;\n".repeat(80)
            ),
            false,
        )]);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        let mut fold = WorkFoldProjection {
            key: document.revisions()[0].id,
            start_index: 0,
            end_index: 0,
            visible_members: 1,
            total_members: 1,
            boundary_member: Some(0),
            boundary_fraction: u16::MAX,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: false,
            animating: true,
            hovered: false,
            title: "Prepared body".into(),
            stats: String::new(),
        };
        cache.set_work_folds(vec![fold.clone()]);
        settle(&mut cache, &document);
        let full_rows = cache.total_rows;
        let mut previous = full_rows;
        for percent in (1..100).rev() {
            fold.boundary_fraction = (u32::from(u16::MAX) * percent / 100) as u16;
            cache.set_work_folds(vec![fold.clone()]);
            update(&mut cache, &document);
            assert!(!cache.preparing(), "clipping must reuse the prepared body");
            assert!(cache.total_rows <= previous);
            assert!(
                previous - cache.total_rows <= 1,
                "a tall item must shrink by rows"
            );
            previous = cache.total_rows;
            let bottom = cache.entries[0].content_row_end() as u32;
            let visible = cache.visible_slice(0, full_rows, 0);
            assert_eq!(visible.lines.len(), cache.total_rows as usize);
            assert!(
                visible
                    .regions
                    .iter()
                    .all(|region| region.end_row <= bottom)
            );
            for surface in &visible.selection.surfaces {
                assert!(
                    surface
                        .prose_atoms
                        .iter()
                        .all(|atom| atom.screen_row < bottom)
                );
                assert!(
                    surface
                        .code_atoms
                        .iter()
                        .all(|code| code.atom.screen_row < bottom)
                );
            }
        }
        assert!(previous < full_rows / 2);
        fold.boundary_fraction = 1;
        cache.set_work_folds(vec![fold.clone()]);
        update(&mut cache, &document);
        let terminal_rows = render_work_fold_header(&fold, 80).len() + 1;
        assert_eq!(
            cache.total_rows as usize, terminal_rows,
            "the footer must shrink with the body"
        );
        let terminal_lines = cache.visible_slice(0, full_rows, 0).lines;
        assert!(!terminal_lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains('⠧') || span.content.contains('⠇'))
        }));
        fold.boundary_fraction = u16::MAX;
        cache.set_work_folds(vec![fold]);
        update(&mut cache, &document);
        assert!(!cache.preparing());
        assert_eq!(cache.total_rows, full_rows, "clipping must not accumulate");
    }

    #[test]
    fn pending_body_receives_current_work_chrome_before_final_is_visible() {
        let mut document = FlowDocument::from(vec![item(&"retained body\n".repeat(300), false)]);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        settle(&mut cache, &document);
        let (pool, releases) = gated_pool();
        cache.preparation.as_mut().unwrap().pool = pool;
        document.push(item("FINAL-DELIVERY", true));
        let fold = WorkFoldProjection {
            key: document.revisions()[0].id,
            start_index: 0,
            end_index: 0,
            visible_members: 1,
            total_members: 1,
            boundary_member: None,
            boundary_fraction: u16::MAX,
            boundary_level: 3,
            completed_steps: 4,
            total_steps: 4,
            expanded: false,
            animating: true,
            hovered: false,
            title: "Work".into(),
            stats: String::new(),
        };
        cache.set_work_folds(vec![fold.clone()]);
        update(&mut cache, &document);
        assert!(cache.preparing());
        let first_rows = cache.entries[0].rows;
        let header = cache.entries[0]
            .prefix_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let final_ready = cache.entries[1].has_retained_lines();
        for release in releases {
            release.send(()).unwrap();
        }
        assert!(
            header.to_lowercase().contains("work"),
            "the UI owns Work chrome even while its body waits for a worker"
        );
        assert!(final_ready);
        cache.set_work_folds(vec![WorkFoldProjection {
            hovered: true,
            ..fold
        }]);
        settle(&mut cache, &document);
        assert_eq!(
            cache.entries[0].rows, first_rows,
            "body completion and hover cannot apply the Work prefix twice"
        );
    }

    #[test]
    fn body_failure_stops_feedback_and_retries_without_losing_the_document() {
        let document = FlowDocument::from(vec![item(&"Retained body\n".repeat(300), false)]);
        let mut cache = LayoutCache::default();
        cache.enable_background();
        settle(&mut cache, &document);
        let id = document.revisions()[0].id;
        let state = cache.preparation.as_mut().unwrap();
        let slot = state.slots.get_mut(&id).unwrap();
        let key = slot.ready.as_ref().unwrap().key.clone();
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        slot.pending = Some(Pending {
            key: key.clone(),
            receiver: Some(receiver),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        cache.pending_layout.insert(0);
        update(&mut cache, &document);
        assert!(cache.preparation_error().is_some());
        assert!(!cache.preparing());
        assert!(cache.entries[0].has_retained_lines());
        cache.retry_preparation();
        settle(&mut cache, &document);
        assert!(cache.preparation_error().is_none());
        assert_eq!(cache.entries[0].revision, document.revisions()[0]);
    }

    #[test]
    fn rich_bodies_keep_original_regions_and_prepare_bash_only_for_the_visible_interval() {
        let document = FlowDocument::from(vec![
            OutputItem::Divider,
            OutputItem::FsDetail {
                view: crate::app::FsDetail::Read {
                    path: "source.rs".into(),
                    content: "let value = 123;\n".repeat(100),
                    start_line: 1,
                    total_lines: None,
                    truncated: false,
                },
                expanded: false,
            },
            OutputItem::Bash {
                handle: "large-log".into(),
                title: None,
                command: Some("show log".into()),
                output: (0..4096).map(|row| format!("row {row:04}\n")).collect(),
                done: true,
                expanded: true,
            },
        ]);
        let mut asynchronous = LayoutCache::default();
        asynchronous.enable_background();
        let (pool, releases) = gated_pool();
        asynchronous.preparation.as_mut().unwrap().pool = pool;
        update(&mut asynchronous, &document);
        assert!(asynchronous.preparing());
        assert!(asynchronous.entries[1].semantic.is_none());
        assert!(asynchronous.entries[2].bash_output.is_none());
        for release in releases {
            release.send(()).unwrap();
        }
        settle(&mut asynchronous, &document);
        let mut synchronous = LayoutCache::default();
        update(&mut synchronous, &document);
        assert_eq!(asynchronous.total_rows, synchronous.total_rows);
        assert_eq!(
            asynchronous.visible_slice(0, 30, 0).lines,
            synchronous.visible_slice(0, 30, 0).lines
        );
        assert_eq!(
            asynchronous.entries[1].regions,
            synchronous.entries[1].regions
        );
        assert!(
            asynchronous.entries[1]
                .regions
                .iter()
                .all(|region| region.panel_item_index == 1)
        );
        let scroll = asynchronous.row_start(2) + 2048;
        reset_perf_counters();
        settle_at(&mut asynchronous, &document, scroll);
        assert_eq!(
            perf_counters().bash_materialized_rows,
            0,
            "UI must not materialize Bash text"
        );
        update_at(&mut synchronous, &document, scroll);
        let visible = asynchronous.visible_slice(scroll, 30, 0);
        assert_eq!(
            visible.lines,
            synchronous.visible_slice(scroll, 30, 0).lines
        );
        let projection = asynchronous.entries[2].bash_output.as_ref().unwrap();
        assert!(projection.prepared_end - projection.prepared_start <= 46);
        assert_eq!(
            visible.regions,
            synchronous.visible_slice(scroll, 30, 0).regions
        );
    }

    #[test]
    fn background_workflow_preserves_the_observed_minimum_body_height() {
        let now = Instant::now();
        let document = FlowDocument::from(vec![OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: WorkflowProjection::new(atman_rt::TurnId::now()),
            presentation: Default::default(),
            expanded_nodes: HashSet::new(),
            panel_expanded: false,
            started_at: now,
            ended_at: Some(now),
            cancelled: false,
        }]);
        let id = document.revisions()[0].id;
        let mut asynchronous = LayoutCache::default();
        asynchronous.enable_background();
        update(&mut asynchronous, &document);
        asynchronous.workflow_body_rows.insert(id, 57);
        settle(&mut asynchronous, &document);
        let mut synchronous = LayoutCache::default();
        update(&mut synchronous, &document);
        synchronous.workflow_body_rows.insert(id, 57);
        synchronous.pending_layout.insert(0);
        update(&mut synchronous, &document);
        assert_eq!(asynchronous.total_rows, synchronous.total_rows);
        assert!(asynchronous.workflow_body_rows[&id] >= 57);
        assert!(asynchronous.metric_motion.contains_key(&id));
        assert!(synchronous.metric_motion.contains_key(&id));
        assert_eq!(
            asynchronous.visible_slice(0, 30, 0).lines,
            synchronous.visible_slice(0, 30, 0).lines
        );
    }
}
