use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use atman_proto::{SessionId, flows::FlowAddress};
use atman_runtime::flow_store::RuntimeStore;
use axum::{
    Json,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, Method},
};
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DeviceError, RemoteState, authorized, device_request, notification_queue::Destination,
    notification_source, run,
};

const SOURCES_PER_POLL: usize = 16;
type Stamp = (u64, SystemTime);

#[derive(Clone)]
struct Source {
    flow: FlowAddress,
    journal: PathBuf,
}

#[derive(Default)]
struct Observation {
    catalogs: BTreeMap<Uuid, (Option<Stamp>, Vec<Source>)>,
    drained: BTreeMap<(Uuid, Uuid), Stamp>,
    failures: BTreeSet<(Uuid, Uuid)>,
    next: usize,
    pruned: Option<tokio::time::Instant>,
}

pub(super) async fn observe(state: Arc<RemoteState>, shutdown: CancellationToken) {
    if !state.notifications.enabled() {
        return;
    }
    let mut observations = Observation::default();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        let state = state.clone();
        let work = async {
            let sessions: BTreeSet<_> = state
                .devices
                .notification_recipients(&state.notifications, Utc::now())
                .into_iter()
                .map(|recipient| recipient.scope.session_id)
                .collect();
            let catalog = std::mem::take(&mut observations.catalogs);
            let now = tokio::time::Instant::now();
            let prune = observations
                .pruned
                .is_none_or(|then| now.duration_since(then) >= Duration::from_secs(60));
            let result = run(state.clone(), move |state| {
                if prune {
                    state
                        .notification_queue
                        .as_ref()
                        .ok_or(DeviceError::Missing)?
                        .prune(Utc::now())?;
                }
                Ok(refresh(catalog, sessions, state.daemon.data_dir()))
            })
            .await;
            let Ok((catalogs, sources)) = result else {
                return;
            };
            if prune {
                observations.pruned = Some(now);
            }
            observations.catalogs = catalogs;
            observations
                .drained
                .retain(|(session, _), _| observations.catalogs.contains_key(session));
            observations
                .failures
                .retain(|(session, _)| observations.catalogs.contains_key(session));
            if sources.is_empty() {
                observations.next = 0;
                return;
            }
            observations.next %= sources.len();
            for _ in 0..sources.len().min(SOURCES_PER_POLL) {
                let source = &sources[observations.next];
                observations.next = (observations.next + 1) % sources.len();
                let key = (source.flow.session_id.0, source.flow.instance_id);
                match observe_source(state.clone(), source, &mut observations.drained).await {
                    Err(error)
                        if !matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::NotFound
                        ) && observations.failures.insert(key) =>
                    {
                        eprintln!("[atman-daemon] notification observation will retry: {error}");
                    }
                    Ok(()) => {
                        observations.failures.remove(&key);
                    }
                    Err(_) => {}
                }
            }
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = work => {}
        }
    }
}

type Catalogs = BTreeMap<Uuid, (Option<Stamp>, Vec<Source>)>;

fn refresh(
    mut catalogs: Catalogs,
    sessions: BTreeSet<Uuid>,
    data: &Path,
) -> (Catalogs, Vec<Source>) {
    catalogs.retain(|session, _| sessions.contains(session));
    for session in sessions {
        let directory = RuntimeStore::session_directory(data, session);
        let journal = directory.join("events.jsonl");
        let before = stamp(&journal);
        if before.is_some()
            && catalogs
                .get(&session)
                .is_some_and(|(known, _)| *known == before)
        {
            continue;
        }
        let Ok(catalog) = RuntimeStore::inspect_session(data, session) else {
            catalogs.remove(&session);
            continue;
        };
        let sources = catalog
            .projection
            .flows
            .into_iter()
            .map(|registration| Source {
                journal: directory
                    .join("flows")
                    .join(registration.id.to_string())
                    .join("events.jsonl"),
                flow: FlowAddress {
                    session_id: SessionId(session),
                    instance_id: registration.id.0.0,
                },
            })
            .collect();
        let after = stamp(&journal);
        catalogs.insert(
            session,
            (before.filter(|before| Some(*before) == after), sources),
        );
    }
    let sources = catalogs
        .values()
        .flat_map(|(_, sources)| sources.iter().cloned())
        .collect();
    (catalogs, sources)
}

async fn observe_source(
    state: Arc<RemoteState>,
    source: &Source,
    drained: &mut BTreeMap<(Uuid, Uuid), Stamp>,
) -> io::Result<()> {
    let key = (source.flow.session_id.0, source.flow.instance_id);
    let path = source.journal.clone();
    let flow = source.flow.clone();
    let (before, checkpoint) = run(state.clone(), move |state| {
        let before = stamp(&path);
        let checkpoint = state
            .notification_queue
            .as_ref()
            .ok_or(DeviceError::Missing)?
            .checkpoint(&flow)?;
        Ok((before, checkpoint))
    })
    .await
    .map_err(device_io)?;
    if before.is_some_and(|value| drained.get(&key) == Some(&value)) {
        return Ok(());
    }
    let Some(observed) =
        notification_source::observe(&state.daemon, source.flow.clone(), checkpoint.clone())
            .await?
    else {
        return Ok(());
    };
    let flow = source.flow.clone();
    let path = source.journal.clone();
    let synchronized = run(state, move |state| {
        let queue = state
            .notification_queue
            .as_ref()
            .ok_or(DeviceError::Missing)?;
        let now = Utc::now();
        state
            .devices
            .with_notification_recipients(&state.notifications, now, |recipients| {
                if !recipients
                    .iter()
                    .any(|recipient| recipient.scope.session_id == flow.session_id.0)
                {
                    return Ok(None);
                }
                queue.record(&flow, checkpoint.as_ref(), &observed.page, &recipients, now)?;
                let still = observed.detached && !observed.page.has_more && before == stamp(&path);
                Ok(if still { before } else { None })
            })
    })
    .await
    .map_err(device_io)?;
    if let Some(stamp) = synchronized {
        drained.insert(key, stamp);
    } else {
        drained.remove(&key);
    }
    Ok(())
}

fn stamp(path: &Path) -> Option<Stamp> {
    let file = fs::metadata(path).ok()?;
    Some((file.len(), file.modified().ok()?))
}

fn device_io(error: DeviceError) -> io::Error {
    match error {
        DeviceError::Storage(error) => error,
        _ => io::Error::other(error),
    }
}

pub(super) async fn destination(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<Uuid>,
) -> Result<Json<Destination>, DeviceError> {
    if state.notification_queue.is_none() {
        return Err(DeviceError::Missing);
    }
    let grant = authorized(state.clone(), &headers, &Method::GET, true).await?;
    device_request(state, move |state| {
        let queue = state
            .notification_queue
            .as_ref()
            .ok_or(DeviceError::Missing)?;
        let authority = grant.authority();
        let now = Utc::now();
        state.devices.read_notification(authority, now, |scope| {
            queue
                .destination(
                    id,
                    authority.device_id,
                    scope.session_id,
                    scope.permission,
                    now,
                )?
                .map(Json)
                .ok_or(DeviceError::Missing)
        })
    })
    .await
}
