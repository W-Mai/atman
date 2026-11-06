//! Source-owned artifact preparation after a committed reader boundary.

use std::{
    io,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use atman_runtime::{
    flow_runtime::{FlowView, FlowViewReader},
    flow_store::FlowRef,
    tools::tool_output::OutputStore,
};
use futures::{executor::ThreadPool, future::RemoteHandle, task::SpawnExt};
use tokio::sync::watch;

use crate::history::{MaterializedTools, ToolMaterializer};

fn workers() -> ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-artifacts-")
            .create()
            .expect("create artifact workers")
    })
    .clone()
}

struct Captured {
    reader: Box<FlowViewReader>,
    materializer: ToolMaterializer,
    result: io::Result<MaterializedTools>,
}

enum State {
    Reading {
        reader: Box<FlowViewReader>,
        materializer: ToolMaterializer,
        tools: Option<Box<MaterializedTools>>,
    },
    Preparing {
        work: Option<RemoteHandle<Captured>>,
        cancelled: Arc<AtomicBool>,
        full: bool,
    },
}

/// A retained reader with immutable tool artifacts and source-bound read progress.
pub struct LiveSource {
    state: Option<State>,
    address: FlowRef,
    output: Option<Arc<OutputStore>>,
    output_bound: bool,
    pending_offset: Option<usize>,
    refresh_requested: bool,
    reader_progress: watch::Receiver<Option<Instant>>,
    progress: watch::Sender<Option<Instant>>,
    pool: ThreadPool,
}

impl LiveSource {
    pub(super) fn new(reader: FlowViewReader) -> Self {
        let reader_progress = reader.subscribe_read_progress();
        let (progress, _) = watch::channel(reader.read_started_at());
        Self {
            address: reader.address().clone(),
            state: Some(State::Reading {
                reader: Box::new(reader),
                materializer: ToolMaterializer::default(),
                tools: None,
            }),
            output: None,
            output_bound: false,
            pending_offset: None,
            refresh_requested: false,
            reader_progress,
            progress,
            pool: workers(),
        }
    }

    pub(super) fn set_output(&mut self, output: Option<Arc<OutputStore>>) {
        if !self.output_bound {
            self.output = output;
            self.output_bound = true;
        }
    }

    pub(super) fn address(&self) -> &FlowRef {
        &self.address
    }

    pub(super) fn read_started_at(&self) -> Option<Instant> {
        *self.progress.borrow()
    }

    pub(super) fn subscribe_read_progress(&self) -> watch::Receiver<Option<Instant>> {
        self.progress.subscribe()
    }

    pub(super) fn set_pending_offset(&mut self, offset: usize) {
        if let Some(State::Reading { reader, .. }) = &mut self.state {
            reader.set_pending_offset(offset);
        } else {
            self.pending_offset = Some(offset);
        }
    }

    pub(super) fn current(&self) -> io::Result<super::FlowViewContent<'_>> {
        if let Some(State::Reading {
            reader,
            tools: Some(tools),
            ..
        }) = &self.state
            && let Some(view) = reader.view()
        {
            return Ok(super::FlowViewContent::Materialized { view, tools });
        }
        Err(io::Error::other("live artifacts are not prepared"))
    }

    pub(super) async fn next(&mut self, refresh: bool) -> io::Result<super::FlowViewContent<'_>> {
        self.refresh_requested |= refresh;
        if let Some(State::Reading { reader, .. }) = &mut self.state {
            let result = read_boundary(
                reader,
                &mut self.reader_progress,
                &self.progress,
                self.refresh_requested,
            )
            .await;
            if let Err(error) = result {
                self.progress.send_replace(None);
                return Err(error);
            }
            if self.read_started_at().is_none() {
                self.progress.send_replace(Some(Instant::now()));
            }
            self.prepare(self.refresh_requested);
        }
        loop {
            let (captured, full) = match &mut self.state {
                Some(State::Preparing { work, full, .. }) => {
                    let captured = work.as_mut().expect("retained artifact task").await;
                    *work = None;
                    (captured, *full)
                }
                _ => return Err(io::Error::other("live source has no artifact task")),
            };
            let Captured {
                mut reader,
                materializer,
                result,
            } = captured;
            if let Some(offset) = self.pending_offset.take() {
                reader.set_pending_offset(offset);
            }
            let (tools, error) = match result {
                Ok(tools) => (Some(Box::new(tools)), None),
                Err(error) => (None, Some(error)),
            };
            self.state = Some(State::Reading {
                reader,
                materializer,
                tools,
            });
            if self.refresh_requested && !full {
                self.prepare(true);
                continue;
            }
            self.progress.send_replace(None);
            self.refresh_requested = false;
            if let Some(error) = error {
                return Err(error);
            }
            return self.current();
        }
    }

    fn prepare(&mut self, full: bool) {
        self.output_bound = true;
        let Some(State::Reading {
            reader,
            mut materializer,
            tools,
        }) = self.state.take()
        else {
            unreachable!("reader is retained until capture starts")
        };
        let output = self.output.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let work = self
            .pool
            .spawn_with_handle(async move {
                drop(tools);
                let result = if stop.load(Ordering::Relaxed) {
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "artifact observation closed",
                    ))
                } else {
                    atman_runtime::capture_blocking_panic(|| {
                        let view: &FlowView = reader.view().ok_or_else(|| {
                            io::Error::other("artifact capture has no committed view")
                        })?;
                        materializer.capture(view, output.as_deref(), full)
                    })
                    .unwrap_or_else(|| Err(io::Error::other("tool artifact preparation failed")))
                };
                Captured {
                    reader,
                    materializer,
                    result,
                }
            })
            .expect("artifact workers remain open");
        self.state = Some(State::Preparing {
            work: Some(work),
            cancelled,
            full,
        });
    }
}

async fn read_boundary(
    reader: &mut FlowViewReader,
    activity: &mut watch::Receiver<Option<Instant>>,
    progress: &watch::Sender<Option<Instant>>,
    refresh: bool,
) -> io::Result<()> {
    let read = async {
        if refresh {
            reader.refresh().await
        } else {
            reader.next().await
        }
        .map(|_| ())
    };
    tokio::pin!(read);
    let mut closed = false;
    loop {
        tokio::select! {
            result = &mut read => return result,
            changed = activity.changed(), if !closed => {
                if changed.is_err() { closed = true; }
                else if let Some(started) = *activity.borrow_and_update() {
                    let idle = progress.borrow().is_none();
                    if idle { progress.send_replace(Some(started)); }
                }
            }
        }
    }
}

impl Drop for LiveSource {
    fn drop(&mut self) {
        if let Some(State::Preparing { cancelled, .. }) = &self.state {
            cancelled.store(true, Ordering::Relaxed);
        }
        let state = self.state.take();
        self.pool.spawn_ok(async move { drop(state) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        flow_runtime::FlowWorker,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowTransition, RuntimeStore,
            SessionDescriptor, publication::MessagePublication,
        },
        message::{Message, MessageOrigin, MessagePart, MessageRole},
        source_program::RevisionBundle,
    };
    use std::{collections::HashSet, num::NonZeroUsize, sync::mpsc, time::Duration};

    #[tokio::test]
    async fn native_artifacts_retain_cancelled_reads_and_render_only_captured_sources() {
        let directory = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let session_id = uuid::Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id: session_id,
                configuration: Default::default(),
                project: None,
                title: "Artifact contract".into(),
            })
            .unwrap();
        let mut store = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: session_id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: uuid::Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        store
            .transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )
            .unwrap();
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
            store.state().state()
        else {
            panic!("active lease")
        };
        let lease = active.lease.clone();
        let log = directory.path().join("task.log");
        std::fs::write(&log, "[out] captured log\n[err] 汉字\n").unwrap();
        let output = Arc::new(OutputStore::at(directory.path().join("outputs")));
        let full = "captured registered output\n";
        let id = output.register("test", full).unwrap();
        let reference = serde_json::json!({ "content": "captured", "output_id": id, "total_bytes": full.len(), "next": {"mode":"bytes","offset":full.len(),"has_more":false} }).to_string();
        for (role, parts) in [
            (
                MessageRole::Assistant,
                vec![
                    MessagePart::ToolUse {
                        id: "bash".into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({"cmd":"echo captured"}),
                        intent: None,
                    },
                    MessagePart::ToolUse {
                        id: "read".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({"path":"notes.txt"}),
                        intent: None,
                    },
                ],
            ),
            (
                MessageRole::Tool,
                vec![
                    MessagePart::ToolResult {
                        tool_use_id: "bash".into(),
                        content: serde_json::json!({"handle":"task", "log_path":log}).to_string(),
                        is_error: false,
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "read".into(),
                        content: reference,
                        is_error: false,
                    },
                ],
            ),
        ] {
            store
                .publish_message(MessagePublication {
                    id: uuid::Uuid::now_v7(),
                    lease: lease.clone(),
                    run: lease.run.clone(),
                    context_run: None,
                    message: Message {
                        role,
                        origin: MessageOrigin::Internal,
                        turn_id: lease.turn.clone(),
                        parts,
                    },
                })
                .unwrap();
        }
        let worker = FlowWorker::start(store).await.unwrap();
        let reader = worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap();
        let mut source = LiveSource::new(reader);
        source.set_output(Some(output.clone()));
        let pool = ThreadPool::builder().pool_size(2).create().unwrap();
        source.pool = pool.clone();
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
        let mut progress = source.subscribe_read_progress();
        assert!(
            tokio::time::timeout(Duration::from_millis(40), source.next(false))
                .await
                .is_err()
        );
        let started = source
            .read_started_at()
            .expect("artifact progress survives cancelled polling");
        assert!(matches!(source.state, Some(State::Preparing { .. })));
        source.set_pending_offset(64);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), source.next(true))
                .await
                .is_err()
        );
        assert_eq!(source.read_started_at(), Some(started));
        assert!(progress.borrow_and_update().is_some());
        for release in releases {
            release.send(()).unwrap();
        }
        let content = tokio::time::timeout(Duration::from_secs(5), source.next(false))
            .await
            .unwrap()
            .unwrap();
        std::fs::write(&log, "changed original").unwrap();
        std::fs::remove_dir_all(directory.path().join("outputs")).unwrap();
        let mut document = crate::flow_view::FlowViewModel::default();
        document.sync_content(content, Some(&output)).unwrap();
        let mut seen = HashSet::new();
        for item in document.iter() {
            if let crate::app::OutputItem::ToolDispatch { calls } = item {
                for call in calls {
                    match call.detail.as_deref().unwrap() {
                        crate::app::OutputItem::Bash { output, .. } => {
                            assert_eq!(output, "captured log\n汉字");
                            seen.insert("log");
                        }
                        crate::app::OutputItem::FsDetail {
                            view: crate::app::FsDetail::Read { content, .. },
                            ..
                        } => {
                            assert_eq!(content, full);
                            seen.insert("registered");
                        }
                        _ => panic!("unexpected restored detail"),
                    }
                }
            }
        }
        assert_eq!(seen.len(), 2);
        assert!(source.read_started_at().is_none());
        assert!(progress.borrow_and_update().is_none());
        let page = tokio::time::timeout(Duration::from_secs(3), source.next(false))
            .await
            .unwrap()
            .unwrap();
        let super::super::FlowViewContent::Materialized { view, tools } = page else {
            panic!("prepared source content")
        };
        assert!(
            crate::flow_view::FlowViewModel::default()
                .sync_content(
                    super::super::FlowViewContent::Materialized { view, tools },
                    Some(&output),
                )
                .is_err(),
            "a fresh document requires complete artifact coverage"
        );
        assert!(
            !document
                .sync_content(
                    super::super::FlowViewContent::Materialized { view, tools },
                    Some(&output)
                )
                .unwrap()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), source.next(false))
                .await
                .is_err()
        );
        assert!(
            source.read_started_at().is_none(),
            "idle commit waiting must not animate"
        );
        drop(source);
        worker.shutdown().await.unwrap();
    }
}
