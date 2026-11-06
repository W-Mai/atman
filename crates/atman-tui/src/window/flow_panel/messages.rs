//! Retained background restoration for one legacy child view.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

use futures::executor::ThreadPool;

use crate::{app::OutputRevision, history::PreparedMessages};

pub(crate) trait MessageSource: Send + 'static {
    fn messages(&self) -> Option<&[atman_runtime::message::Message]>;
}

impl MessageSource for Vec<atman_runtime::message::Message> {
    fn messages(&self) -> Option<&[atman_runtime::message::Message]> {
        Some(self)
    }
}

impl MessageSource for crate::flow_view::OutputCapture {
    fn messages(&self) -> Option<&[atman_runtime::message::Message]> {
        match self.item()? {
            crate::app::OutputItem::SubAgentActivity { messages, .. } => Some(messages),
            _ => None,
        }
    }
}

type Key = (u64, u64);

fn key(revision: OutputRevision) -> Key {
    (revision.id, revision.semantic)
}

fn workers() -> ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-child-messages-")
            .create()
            .expect("create child message workers")
    })
    .clone()
}

struct Pending {
    key: Key,
    result: Option<mpsc::Receiver<Result<PreparedMessages, String>>>,
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

pub(crate) struct Preparation {
    source: Option<String>,
    applied: Option<Key>,
    pending: Option<Pending>,
    failed: Option<(Key, String)>,
    pool: ThreadPool,
}

impl Default for Preparation {
    fn default() -> Self {
        Self {
            source: None,
            applied: None,
            pending: None,
            failed: None,
            pool: workers(),
        }
    }
}

impl Preparation {
    pub fn bind(&mut self, handle: &str) -> bool {
        if self.source.as_deref() == Some(handle) {
            return false;
        }
        let changed = self.source.is_some();
        self.clear();
        self.source = Some(handle.to_owned());
        changed
    }

    pub fn discard(&self, value: impl Send + 'static) {
        self.pool.spawn_ok(async move { drop(value) });
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
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
        self.applied = None;
        self.source = None;
    }

    pub fn update<S: MessageSource>(
        &mut self,
        handle: &str,
        revision: OutputRevision,
        capture: impl FnOnce() -> (S, Option<atman_runtime::tools::tool_output::OutputStore>),
    ) -> Option<PreparedMessages> {
        self.bind(handle);
        let current = key(revision);
        if self.failed.as_ref().is_some_and(|(old, _)| *old != current) {
            self.failed = None;
        }
        if let Some(pending) = &mut self.pending {
            let received = match pending.result.as_ref().expect("retained result").try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("History preparation stopped".into()))
                }
            };
            if let Some(result) = received {
                let completed = pending.key;
                self.pending = None;
                if completed == current {
                    match result {
                        Ok(prepared) => {
                            self.applied = Some(current);
                            return Some(prepared);
                        }
                        Err(error) => self.failed = Some((current, error)),
                    }
                } else {
                    self.pool.spawn_ok(async move { drop(result) });
                }
            }
        }
        if self.pending.is_some() || self.applied == Some(current) || self.failed.is_some() {
            return None;
        }
        let (messages, output) = capture();
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        self.pool.spawn_ok(async move {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let result = atman_runtime::capture_blocking_panic(|| {
                messages
                    .messages()
                    .map(|messages| PreparedMessages::capture(revision, messages, output.as_ref()))
            })
            .flatten()
            .ok_or_else(|| "History preparation failed".to_owned());
            let _ = sender.send(result);
        });
        self.pending = Some(Pending {
            key: current,
            result: Some(receiver),
            cancelled,
            pool: self.pool.clone(),
        });
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::message::{Message, MessagePart, MessageRole};
    use std::{
        cell::Cell,
        time::{Duration, Instant},
    };

    fn settle(
        state: &mut Preparation,
        revision: OutputRevision,
        messages: &[Message],
        captures: &Cell<usize>,
    ) -> PreparedMessages {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(ready) = state.update("child", revision, || {
                captures.set(captures.get() + 1);
                (messages.to_vec(), None)
            }) {
                return ready;
            }
            assert!(state.error().is_none());
            assert!(Instant::now() < end, "child message capture did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn restoration_retains_work_rejects_stale_sources_and_installs_without_files() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("child.log");
        std::fs::write(&log, "[out] captured child log\n").unwrap();
        let turn = atman_rt::TurnId::now();
        let mut call = Message::assistant_text(turn.clone(), "");
        call.parts = vec![MessagePart::ToolUse {
            id: "call".into(),
            name: "bash.spawn".into(),
            input: serde_json::json!({}),
            intent: None,
        }];
        let mut result = call.clone();
        result.role = MessageRole::Tool;
        result.parts = vec![MessagePart::ToolResult {
            tool_use_id: "call".into(),
            is_error: false,
            content: serde_json::json!({"handle":"process", "log_path":log}).to_string(),
        }];
        let messages = vec![call, result];
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
            pool,
            ..Default::default()
        };
        let captures = Cell::new(0);
        let mut revision = OutputRevision {
            id: 1,
            semantic: 1,
            layout: 1,
            ..Default::default()
        };
        assert!(
            state
                .update("child", revision, || {
                    captures.set(1);
                    (vec![Message::assistant_text(turn, "stale")], None)
                })
                .is_none()
        );
        revision.semantic += 1;
        assert!(
            state
                .update::<Vec<atman_runtime::Message>>("child", revision, || panic!(
                    "coalesce before recapture"
                ))
                .is_none()
        );
        assert!(state.is_pending());
        for release in releases {
            release.send(()).unwrap();
        }
        let prepared = settle(&mut state, revision, &messages, &captures);
        assert_eq!(captures.get(), 2);
        std::fs::remove_file(&log).unwrap();
        let mut document = crate::flow_view::FlowViewModel::default();
        document.restore_prepared_messages(prepared);
        let crate::app::OutputItem::ToolDispatch { calls } = &document[0] else {
            panic!("restored calls")
        };
        let Some(crate::app::OutputItem::Bash { output, .. }) = calls[0].detail.as_deref() else {
            panic!("restored log")
        };
        assert_eq!(output, "captured child log");
        revision.layout += 1;
        assert!(
            state
                .update::<Vec<atman_runtime::Message>>("child", revision, || panic!(
                    "layout is not a source change"
                ))
                .is_none()
        );
        assert!(!state.is_pending());
        assert!(!state.bind("child"));
        assert!(state.bind("other-child"));
        assert!(state.applied.is_none());
    }

    #[test]
    fn closed_observation_cancels_queued_capture_and_disconnection_can_retry() {
        let pool = ThreadPool::builder().pool_size(1).create().unwrap();
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            wait.recv().unwrap();
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut state = Preparation {
            pool: pool.clone(),
            ..Default::default()
        };
        let revision = OutputRevision {
            id: 1,
            semantic: 1,
            ..Default::default()
        };
        state.update("child", revision, || (Vec::new(), None));
        let cancelled = state.pending.as_ref().unwrap().cancelled.clone();
        drop(state);
        assert!(cancelled.load(Ordering::Relaxed));
        release.send(()).unwrap();
        let mut state = Preparation {
            pool,
            ..Default::default()
        };
        state.bind("child");
        let (sender, receiver) = mpsc::channel();
        state.pending = Some(Pending {
            key: key(revision),
            result: Some(receiver),
            cancelled: Arc::new(AtomicBool::new(false)),
            pool: state.pool.clone(),
        });
        drop(sender);
        assert!(
            state
                .update::<Vec<atman_runtime::Message>>("child", revision, || panic!(
                    "failure must remain visible"
                ))
                .is_none()
        );
        assert!(state.error().is_some());
        assert!(!state.is_pending());
        state.retry();
        settle(&mut state, revision, &[], &Cell::new(0));
        assert!(state.error().is_none());

        use crate::wm::component::{WindowComponent, WmEvent, WmEventResult};
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        state.failed = Some((key(revision), "Unavailable".into()));
        let mut panel = super::super::FlowPanelContent {
            document: Default::default(),
            interaction: Default::default(),
            goal_entry: None,
            goal_rect: Default::default(),
            goal_hover: false,
            handle: "child".into(),
            scroll: 0,
            render_cache: None,
            output_store: None,
            selection_projection: None,
            restoration: state,
            retry_rect: Some(ratatui::layout::Rect::new(3, 4, 16, 3)),
            retry_hover: false,
        };
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut context = crate::wm::component::EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
        ] {
            assert!(matches!(
                panel.handle_event(
                    &WmEvent::Mouse(MouseEvent {
                        kind,
                        column: 4,
                        row: 5,
                        modifiers: KeyModifiers::NONE,
                    }),
                    &mut context
                ),
                WmEventResult::Consumed(_)
            ));
            assert!(panel.retry_hover);
        }
        assert!(panel.restoration.error().is_none());
        panel.restoration.failed = Some((key(revision), "Unavailable".into()));
        assert!(matches!(
            panel.handle_event(
                &WmEvent::Key(crate::keys::KeyAction::Char('r')),
                &mut context
            ),
            WmEventResult::Consumed(_)
        ));
        assert!(panel.restoration.error().is_none());
    }
}
