//! Instance-owned process observation. Producers retain cursors, not a second notification inbox.

use std::{
    collections::{BTreeMap, HashSet},
    io,
    sync::{Arc, Mutex, Weak},
};

use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use uuid::Uuid;

use super::{Command, FlowWorker, request};
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::{
    FlowStore, invalid,
    process_watch::{
        ProcessMatch, ProcessReport, ProcessSignal, ProcessSource, ProcessWatch,
        ProcessWatchRecord, ProcessWatchStatus,
    },
    watch_edit::WatchDefinition,
};
use crate::task_registry::{TaskFilter, TaskKind, TaskStatus};
use crate::tools::{
    bash_bg::BgEntry,
    term::{TermEntry, TermStreamEvent, TerminalScreen, find_pattern_columns},
};
use crate::watch::OutputCursor;

type Authorize = Box<dyn FnOnce() -> io::Result<()> + Send>;
type RegistrationResult = (ProcessWatchRecord, Option<CommitReceipt>);

/// A live resource-directory entry. Registration rechecks the selected incarnation; listing does not subscribe to output or claim execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessWatchSource {
    pub source: ProcessSource,
    pub label: String,
    pub status: TaskStatus,
}

struct Registration {
    watch: ProcessWatch,
    expected: Option<WatchDefinition>,
    authorize: Authorize,
    reply: oneshot::Sender<io::Result<RegistrationResult>>,
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
    resources: Weak<super::resources::InstanceResources>,
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
            expected,
            authorize,
            reply,
        } = registration;
        // Capture the initial replaceable state before acknowledging registration. The feed was subscribed before this snapshot.
        let snapshot = self.feed.snapshot();
        let mut matcher = Matcher::default();
        if expected.is_some() {
            if !snapshot.running() {
                let _ = reply.send(Err(invalid("replacement process is no longer running")));
                return Ok(());
            }
            // Editing starts at the captured live state, without replaying retained output.
            match &snapshot {
                Snapshot::Bash { bytes, base, .. } => {
                    matcher.cursor = Some(OutputCursor::at(base + bytes.len()));
                }
                Snapshot::Terminal { .. } => {
                    matcher.matches(&watch, &snapshot)?;
                }
            }
        }
        let requested = watch.clone();
        let position = self.position;
        let resources = self.resources.clone();
        let result = self
            .store(move |store| {
                authorize()?;
                if let Some(expected) = expected {
                    let receipt =
                        store.replace_process_watch(expected, requested.clone(), position)?;
                    if let Some(resources) = resources.upgrade() {
                        resources.observers.refresh();
                    }
                    let record = store
                        .process_watches()
                        .find(|record| record.watch.id == requested.id)
                        .cloned()
                        .ok_or_else(|| invalid("replacement registration was not committed"))?;
                    Ok((record, Some(receipt)))
                } else {
                    store
                        .register_process_watch(requested, position)
                        .map(|record| (record, None))
                }
            })
            .await;
        match result {
            Ok((record, receipt)) => {
                if receipt.is_some() {
                    self.refresh().await?;
                }
                if record.status == ProcessWatchStatus::Active {
                    self.watches
                        .entry(watch.id)
                        .or_insert_with(|| (watch, matcher));
                }
                let _ = reply.send(Ok((record, receipt)));
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
    /// Snapshot process resources owned by this loaded instance. Only running task records from open resource owners are included; selection is revalidated when registering a watch.
    pub fn process_watch_sources(&self) -> Vec<ProcessWatchSource> {
        if self.resources.lifetime.is_cancelled() {
            return Vec::new();
        }
        let mut sources: Vec<_> = self
            .resources
            .tasks()
            .list(&TaskFilter::running())
            .into_iter()
            .filter_map(|task| {
                let source = match task.kind {
                    TaskKind::Bash => ProcessSource::Bash {
                        incarnation: task.id.0,
                        handle: task.source_handle,
                    },
                    TaskKind::Terminal => ProcessSource::Terminal {
                        incarnation: task.id.0,
                        handle: task.source_handle,
                    },
                    TaskKind::Flow => return None,
                };
                Some(ProcessWatchSource {
                    source,
                    label: task.label,
                    status: task.status,
                })
            })
            .collect();
        sources.sort_by(|left, right| left.source.cmp(&right.source));
        sources
    }

    /// Register against the exact resource selected by a host. Identical retries return the recorded registration even after resource loss or cancellation; changed content and reused handles cannot redirect it to another process lifetime.
    pub async fn watch_process_source(
        &self,
        watch: ProcessWatch,
        authorize_owner: impl Fn(&crate::flow_store::FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<ProcessWatchRecord> {
        authorize_owner(&self.address)?;
        let authorize_owner = Arc::new(authorize_owner);
        let authorize = authorize_owner.clone();
        let requested = watch.clone();
        if let Some(record) = self
            .request(move |store| {
                authorize(store.state().address())?;
                match store
                    .process_watches()
                    .find(|record| record.watch.id == requested.id)
                {
                    Some(record) if record.watch == requested => Ok(Some(record.clone())),
                    Some(_) => Err(invalid(
                        "process watch identity was reused with different content",
                    )),
                    None => Ok(None),
                }
            })
            .await?
        {
            return Ok(record);
        }
        let (source, feed) = self.process_feed(watch.source.handle())?;
        if source != watch.source {
            return Err(invalid(
                "selected process incarnation is no longer available; refresh the resource list",
            ));
        }
        let owner = self.address.clone();
        self.register_process_feed(watch, feed, None, Box::new(move || authorize_owner(&owner)))
            .await
            .map(|(record, _)| record)
    }

    pub(super) async fn replace_process_source(
        &self,
        expected: WatchDefinition,
        watch: ProcessWatch,
        authorize_owner: impl Fn(&crate::flow_store::FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<CommitReceipt> {
        let (source, feed) = self.process_feed(watch.source.handle())?;
        if source != watch.source {
            return Err(invalid(
                "selected process incarnation is no longer available; refresh the resource list",
            ));
        }
        let owner = self.address.clone();
        self.register_process_feed(
            watch,
            feed,
            Some(expected),
            Box::new(move || authorize_owner(&owner)),
        )
        .await?
        .1
        .ok_or_else(|| invalid("process watcher replacement was not committed"))
    }

    /// Register observation of a live process owned by this instance without claiming a turn. Owner authorization runs before resource lookup and again at registration; registration captures the resource incarnation and initial source position before acknowledgement.
    #[allow(clippy::too_many_arguments)]
    pub async fn watch_process(
        &self,
        id: Uuid,
        handle: String,
        pattern: String,
        once: bool,
        expires_at: Option<DateTime<Utc>>,
        authorize_owner: impl Fn(&crate::flow_store::FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<ProcessWatchRecord> {
        authorize_owner(&self.address)?;
        let owner = self.address.clone();
        let authorize: Authorize = Box::new(move || authorize_owner(&owner));
        let (source, feed) = self.process_feed(&handle)?;
        self.register_process_feed(
            ProcessWatch {
                id,
                source,
                pattern,
                once,
                expires_at,
            },
            feed,
            None,
            authorize,
        )
        .await
        .map(|(record, _)| record)
    }

    fn process_feed(&self, handle: &str) -> io::Result<(ProcessSource, Feed)> {
        let session_id = self.address.session_id.to_string();
        let task = self
            .resources
            .tasks()
            .lookup_by_handle(handle)
            .ok_or_else(|| invalid("process handle is not owned by this instance"))?;
        let (source, feed) = match task.kind {
            TaskKind::Bash => {
                let entry = self
                    .resources
                    .background()
                    .lookup(handle, &session_id)
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
                        handle: handle.to_owned(),
                    },
                    Feed::Bash { entry, changes },
                )
            }
            TaskKind::Terminal => {
                let entry = self
                    .resources
                    .terminals()
                    .lookup(handle, &session_id)
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
                        handle: handle.to_owned(),
                    },
                    Feed::Terminal { entry, changes },
                )
            }
            TaskKind::Flow => return Err(invalid("flow events require a flow subscription")),
        };
        Ok((source, feed))
    }

    async fn register_process_feed(
        &self,
        watch: ProcessWatch,
        feed: Feed,
        expected: Option<WatchDefinition>,
        authorize: Authorize,
    ) -> io::Result<RegistrationResult> {
        let source = watch.source.clone();
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
                    resources: Arc::downgrade(&self.resources),
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
                watch,
                expected,
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
