//! Prepared settings owned by one source, with no filesystem work under publication locks.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use atman_runtime::{
    auth_store::AuthStore,
    config_hub::ConfigHub,
    model_registry::{ModelRegistry, ModelRegistrySnapshot, ProviderGroup},
};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
static SOURCES: OnceLock<Mutex<BTreeMap<Key, Weak<Store>>>> = OnceLock::new();

#[derive(Clone)]
pub(super) enum Origin {
    Global,
    Config(ConfigHub),
    Models(Arc<ModelRegistry>),
    Unavailable(String),
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
enum Key {
    Global,
    Config(PathBuf, PathBuf),
    Models(usize),
}

pub(super) struct Data {
    pub prepared: bool,
    pub config: Result<ConfigHub, String>,
    pub models: Arc<ModelRegistrySnapshot>,
    pub auth: Result<Arc<AuthStore>, String>,
    pub groups: Vec<ProviderGroup>,
    pub configured: Result<Arc<HashSet<(String, String)>>, String>,
    pub local_mcp: Vec<atman_runtime::mcp::McpServerConfig>,
}

impl Data {
    fn empty(reason: String) -> Arc<Self> {
        static EMPTY_MODELS: OnceLock<Arc<ModelRegistrySnapshot>> = OnceLock::new();
        Arc::new(Self {
            prepared: false,
            config: Err(reason.clone()),
            models: EMPTY_MODELS
                .get_or_init(|| Arc::new(ModelRegistry::new().snapshot()))
                .clone(),
            auth: Err(reason.clone()),
            configured: Err(reason),
            groups: Vec::new(),
            local_mcp: Vec::new(),
        })
    }
}

pub(super) struct Projection {
    pub generation: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub data: Arc<Data>,
}

pub(super) struct Store {
    origin: Origin,
    pub snapshot: watch::Sender<Arc<Projection>>,
    desired: Arc<AtomicU64>,
    wake: watch::Sender<()>,
    started: Arc<AtomicBool>,
    cancelled: CancellationToken,
}

impl Drop for Store {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}

impl Store {
    pub fn desired_generation(&self) -> u64 {
        self.desired.load(Ordering::Acquire)
    }
    pub fn bind(origin: Origin) -> Arc<Self> {
        let key = match &origin {
            Origin::Global => Some(Key::Global),
            Origin::Config(config) => {
                let source = config.source();
                Some(Key::Config(
                    source.config_dir().into(),
                    source.auth_path().into(),
                ))
            }
            Origin::Models(models) => Some(Key::Models(Arc::as_ptr(models) as usize)),
            Origin::Unavailable(_) => None,
        };
        let unavailable = match &origin {
            Origin::Unavailable(reason) => Some(reason.clone()),
            _ => None,
        };
        let create = || {
            Arc::new(Self {
                origin: origin.clone(),
                snapshot: watch::channel(Arc::new(Projection {
                    generation: 0,
                    loading: unavailable.is_none(),
                    error: None,
                    data: Data::empty(
                        unavailable
                            .clone()
                            .unwrap_or_else(|| "Settings are loading".into()),
                    ),
                }))
                .0,
                desired: Arc::new(AtomicU64::new(u64::from(unavailable.is_none()))),
                wake: watch::channel(()).0,
                started: Arc::new(AtomicBool::new(false)),
                cancelled: CancellationToken::new(),
            })
        };
        let Some(key) = key else {
            return create();
        };
        let mut sources = SOURCES.get_or_init(Mutex::default).lock().unwrap();
        sources.retain(|_, source| source.strong_count() > 0);
        if let Some(source) = sources.get(&key).and_then(Weak::upgrade) {
            return source;
        }
        let store = create();
        sources.insert(key, Arc::downgrade(&store));
        store
    }

    pub fn start(&self) {
        if matches!(self.origin, Origin::Unavailable(_)) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.snapshot.borrow().data.prepared {
            self.desired.fetch_add(1, Ordering::AcqRel);
        }
        let alive = Running(self.started.clone());
        let (origin, output, desired, wake, cancelled) = (
            self.origin.clone(),
            self.snapshot.clone(),
            self.desired.clone(),
            self.wake.subscribe(),
            self.cancelled.clone(),
        );
        runtime.spawn(async move {
            let _alive = alive;
            run(origin, output, desired, wake, cancelled).await;
        });
    }

    #[cfg(test)]
    pub fn is_waiting(&self) -> bool {
        self.start();
        let projection = self.snapshot.borrow();
        projection.loading || projection.generation < self.desired.load(Ordering::Acquire)
    }

    pub fn matches_registry(&self, registry: &Arc<ModelRegistry>) -> bool {
        match &self.origin {
            Origin::Global => {
                Arc::ptr_eq(registry, atman_runtime::model_registry::default_registry())
            }
            Origin::Config(config) => Arc::ptr_eq(registry, config.models()),
            Origin::Models(models) => Arc::ptr_eq(registry, models),
            Origin::Unavailable(_) => false,
        }
    }

    pub fn invalidate(&self) {
        if matches!(self.origin, Origin::Unavailable(_)) {
            return;
        }
        self.start();
        self.desired.fetch_add(1, Ordering::AcqRel);
        self.wake.send_replace(());
    }
}

struct Running(Arc<AtomicBool>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(super) async fn blocking<T: Send + 'static>(
    cancelled: &CancellationToken,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Option<Result<T, String>> {
    let limit = LIMIT.get_or_init(|| Arc::new(Semaphore::new(2))).clone();
    blocking_with_limit(cancelled, limit, work).await
}

async fn blocking_with_limit<T: Send + 'static>(
    cancelled: &CancellationToken,
    limit: Arc<Semaphore>,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Option<Result<T, String>> {
    let permit = tokio::select! {
        _ = cancelled.cancelled() => return None,
        permit = limit.acquire_owned() => match permit { Ok(permit) => permit, Err(error) => return Some(Err(error.to_string())) },
    };
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    });
    tokio::select! {
        _ = cancelled.cancelled() => None,
        result = work => Some(result.map_err(|error| error.to_string()).and_then(|result| result)),
    }
}

async fn run(
    origin: Origin,
    output: watch::Sender<Arc<Projection>>,
    desired: Arc<AtomicU64>,
    mut wake: watch::Receiver<()>,
    cancelled: CancellationToken,
) {
    let source = match origin {
        Origin::Global => loop {
            wake.borrow_and_update();
            match blocking(&cancelled, || {
                ConfigHub::global().map_err(|error| error.to_string())
            })
            .await
            {
                Some(Ok(config)) => break Origin::Config(config),
                Some(Err(error)) => {
                    let previous = output.borrow().data.clone();
                    output.send_replace(Arc::new(Projection {
                        generation: desired.load(Ordering::Acquire),
                        loading: false,
                        error: Some(error),
                        data: previous,
                    }));
                    tokio::select! { _ = cancelled.cancelled() => return, _ = wake.changed() => {} }
                }
                None => return,
            }
        },
        origin => origin,
    };
    let config = match &source {
        Origin::Config(config) => Some(config.clone()),
        _ => None,
    };
    let models = match source {
        Origin::Config(config) => config.models().clone(),
        Origin::Models(models) => models,
        _ => return,
    };
    let mut changes = config.as_ref().map(ConfigHub::subscribe_changes);
    let mut catalog = models.subscribe_model_catalog();
    loop {
        wake.borrow_and_update();
        catalog.borrow_and_update();
        if let Some(changes) = &mut changes {
            changes.borrow_and_update();
        }
        let generation = desired.load(Ordering::Acquire);
        let previous = output.borrow().data.clone();
        output.send_replace(Arc::new(Projection {
            generation,
            loading: true,
            error: None,
            data: previous.clone(),
        }));
        let (config, models) = (config.clone(), models.clone());
        let Some(result) = blocking(&cancelled, move || Ok(prepare(config, models))).await else {
            return;
        };
        if catalog.has_changed().unwrap_or(false)
            || changes
                .as_ref()
                .is_some_and(|changes| changes.has_changed().unwrap_or(false))
        {
            desired.fetch_add(1, Ordering::AcqRel);
            continue;
        }
        if generation != desired.load(Ordering::Acquire) {
            continue;
        }
        let (data, error) = match result {
            Ok(data) => (Arc::new(data), None),
            Err(error) => (previous, Some(error)),
        };
        output.send_replace(Arc::new(Projection {
            generation,
            loading: false,
            error,
            data,
        }));
        tokio::select! {
            _ = cancelled.cancelled() => return,
            _ = wake.changed() => {},
            _ = catalog.changed() => { desired.fetch_add(1, Ordering::AcqRel); },
            _ = async {
                match changes.as_mut() { Some(changes) => { let _ = changes.changed().await; }, None => std::future::pending().await }
            } => { desired.fetch_add(1, Ordering::AcqRel); },
        }
    }
}

fn prepare(config: Option<ConfigHub>, registry: Arc<ModelRegistry>) -> Data {
    let auth = config
        .as_ref()
        .ok_or_else(|| "Settings source is unavailable".to_owned())
        .and_then(|config| {
            config
                .load_auth()
                .map(Arc::new)
                .map_err(|error| error.to_string())
        });
    let mut groups = registry.all_provider_groups_for_management(
        auth.as_deref().unwrap_or(&AuthStore::default()),
        config.as_ref(),
    );
    groups.sort_by(|left, right| left.provider_name.cmp(&right.provider_name));
    let configured = config
        .as_ref()
        .ok_or_else(|| "Settings source is unavailable".to_owned())
        .and_then(|config| config.model_config().map_err(|error| error.to_string()))
        .map(|config| {
            Arc::new(
                config
                    .map(|config| {
                        config
                            .models
                            .into_iter()
                            .map(|(name, entry)| {
                                (entry.provider.unwrap_or_else(|| "unknown".into()), name)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        });
    let local_mcp = config
        .as_ref()
        .map(ConfigHub::load_local_mcp)
        .unwrap_or_default();
    Data {
        prepared: true,
        config: config.ok_or_else(|| "Settings source is unavailable".into()),
        models: Arc::new(registry.snapshot()),
        auth,
        groups,
        configured,
        local_mcp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ready(store: &Store) {
        let mut changed = store.snapshot.subscribe();
        store.start();
        while store.is_waiting() {
            changed.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn source_clones_share_preparation_without_sharing_other_sources_or_retaining_owners() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("one"));
        config.add_alias("smart", "first").unwrap();
        let first = Store::bind(Origin::Config(config.clone()));
        let second = Store::bind(Origin::Config(config.clone()));
        let foreign = Store::bind(Origin::Config(ConfigHub::from_config_dir(
            directory.path().join("two"),
        )));
        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &foreign));
        ready(&first).await;
        ready(&foreign).await;
        let retained = first.snapshot.borrow().data.clone();
        let foreign_generation = foreign.snapshot.borrow().generation;
        assert_eq!(retained.models.resolve_alias("smart"), "first");
        config
            .update_alias(Some("smart"), "smart", "second")
            .unwrap();
        first.invalidate();
        assert!(second.is_waiting());
        ready(&second).await;
        assert_eq!(
            second.snapshot.borrow().data.models.resolve_alias("smart"),
            "second"
        );
        assert_eq!(retained.models.resolve_alias("smart"), "first");
        assert_eq!(foreign.snapshot.borrow().generation, foreign_generation);
        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(
            weak.upgrade().is_none(),
            "background preparation cannot retain its owner"
        );
    }

    #[tokio::test]
    async fn cancelled_consumers_do_not_release_still_running_settings_workers() {
        let limit = Arc::new(Semaphore::new(1));
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        let worker_limit = limit.clone();
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = tokio::spawn(async move {
            blocking_with_limit(&cancelled, worker_limit, move || {
                started_tx.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            })
            .await
        });
        started.await.unwrap();
        cancellation.cancel();
        assert!(worker.await.unwrap().is_none());
        assert_eq!(limit.available_permits(), 0);
        release.send(()).unwrap();
        let _finished = limit.acquire().await.unwrap();
    }

    #[test]
    fn retained_sources_restart_preparation_after_their_runtime_exits() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("config"));
        config.add_alias("smart", "first").unwrap();
        let source = Store::bind(Origin::Config(config.clone()));
        {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(ready(&source));
        }
        assert!(!source.started.load(Ordering::Acquire));
        config
            .update_alias(Some("smart"), "smart", "second")
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(ready(&source));
        assert_eq!(
            source.snapshot.borrow().data.models.resolve_alias("smart"),
            "second"
        );
    }
}
