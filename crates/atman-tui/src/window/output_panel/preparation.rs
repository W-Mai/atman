use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use crate::render_work::workers;
use futures::executor::ThreadPool;
use ratatui::text::Line;

use super::super::common::WindowTextSource;
use crate::{
    app::{OutputItem, OutputRevision},
    flow_view::OutputCapture,
    output::{DynamicPaint, RenderCtx},
    render_preferences::RenderPreferences,
    theme::Theme,
    wm::WindowId,
};

pub(super) fn discard(value: impl Send + 'static) {
    crate::render_work::discard(value);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Key {
    pub revision: OutputRevision,
    pub window: WindowId,
    pub width: u16,
    pub theme: Theme,
    pub preferences: RenderPreferences,
}

impl Key {
    fn same_source(self, other: Self) -> bool {
        self.window == other.window
            && self.revision.id == other.revision.id
            && self.revision.source_generation == other.revision.source_generation
    }
}

pub(super) struct Prepared {
    pub key: Key,
    pub lines: Vec<Line<'static>>,
    pub text: WindowTextSource,
    pub paint: DynamicPaint,
    animated: bool,
}

impl Prepared {
    fn new(key: Key, item: &OutputItem) -> Self {
        key.theme.with(|| {
            key.preferences.with(|| {
                let context = RenderCtx {
                    panel_width: key.width,
                    animation_frame: crate::output::LAYOUT_ANIMATION_FRAME,
                    ..RenderCtx::empty()
                };
                let lines = crate::output::render_item(item, &context);
                let paint = crate::output::dynamic_paint_for_item(item, &lines, &context);
                let mut copy_lines = lines.clone();
                crate::output::patch_animation_lines(&mut copy_lines, &paint, 0, 0);
                let text =
                    WindowTextSource::new(key.window, "tool-output", key.revision, &copy_lines);
                Self {
                    key,
                    lines,
                    text,
                    paint,
                    animated: item.has_dynamic_paint(),
                }
            })
        })
    }
}

struct Pending {
    key: Key,
    result: Option<mpsc::Receiver<Result<Prepared, String>>>,
    cancelled: Arc<AtomicBool>,
    pool: ThreadPool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        let result = self.result.take();
        self.pool.spawn_ok(async move { drop(result) });
    }
}

pub(super) struct Preparation {
    pending: Option<Pending>,
    pub ready: Option<Prepared>,
    failed: Option<(Key, String)>,
    pool: ThreadPool,
}

impl Default for Preparation {
    fn default() -> Self {
        Self {
            pending: None,
            ready: None,
            failed: None,
            pool: workers(),
        }
    }
}

impl Preparation {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn is_animating(&self) -> bool {
        self.is_pending()
            || (self.failed.is_none() && self.ready.as_ref().is_some_and(|ready| ready.animated))
    }

    pub fn error(&self) -> Option<&str> {
        self.failed.as_ref().map(|(_, error)| error.as_str())
    }

    pub fn retry(&mut self) {
        self.failed = None;
    }

    pub fn clear(&mut self) {
        self.pending = None;
        self.failed = None;
        if let Some(ready) = self.ready.take() {
            self.pool.spawn_ok(async move { drop(ready) });
        }
    }

    pub fn update<S: Into<OutputCapture>>(&mut self, key: Key, source: impl FnOnce() -> S) {
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| !ready.key.same_source(key))
        {
            let ready = self.ready.take();
            self.pool.spawn_ok(async move { drop(ready) });
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|(failed, _)| *failed != key)
        {
            self.failed = None;
        }
        if let Some(pending) = &mut self.pending {
            let result = match pending.result.as_ref().expect("pending result").try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Background rendering stopped".into()))
                }
            };
            if let Some(result) = result {
                let completed = pending.key;
                self.pending = None;
                if completed == key {
                    match result {
                        Ok(ready) => {
                            let old = self.ready.replace(ready);
                            self.pool.spawn_ok(async move { drop(old) });
                            self.failed = None;
                        }
                        Err(error) => self.failed = Some((key, error)),
                    }
                } else {
                    self.pool.spawn_ok(async move { drop(result) });
                }
            }
        }
        if self.pending.is_some()
            || self.ready.as_ref().is_some_and(|ready| ready.key == key)
            || self.failed.is_some()
        {
            return;
        }
        let item: OutputCapture = source().into();
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        self.pool.spawn_ok(async move {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let result = atman_runtime::capture_blocking_panic(|| {
                item.item().map(|item| Prepared::new(key, item))
            })
            .flatten()
            .ok_or_else(|| "Output preparation failed".to_owned());
            let _ = sender.send(result);
            crate::render_work::notify();
        });
        self.pending = Some(Pending {
            key,
            result: Some(receiver),
            cancelled,
            pool: self.pool.clone(),
        });
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
    use std::time::{Duration, Instant};

    fn key() -> Key {
        Key {
            revision: OutputRevision {
                id: 1,
                layout: 1,
                ..Default::default()
            },
            window: WindowId(1),
            width: 80,
            theme: crate::theme::theme(),
            preferences: RenderPreferences::default(),
        }
    }

    fn item(text: &str) -> Arc<OutputItem> {
        Arc::new(OutputItem::AssistantMd {
            md: text.into(),
            streaming: false,
            retried: false,
        })
    }

    fn settle(state: &mut Preparation, key: Key, source: &Arc<OutputItem>) {
        let until = Instant::now() + Duration::from_secs(3);
        while state.ready.as_ref().is_none_or(|ready| ready.key != key) {
            state.update(key, || source.clone());
            assert!(state.error().is_none());
            assert!(Instant::now() < until, "render worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn preparation_stays_pending_without_blocking_and_rejects_stale_display_sources() {
        let pool = ThreadPool::builder().pool_size(2).create().unwrap();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (started, ready) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            pool.spawn_ok(async move {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            releases.push(release);
        }
        let mut state = Preparation {
            pool: pool.clone(),
            pending: None,
            ready: None,
            failed: None,
        };
        let mut latest = key();
        state.update(latest, || item("old source"));
        assert!(state.is_pending() && state.is_animating());
        assert!(state.ready.is_none());
        latest.width = 31;
        latest.preferences.math = false;
        latest.theme.accent = crate::theme::ThemeColor::new(ratatui::style::Color::Magenta);
        latest.revision.source_generation = 2;
        state.update::<Arc<OutputItem>>(latest, || {
            panic!("in-flight requests must coalesce before capture")
        });

        // Closing another queued reader does not wait for either busy CPU worker.
        let cancelled_source = item("cancelled reader");
        let weak = Arc::downgrade(&cancelled_source);
        let mut closed = Preparation {
            pool,
            pending: None,
            ready: None,
            failed: None,
        };
        closed.update(key(), || cancelled_source);
        drop(closed);
        for release in releases {
            release.send(()).unwrap();
        }

        let source = item("new source 汉 e\u{301} 👩‍💻");
        settle(&mut state, latest, &source);
        let ready = state.ready.as_ref().unwrap();
        let text = ready
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("new source") && !text.contains("old source"));
        assert_eq!(ready.key, latest);
        assert!(!state.is_pending() && !state.is_animating());
        for _ in 0..10 {
            state.update::<Arc<OutputItem>>(latest, || panic!("unchanged content was recaptured"));
        }
        let until = Instant::now() + Duration::from_secs(2);
        while weak.upgrade().is_some() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
    }

    #[test]
    fn preparation_failure_is_local_stops_animation_and_retries() {
        let mut state = Preparation::default();
        let source = item("previous content");
        let mut request = key();
        settle(&mut state, request, &source);
        request.width -= 1;
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        state.pending = Some(Pending {
            key: request,
            result: Some(receiver),
            cancelled: Arc::new(AtomicBool::new(false)),
            pool: state.pool.clone(),
        });
        state.update::<Arc<OutputItem>>(request, || panic!("failed requests must wait for retry"));
        assert_eq!(state.error(), Some("Background rendering stopped"));
        assert!(state.ready.is_some() && !state.is_animating());
        state.retry();
        settle(&mut state, request, &source);
        assert!(state.error().is_none());
        let mut replacement = request;
        replacement.revision.id += 1;
        state.update(replacement, || source.clone());
        assert!(
            state.ready.is_none(),
            "another source cannot display the retained body"
        );
    }

    #[test]
    fn prepared_dynamic_rows_use_original_paint_and_copy_no_internal_markers() {
        let source = OutputItem::Bash {
            handle: "task".into(),
            title: None,
            command: Some("echo result".into()),
            output: "result".into(),
            done: false,
            expanded: true,
        };
        let ready = Prepared::new(key(), &source);
        let mut first = ready.lines.clone();
        let mut second = first.clone();
        crate::output::patch_animation_lines(&mut first, &ready.paint, 0, 0);
        crate::output::patch_animation_lines(&mut second, &ready.paint, 1, 0);
        assert_ne!(first, second);
        let projection = ready
            .text
            .visible(ratatui::layout::Rect::new(0, 0, 80, 40), 0);
        assert!(!projection.surfaces[0].source.source.contains('\u{e000}'));
    }
}
