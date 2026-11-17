//! Own accepted workspace services after their presentation retires.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(super) struct Services {
    tasks: Mutex<Vec<Retained>>,
}

struct Retained {
    stop: watch::Sender<bool>,
    pending: Arc<AtomicBool>,
    task: JoinHandle<Result<()>>,
}

pub(super) struct Owner {
    stop: watch::Receiver<bool>,
    pending: Arc<AtomicBool>,
}

impl Services {
    pub fn prepare(&self) -> (Owner, impl FnOnce(JoinHandle<Result<()>>) + '_) {
        let (stop, receiver) = watch::channel(false);
        let pending = Arc::new(AtomicBool::new(true));
        let owner = Owner {
            stop: receiver,
            pending: pending.clone(),
        };
        (owner, move |task| {
            self.tasks.lock().unwrap().push(Retained {
                stop,
                pending,
                task,
            })
        })
    }

    pub fn has_pending(&self) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .any(|task| task.pending.load(Ordering::Acquire))
    }

    /// Only the host's verified settlement path may retire these owners.
    pub async fn finish(&self) -> Result<()> {
        while self.has_pending() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in &tasks {
            task.stop.send_replace(true);
        }
        let mut first = None;
        for task in tasks {
            if let Err(error) = task
                .task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

impl Owner {
    pub fn pending(&self, value: bool) {
        self.pending.store(value, Ordering::Release);
    }

    pub async fn stopped(&mut self) {
        while !*self.stop.borrow_and_update() {
            if self.stop.changed().await.is_err() {
                break;
            }
        }
    }
}

pub(super) async fn drive(
    scopes: &super::scopes::Scopes,
    diagnostics: crate::host_process::diagnostics::Diagnostics,
) -> Result<()> {
    let mut diagnostics = Some(diagnostics);
    let mut maintained = std::time::Instant::now();
    let mut returned: Option<JoinHandle<std::io::Result<()>>> = None;
    let mut attempted = None::<std::time::Instant>;
    let mut host_operations = scopes.host.subscribe_host_operations();
    loop {
        host_operations.borrow_and_update();
        let rpc = scopes.rpc.get();
        if returned.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(task) = returned.take() {
                if let Err(error) = task.await?
                    && error.kind() != std::io::ErrorKind::NotConnected
                {
                    atman_runtime::notify!(
                        warn,
                        location = Log,
                        "Session handoff remains pending: {error}"
                    );
                }
            }
        }
        if returned.is_none()
            && (!scopes.host.sessions().is_empty()
                || rpc.is_some_and(|rpc| rpc.state.handoff_pending()))
            && attempted.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(2))
            && let Some(rpc) = rpc
        {
            let state = rpc.state.clone();
            returned = Some(tokio::spawn(async move {
                state
                    .return_sessions_to_daemon(tokio_util::sync::CancellationToken::new())
                    .await
            }));
            attempted = Some(std::time::Instant::now());
        }
        if returned.is_none()
            && !rpc.is_some_and(|rpc| rpc.state.handoff_pending())
            && !scopes.rpc_initializing()
            && !scopes.background.has_pending()
            && scopes.host.try_shutdown_idle().await?
        {
            return Ok(());
        }
        if maintained.elapsed() >= std::time::Duration::from_secs(30) {
            let log = diagnostics
                .take()
                .expect("diagnostics retained by background host");
            diagnostics = Some(
                tokio::task::spawn_blocking(move || {
                    if let Err(error) = log.maintain() {
                        atman_runtime::notify!(
                            warn,
                            location = Log,
                            "Host diagnostics maintenance failed: {error}"
                        );
                    }
                    log
                })
                .await?,
            );
            maintained = std::time::Instant::now();
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            _ = host_operations.changed() => {}
        }
    }
}
