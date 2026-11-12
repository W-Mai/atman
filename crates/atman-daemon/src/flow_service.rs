//! Daemon-owned persistent session catalogs. Reading or selecting a flow never installs a scheduler.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use atman_proto::{JsonRpcError, flows::*, methods::Method};
use atman_rt::FlowInstanceId;
use atman_runtime::flow_runtime::{
    FlowWorker,
    host::{HostServices, ProfileHandle, RuntimeHost, SessionHandle},
};
use atman_runtime::flow_store::{FlowRef, SessionDescriptor};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::DaemonState;

pub(crate) mod catalog;
pub(crate) mod events;
mod execution;
pub(crate) use execution::launch_run;
mod controls;
pub(crate) mod goal;
mod history;
mod history_wire;
mod management;
mod wire;

// Transport authentication identifies this operator; callers cannot supply sender provenance.
const OPERATOR: Uuid = Uuid::from_bytes(*b"atman-operator!!");

pub(crate) struct SessionService {
    data_dir: PathBuf,
    host: RuntimeHost,
    _permission_clients: Arc<atman_runtime::permission::PermissionClients>,
    executors: Mutex<HashMap<execution::ExecutorScope, Arc<OnceCell<atman_runtime::Executor>>>>,
    profile: OnceCell<Result<ProfileHandle, String>>,
}

impl SessionService {
    fn new(data_dir: PathBuf, redactor: Option<Arc<atman_runtime::redact::Redactor>>) -> Self {
        let permission_clients = Arc::new(atman_runtime::permission::PermissionClients::default());
        Self {
            data_dir,
            host: RuntimeHost::with_services(HostServices {
                redactor,
                session_redactor: None,
                permission_clients: Arc::downgrade(&permission_clients),
            }),
            _permission_clients: permission_clients,
            executors: Default::default(),
            profile: Default::default(),
        }
    }

    async fn attach(&self, id: Uuid, create: Option<SessionDescriptor>) -> Result<SessionHandle> {
        anyhow::ensure!(!id.is_nil(), "session identity must not be nil");
        // Unavailable management authority must not prevent unrelated work. Initialization reports its diagnostic once and management endpoints return the error.
        let _ = self.profile().await;
        Ok(match create {
            Some(descriptor) => self.host.create_session(&self.data_dir, descriptor).await?,
            None => self.host.open_session(&self.data_dir, id).await?,
        })
    }

    fn session(&self, id: Uuid) -> Result<SessionHandle> {
        self.host
            .session(id)
            .context("open the persistent session before accessing its flows")
    }
}

pub(crate) async fn dispatch(
    state: Arc<DaemonState>,
    method: Method,
    params: Value,
    principal: &str,
) -> Result<Value, JsonRpcError> {
    authorize_operator(principal)?;
    let request = Request::parse(method, params)?;
    if let Request::InputStatus(request) = request {
        return catalog::input_status(&state, request)
            .await
            .map_err(|error| JsonRpcError::application(error.to_string()));
    }
    if let Request::HistoryStatus(request) = request {
        return history::status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    // Accepted management work survives a disconnected RPC waiter.
    tokio::spawn(async move { dispatch_owned(&state, request).await })
        .await
        .map_err(|error| JsonRpcError::internal(error.to_string()))?
        .map_err(wire::application_error)
}

pub(crate) fn authorize_operator(principal: &str) -> Result<(), JsonRpcError> {
    // Both authenticated transports represent this one local operator. Unknown principals must not acquire storage ownership.
    if principal != crate::state::LOCAL_OPERATOR {
        return Err(JsonRpcError::application(
            "permission denied for persistent session management",
        ));
    }
    Ok(())
}

pub(crate) async fn context(
    state: &DaemonState,
    flow: FlowAddress,
    principal: &str,
) -> Result<Arc<atman_runtime::Session>, JsonRpcError> {
    Ok(worker(state, flow, principal).await?.context())
}

pub(crate) fn uses_persistent(
    state: &DaemonState,
    session: &atman_proto::SessionId,
    instance: Option<Uuid>,
) -> bool {
    instance.is_some()
        || state
            .session_service
            .get()
            .is_some_and(|service| service.host.session(session.0).is_ok())
}

/// Resolve a view once at admission. Subsequent default-view changes do not retarget accepted work.
pub(crate) async fn resolve_target(
    state: &DaemonState,
    session: &atman_proto::SessionId,
    instance: Option<Uuid>,
    principal: &str,
) -> Result<Option<FlowAddress>, JsonRpcError> {
    if !uses_persistent(state, session, instance) {
        return Ok(None);
    }
    authorize_operator(principal)?;
    if session.0.is_nil() || instance.is_some_and(|id| id.is_nil()) {
        return Err(JsonRpcError::invalid_params(
            "session and flow identities must not be nil",
        ));
    }
    let instance_id = match instance {
        Some(id) => id,
        None => {
            let service = state
                .session_service
                .get()
                .expect("attached session has a service");
            let snapshot = service
                .session(session.0)
                .map_err(|error| JsonRpcError::application(error.to_string()))?
                .snapshot()
                .await
                .map_err(|error| JsonRpcError::application(error.to_string()))?;
            snapshot
                .catalog
                .default_flow
                .ok_or_else(|| JsonRpcError::application("session has no default flow"))?
                .0
                .0
        }
    };
    Ok(Some(FlowAddress {
        session_id: session.clone(),
        instance_id,
    }))
}

pub(crate) async fn cancel_run(
    state: &DaemonState,
    run: &atman_proto::FlowRunId,
    principal: &str,
) -> Result<bool> {
    authorize_operator(principal).map_err(|error| anyhow::anyhow!(error.message))?;
    let Some(service) = state.session_service.get() else {
        return Ok(false);
    };
    // Observe only already-owned instances; cancellation must not recover unrelated journals.
    for id in service.host.sessions() {
        for view in service.session(id)?.observe().await?.loaded {
            if !matches!(view.execution.state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) if active.lease.run.0.0 == run.0)
            {
                continue;
            }
            let worker = service.host.flow(view.execution.address()).await?;
            if worker
                .cancel_run(run.clone().into_core(), |_| Ok(()))
                .await?
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) async fn worker(
    state: &DaemonState,
    flow: FlowAddress,
    principal: &str,
) -> Result<FlowWorker, JsonRpcError> {
    authorize_operator(principal)?;
    if flow.session_id.0.is_nil() || flow.instance_id.is_nil() {
        return Err(JsonRpcError::invalid_params(
            "session and flow identities must not be nil",
        ));
    }
    let service = state.session_service.get().ok_or_else(|| {
        JsonRpcError::application("open the persistent session before accessing its flows")
    })?;
    service
        .host
        .flow(&core_address(flow))
        .await
        .map_err(|error| JsonRpcError::application(error.to_string()))
}

enum Request {
    ManagementGet,
    ManagementOpen,
    ManagementSelect(SelectManagementRequest),
    Create(CreateSessionRequest),
    Open(SessionRequest),
    List(ListFlowsRequest),
    Select(FlowRequest),
    Get(FlowRequest),
    Control(ControlFlowRequest),
    ControlBatch(ControlFlowBatchRequest),
    ControlStatus(ControlFlowStatusRequest),
    Launch(LaunchFlowRequest),
    Bind(FlowRequest),
    Invoke(InvokeFlowRequest),
    InputStatus(InputStatusRequest),
    HistoryBranches(atman_proto::history::HistoryBranchesRequest),
    HistoryReview(atman_proto::history::HistoryReviewRequest),
    HistoryBegin(atman_proto::history::HistoryOperationRequest),
    HistoryAction(Method, atman_proto::history::HistoryOperationQuery),
    HistoryStatus(atman_proto::history::HistoryOperationQuery),
    Output(ReadFlowOutputRequest),
    Events(ReadFlowEventsRequest),
}

impl Request {
    fn parse(method: Method, params: Value) -> Result<Self, JsonRpcError> {
        let request = match method {
            Method::FlowControlBatch => {
                let request: ControlFlowBatchRequest = decode(params)?;
                controls::validate_batch(&request)?;
                return Ok(Self::ControlBatch(request));
            }
            Method::FlowControlStatus => {
                let request: ControlFlowStatusRequest = decode(params)?;
                controls::validate_identity(request.request_id)?;
                return Ok(Self::ControlStatus(request));
            }
            Method::ManagementGet | Method::ManagementOpen => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Empty {}
                let _: Empty = decode(params)?;
                return Ok(if method == Method::ManagementGet {
                    Self::ManagementGet
                } else {
                    Self::ManagementOpen
                });
            }
            Method::ManagementSelect => Self::ManagementSelect(decode(params)?),
            Method::SessionCreate => Self::Create(decode(params)?),
            Method::SessionOpen => Self::Open(decode(params)?),
            Method::SessionFlows => Self::List(decode(params)?),
            Method::SessionSelectFlow => Self::Select(decode(params)?),
            Method::FlowGet => Self::Get(decode(params)?),
            Method::FlowControl => Self::Control(decode(params)?),
            Method::FlowLaunch => Self::Launch(decode(params)?),
            Method::FlowBind => Self::Bind(decode(params)?),
            Method::FlowInvoke => Self::Invoke(decode(params)?),
            Method::FlowInputStatus => Self::InputStatus(decode(params)?),
            Method::FlowHistoryBranches => Self::HistoryBranches(decode(params)?),
            Method::FlowHistoryReview => Self::HistoryReview(decode(params)?),
            Method::FlowHistoryBegin => Self::HistoryBegin(decode(params)?),
            Method::FlowHistoryStop | Method::FlowHistoryApply | Method::FlowHistoryAbort => {
                Self::HistoryAction(method, decode(params)?)
            }
            Method::FlowHistoryStatus => Self::HistoryStatus(decode(params)?),
            Method::FlowOutput => Self::Output(decode(params)?),
            Method::FlowEvents => Self::Events(decode(params)?),
            _ => return Err(JsonRpcError::method_not_found(method.name())),
        };
        let (session, instance) = match &request {
            Self::ManagementGet
            | Self::ManagementOpen
            | Self::ControlBatch(_)
            | Self::ControlStatus(_) => unreachable!(),
            Self::ManagementSelect(request) => {
                if request.request_id.is_nil()
                    || request.expected.journal_id.is_nil()
                    || request.expected.sequence == 0
                {
                    return Err(JsonRpcError::invalid_params(
                        "management selection requires request identity and profile receipt",
                    ));
                }
                (request.session_id.0, None)
            }
            Self::Create(request) => {
                if request.title.trim().is_empty() {
                    return Err(JsonRpcError::invalid_params(
                        "session title must not be empty",
                    ));
                }
                (request.session_id.0, None)
            }
            Self::Open(request) => (request.session_id.0, None),
            Self::List(request) => {
                if !(1..=50).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "instance page limit must be between 1 and 50",
                    ));
                }
                (request.session_id.0, request.before)
            }
            Self::Select(request) | Self::Get(request) | Self::Bind(request) => {
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Control(request) => {
                controls::validate_control(request)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Launch(request) => {
                if request.request_id.is_nil()
                    || request.entry.trim().is_empty()
                    || !std::path::Path::new(&request.path).is_absolute()
                {
                    return Err(JsonRpcError::invalid_params(
                        "launch requires a request UUID, an absolute source path, and an entry name",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Invoke(request) => {
                if request.request_id.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "request identity must not be nil",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::InputStatus(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::HistoryBranches(request) => {
                if !(1..=100).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "branch page limit must be between 1 and 100",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::HistoryReview(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::HistoryBegin(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::HistoryAction(_, request) | Self::HistoryStatus(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Output(request) => {
                if !(1..=50).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "output page limit must be between 1 and 50",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Events(request) => {
                if !(1..=128).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "event page limit must be between 1 and 128",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
        };
        if session.is_nil() || instance.is_some_and(|id| id.is_nil()) {
            return Err(JsonRpcError::invalid_params(
                "session and flow identities must not be nil",
            ));
        }
        Ok(request)
    }
}

fn service(state: &DaemonState) -> &SessionService {
    let config_dir = state
        .launcher()
        .and_then(|launcher| launcher.config_dir.clone());
    service_with_config(state, config_dir.as_deref())
}

fn service_with_config<'a>(
    state: &'a DaemonState,
    config_dir: Option<&std::path::Path>,
) -> &'a SessionService {
    state.session_service.get_or_init(|| {
        SessionService::new(
            state.data_dir().to_path_buf(),
            crate::bootstrap::build_redactor(config_dir),
        )
    })
}

pub(crate) async fn shutdown(state: &DaemonState) -> Result<()> {
    // Initialize even an unused service so an already accepted RPC cannot install a fresh host after shutdown.
    let service = service(state);
    let result = service.host.shutdown().await;
    service.executors.lock().unwrap().clear();
    result.map_err(Into::into)
}

async fn dispatch_owned(state: &Arc<DaemonState>, request: Request) -> Result<Value> {
    anyhow::ensure!(!state.is_stopping(), "daemon is stopping");
    let service = service(state);
    match request {
        Request::ManagementGet => Ok(serde_json::to_value(
            service.profile().await?.snapshot().await?,
        )?),
        Request::ManagementOpen => service.open_management(state).await,
        Request::ManagementSelect(request) => service.select_management(request).await,
        Request::ControlBatch(request) => service.submit_controls(request).await,
        Request::ControlStatus(request) => service.control_status(request).await,
        Request::Launch(request) => service.launch(state, request).await,
        Request::Bind(request) => service.bind(state, request).await,
        Request::Invoke(request) => service.invoke(request).await,
        Request::InputStatus(_) => unreachable!("read-only status is dispatched without ownership"),
        Request::HistoryStatus(_) => {
            unreachable!("read-only history status is dispatched without ownership")
        }
        Request::HistoryBranches(request) => service.history_branches(request).await,
        Request::HistoryReview(request) => service.history_review(request).await,
        Request::HistoryBegin(request) => service.history_begin(request).await,
        Request::HistoryAction(method, request) => service.history_action(method, request).await,
        Request::Output(request) => service.output(request).await,
        Request::Events(request) => service.events(request).await,
        Request::Create(request) => {
            let config_dir = state
                .launcher()
                .and_then(|launcher| launcher.config_dir.clone());
            let hub = crate::bootstrap::resolve_config_hub(config_dir.as_deref())?;
            let project = request
                .project_root
                .map(|root| {
                    atman_runtime::storage::ProjectContext::resolve(
                        &hub,
                        std::path::Path::new(&root),
                        state.data_dir(),
                    )
                })
                .transpose()?;
            let descriptor = SessionDescriptor {
                id: request.session_id.0,
                title: request.title,
                configuration: atman_runtime::flow_store::SessionConfiguration::Atman(hub.source()),
                project,
            };
            describe_session(&service.attach(descriptor.id, Some(descriptor)).await?).await
        }
        Request::Open(request) => {
            describe_session(&service.attach(request.session_id.0, None).await?).await
        }
        Request::List(request) => {
            let page = atman_runtime::flow_runtime::catalog::SessionCatalog::new(
                Some(service.host.clone()),
                Some(service.data_dir.clone()),
            )
            .instances(
                request.session_id.0,
                request.before.map(|id| FlowInstanceId(id.into())),
                request.limit,
                |_| Ok(()),
            )
            .await?;
            let response = atman_proto::responses::FlowListPage {
                instances: page
                    .instances
                    .into_iter()
                    .map(|row| {
                        Ok::<_, anyhow::Error>(atman_proto::responses::FlowListEntry {
                            flow: address(&row.address),
                            entry: row.entry,
                            execution: row.state.as_ref().map(execution),
                            unavailable: row.unavailable,
                            loaded: row.loaded,
                            trailing_bytes: row.trailing_bytes,
                            archived: row.archived,
                            activity: row.activity.map(serde_json::to_value).transpose()?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                next: page.next.map(|id| id.0.0),
                total: page.total,
                catalog: events::cursor(&page.catalog),
            };
            Ok(serde_json::to_value(response)?)
        }
        Request::Select(request) => {
            let flow = core_address(request.flow);
            let session = service.session(flow.session_id.0)?;
            session
                .select_default(session.snapshot().await?.cursor, flow.instance_id)
                .await?;
            describe_session(&session).await
        }
        Request::Get(request) => {
            let flow = core_address(request.flow);
            let worker = service.host.flow(&flow).await?;
            let view = worker.status().await?;
            let scheduler = service
                .session(flow.session_id.0)?
                .scheduling_status(flow.instance_id.clone())
                .await?;
            let scheduler = scheduler.map(|status| scheduling(&status.borrow()));
            let close_error = worker.subscribe_close_error().borrow().clone();
            Ok(
                json!({"flow": address(&flow), "history_head": view.history_head.as_ref().map(wire::head), "execution": execution(&view.execution), "goal": view.goal, "last_completion": view.completion, "cursor": view.cursor, "scheduler": scheduler, "close_error": close_error}),
            )
        }
        Request::Control(request) => {
            let request_id = request.request_id;
            let request = controls::operator_control(request);
            let worker = service.host.flow(&request.target).await?;
            let receipt = worker.control(request, |_| Ok(())).await?;
            Ok(serde_json::to_value(
                atman_proto::responses::ControlFlowResponse {
                    request_id,
                    receipt: wire::cursor(&receipt.receipt)?,
                },
            )?)
        }
    }
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|error| JsonRpcError::invalid_params(error.to_string()))
}

async fn describe_session(session: &SessionHandle) -> Result<Value> {
    let snapshot = session.snapshot().await?;
    Ok(serde_json::to_value(atman_proto::responses::SessionView {
        session_id: atman_proto::SessionId(session.id()),
        title: snapshot.catalog.title,
        project_root: snapshot
            .catalog
            .project
            .as_ref()
            .map(|project| {
                project
                    .root()
                    .to_str()
                    .map(str::to_owned)
                    .context("project root is not valid UTF-8")
            })
            .transpose()?,
        default_flow: snapshot.catalog.default_flow.map(|flow| flow.0.0),
        flow_count: snapshot.catalog.flows.len(),
        catalog: events::cursor(&snapshot.cursor),
    })?)
}

fn core_address(address: FlowAddress) -> FlowRef {
    FlowRef {
        session_id: address.session_id.0.into(),
        instance_id: FlowInstanceId(address.instance_id.into()),
    }
}

fn address(flow: &FlowRef) -> FlowAddress {
    FlowAddress {
        session_id: atman_proto::SessionId(flow.session_id.0),
        instance_id: flow.instance_id.0.0,
    }
}

fn execution(state: &atman_rt::FlowInstance<atman_runtime::event::AtmanUuid>) -> Value {
    json!({"revision": state.revision(), "state": state.state(), "pause_count": state.pauses().len(), "pause_keys": state.pauses(), "can_start_turn": state.can_start_turn()})
}

fn scheduling(
    status: &atman_runtime::flow_runtime::scheduler::SchedulingStatus,
) -> atman_proto::responses::SchedulingView {
    use atman_runtime::flow_runtime::scheduler::SchedulingStatus;
    match status {
        SchedulingStatus::Running => atman_proto::responses::SchedulingView::Running,
        SchedulingStatus::Stopped => atman_proto::responses::SchedulingView::Stopped,
        SchedulingStatus::Failed(error) => atman_proto::responses::SchedulingView::Failed {
            error: error.clone(),
        },
    }
}
