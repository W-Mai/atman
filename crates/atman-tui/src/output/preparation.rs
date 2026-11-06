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
    source: DocumentIdentity,
    revision: OutputRevision,
    width: u16,
    theme: Theme,
    preferences: RenderPreferences,
    retain: bool,
    thinking_hover: bool,
    output_hover: Option<String>,
    expanded: Arc<HashSet<String>>,
}

impl Key {
    fn compatible(&self, other: &Self) -> bool {
        self.source == other.source
            && self.revision.id == other.revision.id
            && self.revision.source_generation == other.revision.source_generation
            && self.width == other.width
            && self.theme == other.theme
            && self.preferences == other.preferences
            && self.retain == other.retain
            && self.thinking_hover == other.thinking_hover
            && self.output_hover == other.output_hover
            && self.expanded == other.expanded
    }
}

struct Ready {
    key: Key,
    body: ItemCacheEntry,
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
}

impl Default for Preparation {
    fn default() -> Self {
        Self {
            pool: crate::render_work::workers(),
            slots: HashMap::new(),
            frame_started: Instant::now(),
            deferred: false,
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
    ) -> (Option<ItemCacheEntry>, bool) {
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
                                // The worker owns coalescing, so retained stream state can update now.
                                renderer.render_entry(
                                    0,
                                    captured.item(),
                                    request.revision,
                                    &context,
                                    request.retain,
                                    true,
                                );
                                Ready {
                                    key: request.clone(),
                                    body: renderer.entries.pop().unwrap(),
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
            slot.ready.as_ref().map(|ready| ready.body.clone()),
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
        let Some(preparation) = self.preparation.as_mut() else {
            return self.render_entry(idx, &items[idx], items.revisions()[idx], ctx, retain, force);
        };
        if preparation.frame_started.elapsed() >= Preparation::UI_BUDGET {
            preparation.deferred = true;
            self.pending_layout.insert(idx);
            return true;
        }
        let fold = self
            .work_folds
            .iter()
            .find(|fold| idx >= fold.start_index && idx <= fold.end_index);
        let hidden = fold.is_some_and(|fold| !fold.member_visible(idx));
        let item = &items[idx];
        let retain = retain || self.retention_protected.contains(&idx);
        // Small text is a bounded synchronous unit; oversized parsing and highlighting
        // always leave the event thread, regardless of whether the source is streaming.
        let heavy = match item {
            OutputItem::AssistantMd { md, .. } => md.len() > 1024,
            OutputItem::UserTurn { text, .. } | OutputItem::Interjection { text, .. } => {
                text.len() > 1024
            }
            OutputItem::Thinking { text, .. } => text.len() > 1024,
            _ => false,
        };
        if hidden || !heavy {
            if let Some(slot) = preparation.slots.remove(&items.revisions()[idx].id) {
                crate::render_work::discard(slot);
            }
            return self.render_entry(idx, item, items.revisions()[idx], ctx, retain, force);
        }
        let key = Key {
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
        };
        let (body, pending) =
            preparation.update(key, || items.snapshot(idx).expect("existing document item"));
        if pending {
            self.pending_layout.insert(idx);
        }
        if let Some(mut entry) = body {
            self.access_clock = self.access_clock.wrapping_add(1);
            entry.last_used = self.access_clock;
            apply_work_fold_to_entry(&mut entry, fold, idx, ctx.panel_width);
            crate::render_work::discard(std::mem::replace(&mut self.entries[idx], entry));
            self.retention_dirty = true;
        } else if self.entries[idx].revision.id != items.revisions()[idx].id
            || self.entries[idx].revision.source_generation
                != items.revisions()[idx].source_generation
        {
            // A local pending row carries no copied source or executable Work item.
            let mut entry = ItemCacheEntry {
                revision: items.revisions()[idx],
                rows: 1,
                lines: Some(Arc::from([crate::loading::line("", 0, ctx.panel_width)])),
                ..Default::default()
            };
            apply_work_fold_to_entry(&mut entry, fold, idx, ctx.panel_width);
            self.entries[idx] = entry;
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
        cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            document,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 30,
                follow_tail_rows: None,
            },
        );
    }

    fn settle(cache: &mut LayoutCache, document: &FlowDocument) {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            update(cache, document);
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
}
