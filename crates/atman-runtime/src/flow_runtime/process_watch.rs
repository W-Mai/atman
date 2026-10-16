//! Instance-owned process observation. Producers retain cursors, not a second notification inbox.

use std::{
    collections::{BTreeMap, HashSet},
    io,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use uuid::Uuid;

use super::{Command, FlowWorker, request};
use crate::flow_store::{
    FlowStore, invalid,
    process_watch::{
        ProcessMatch, ProcessReport, ProcessSignal, ProcessSource, ProcessWatch,
        ProcessWatchRecord, ProcessWatchStatus,
    },
};
use crate::task_registry::TaskKind;
use crate::tools::{
    bash_bg::BgEntry,
    term::{TermEntry, TermStreamEvent, TerminalScreen, find_pattern_columns},
};
use crate::watch::OutputCursor;

type Authorize = Box<dyn FnOnce() -> io::Result<()> + Send>;

struct Registration {
    watch: ProcessWatch,
    authorize: Authorize,
    reply: oneshot::Sender<io::Result<ProcessWatchRecord>>,
}

struct Producer {
    registrations: mpsc::Sender<Registration>,
    task: AbortOnDropHandle<()>,
    refresh: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
pub(super) struct ProcessObservers(Mutex<BTreeMap<ProcessSource, Producer>>);

impl ProcessObservers {
    pub fn refresh(&self) {
        for producer in self.0.lock().unwrap().values() {
            producer.refresh.notify_one();
        }
    }
    pub fn stopped(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .values()
            .all(|producer| producer.task.is_finished())
    }

    pub async fn join(&self) {
        let producers = std::mem::take(&mut *self.0.lock().unwrap());
        for (_, producer) in producers {
            let _ = producer.task.await;
        }
    }
}

enum Feed {
    Bash {
        entry: Arc<BgEntry>,
        changes: watch::Receiver<()>,
    },
    Terminal {
        entry: Arc<TermEntry>,
        changes: broadcast::Receiver<TermStreamEvent>,
    },
}

enum Snapshot {
    Bash {
        bytes: Vec<u8>,
        base: usize,
        running: bool,
    },
    Terminal {
        screen: TerminalScreen,
        revision: u64,
        running: bool,
    },
}

impl Snapshot {
    fn running(&self) -> bool {
        match self {
            Self::Bash { running, .. } | Self::Terminal { running, .. } => *running,
        }
    }
}

impl Feed {
    fn snapshot(&self) -> Snapshot {
        match self {
            Self::Bash { entry, .. } => {
                let (bytes, base, running) = entry.watch_snapshot();
                Snapshot::Bash {
                    bytes,
                    base,
                    running,
                }
            }
            Self::Terminal { entry, .. } => {
                let (screen, revision, running) = entry.watch_snapshot();
                Snapshot::Terminal {
                    screen,
                    revision,
                    running,
                }
            }
        }
    }

    async fn next(&mut self) -> Snapshot {
        match self {
            Self::Bash { changes, .. } => {
                let _ = changes.changed().await;
                self.snapshot()
            }
            Self::Terminal { entry, changes } => match changes.recv().await {
                Ok(TermStreamEvent::Chunk {
                    screen,
                    revision,
                    state,
                    ..
                }) => Snapshot::Terminal {
                    screen,
                    revision,
                    running: state.is_running(),
                },
                _ => {
                    let (screen, revision, running) = entry.watch_snapshot();
                    Snapshot::Terminal {
                        screen,
                        revision,
                        running,
                    }
                }
            },
        }
    }
}

#[derive(Default)]
struct Matcher {
    cursor: Option<OutputCursor>,
    revision: Option<u64>,
    cells: HashSet<(u16, u16)>,
}

impl Matcher {
    fn matches(
        &mut self,
        watch: &ProcessWatch,
        snapshot: &Snapshot,
    ) -> io::Result<Vec<ProcessMatch>> {
        let mut matches = Vec::new();
        match snapshot {
            Snapshot::Bash { bytes, base, .. } => {
                if self
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| *base > cursor.position())
                {
                    return Err(invalid("process output history gap"));
                }
                let cursor = self.cursor.get_or_insert_with(|| OutputCursor::at(*base));
                while let Some(position) = cursor.find(bytes, *base, &watch.pattern) {
                    if matches.is_empty() {
                        matches.push(ProcessMatch {
                            watcher: watch.id,
                            text: watch.pattern.clone(),
                            row: None,
                            col: u16::try_from(position).ok(),
                        });
                    }
                }
            }
            Snapshot::Terminal {
                screen, revision, ..
            } => {
                if self.revision.is_some_and(|seen| seen >= *revision) {
                    return Ok(matches);
                }
                let mut cells = HashSet::new();
                for row in 0..screen.rows {
                    for col in find_pattern_columns(screen, row, &watch.pattern) {
                        if matches.is_empty() && !self.cells.contains(&(row, col)) {
                            matches.push(ProcessMatch {
                                watcher: watch.id,
                                text: watch.pattern.clone(),
                                row: Some(row),
                                col: Some(col),
                            });
                        }
                        cells.insert((row, col));
                    }
                }
                self.cells = cells;
                self.revision = Some(*revision);
            }
        }
        Ok(matches)
    }
}

struct Observation {
    source: ProcessSource,
    feed: Feed,
    commands: mpsc::WeakSender<Command>,
    watches: BTreeMap<Uuid, (ProcessWatch, Matcher)>,
    position: u64,
    ended: bool,
}

impl Observation {
    async fn store<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let sender = self
            .commands
            .upgrade()
            .ok_or_else(|| invalid("process observation owner closed"))?;
        request(&sender, operation).await
    }

    async fn publish(&mut self, signal: ProcessSignal) -> io::Result<()> {
        self.position = self
            .position
            .checked_add(1)
            .ok_or_else(|| invalid("process observation position exhausted"))?;
        let report = ProcessReport {
            source: self.source.clone(),
            position: self.position,
            observed_at: Utc::now(),
            signal,
        };
        let active = self
            .store(move |store| {
                store.observe_process(report)?;
                Ok(store
                    .process_watches()
                    .filter(|record| record.status == ProcessWatchStatus::Active)
                    .map(|record| record.watch.id)
                    .collect::<HashSet<_>>())
            })
            .await?;
        self.watches.retain(|id, _| active.contains(id));
        Ok(())
    }

    async fn scan(&mut self, snapshot: Snapshot) -> io::Result<()> {
        let mut matches = Vec::new();
        for (watch, matcher) in self.watches.values_mut() {
            match matcher.matches(watch, &snapshot) {
                Ok(found) => matches.extend(found),
                Err(_) => {
                    self.publish(ProcessSignal::HistoryGap).await?;
                    return Ok(());
                }
            }
        }
        if !matches.is_empty() {
            self.publish(ProcessSignal::Matches(matches)).await?;
        }
        if !snapshot.running() && !self.ended {
            self.ended = true;
            self.publish(ProcessSignal::Exited).await?;
        }
        Ok(())
    }

    async fn register(&mut self, registration: Registration) -> io::Result<()> {
        let Registration {
            watch,
            authorize,
            reply,
        } = registration;
        // Capture the initial replaceable state before acknowledging registration. The feed was subscribed before this snapshot.
        let snapshot = self.feed.snapshot();
        let requested = watch.clone();
        let position = self.position;
        let result = self
            .store(move |store| {
                authorize()?;
                store.register_process_watch(requested, position)
            })
            .await;
        match result {
            Ok(record) => {
                if record.status == ProcessWatchStatus::Active {
                    self.watches
                        .entry(watch.id)
                        .or_insert_with(|| (watch, Matcher::default()));
                }
                let _ = reply.send(Ok(record));
                self.scan(snapshot).await
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                Ok(())
            }
        }
    }

    async fn expire(&mut self) -> io::Result<()> {
        let active = self
            .store(|store| {
                store.expire_process_watches(Utc::now())?;
                Ok(store
                    .process_watches()
                    .filter(|record| record.status == ProcessWatchStatus::Active)
                    .map(|record| record.watch.id)
                    .collect::<HashSet<_>>())
            })
            .await?;
        self.watches.retain(|id, _| active.contains(id));
        Ok(())
    }

    async fn refresh(&mut self) -> io::Result<()> {
        let active = self
            .store(|store| {
                Ok(store
                    .process_watches()
                    .filter(|record| record.status == ProcessWatchStatus::Active)
                    .map(|record| record.watch.id)
                    .collect::<HashSet<_>>())
            })
            .await?;
        self.watches.retain(|id, _| active.contains(id));
        Ok(())
    }

    async fn run(
        mut self,
        mut registrations: mpsc::Receiver<Registration>,
        cancel: CancellationToken,
        refresh: Arc<tokio::sync::Notify>,
    ) {
        loop {
            let deadline = self
                .watches
                .values()
                .filter_map(|(watch, _)| watch.expires_at)
                .min();
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                registration = registrations.recv() => match registration { Some(registration) => self.register(registration).await, None => return },
                _ = refresh.notified() => self.refresh().await,
                _ = async { match deadline { Some(deadline) => tokio::time::sleep((deadline - Utc::now()).to_std().unwrap_or_default()).await, None => std::future::pending().await } } => self.expire().await,
                snapshot = self.feed.next(), if !self.ended && !self.watches.is_empty() => self.scan(snapshot).await,
            };
            if let Err(error) = result {
                if !cancel.is_cancelled() {
                    crate::notify!(error, "process observation stopped: {error}");
                }
                return;
            }
            tokio::task::yield_now().await;
        }
    }
}

impl FlowWorker {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn watch_process(
        &self,
        id: Uuid,
        handle: String,
        pattern: String,
        once: bool,
        expires_at: Option<DateTime<Utc>>,
        authorize: Authorize,
    ) -> io::Result<ProcessWatchRecord> {
        let session_id = self.address.session_id.to_string();
        let task = self
            .resources
            .tasks()
            .lookup_by_handle(&handle)
            .ok_or_else(|| invalid("process handle is not owned by this instance"))?;
        let (source, feed) = match task.kind {
            TaskKind::Bash => {
                let entry = self
                    .resources
                    .background()
                    .lookup(&handle, &session_id)
                    .map_err(invalid)?;
                let incarnation = entry
                    .task_id
                    .as_ref()
                    .ok_or_else(|| invalid("process has no resource identity"))?
                    .0;
                let changes = entry.output_changes();
                (
                    ProcessSource::Bash {
                        incarnation,
                        handle,
                    },
                    Feed::Bash { entry, changes },
                )
            }
            TaskKind::Terminal => {
                let entry = self
                    .resources
                    .terminals()
                    .lookup(&handle, &session_id)
                    .map_err(invalid)?;
                let incarnation = entry
                    .task_id
                    .lock()
                    .unwrap()
                    .as_ref()
                    .ok_or_else(|| invalid("process has no resource identity"))?
                    .0;
                let changes = entry.stream_tx.subscribe();
                (
                    ProcessSource::Terminal {
                        incarnation,
                        handle,
                    },
                    Feed::Terminal { entry, changes },
                )
            }
            TaskKind::Flow => return Err(invalid("flow events require a flow subscription")),
        };
        let sender = {
            let mut producers = self.resources.observers.0.lock().unwrap();
            if self.resources.lifetime.is_cancelled() {
                return Err(invalid("process resources are closed"));
            }
            if let Some(producer) = producers.get(&source) {
                producer.registrations.clone()
            } else {
                let (registrations, receiver) = mpsc::channel(32);
                let observer = Observation {
                    source: source.clone(),
                    feed,
                    commands: self.commands.downgrade(),
                    watches: BTreeMap::new(),
                    position: 0,
                    ended: false,
                };
                let refresh = Arc::new(tokio::sync::Notify::new());
                let task = AbortOnDropHandle::new(tokio::spawn(observer.run(
                    receiver,
                    self.resources.lifetime.clone(),
                    refresh.clone(),
                )));
                producers.insert(
                    source.clone(),
                    Producer {
                        registrations: registrations.clone(),
                        task,
                        refresh,
                    },
                );
                registrations
            }
        };
        let (reply, response) = oneshot::channel();
        sender
            .send(Registration {
                watch: ProcessWatch {
                    id,
                    source,
                    pattern,
                    once,
                    expires_at,
                },
                authorize,
                reply,
            })
            .await
            .map_err(|_| invalid("process observer is unavailable"))?;
        response
            .await
            .map_err(|_| invalid("process observer stopped before registration acknowledgement"))?
    }
}
