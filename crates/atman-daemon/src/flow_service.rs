//! Daemon-owned persistent session catalogs. Reading or selecting a flow never installs a scheduler.

use std::{
    collections::HashMap,
    path::Path,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use atman_proto::{
    JsonRpcError,
    composer::{
        ComposerPreferenceStatusRequest, FlowComposerRequest, SetComposerPreferenceRequest,
    },
    flows::*,
    methods::Method,
};
use atman_rt::FlowInstanceId;
use atman_runtime::flow_runtime::{
    FlowWorker,
    host::{HostServices, RuntimeHost, SessionHandle},
};
use atman_runtime::flow_store::{FlowRef, SessionDescriptor};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::DaemonState;

mod approvals;
pub(crate) mod catalog;
pub(crate) mod events;
mod execution;
pub(crate) use execution::launch_run;
mod composer;
mod controls;
mod conversation;
pub(crate) mod goal;
mod history;
mod history_wire;
mod human;
mod inbox;
mod live;
mod management;
mod overview;
mod preview;
mod questions;
mod wire;

// Transport authentication identifies this operator; callers cannot supply sender provenance.
pub(crate) const OPERATOR: Uuid = Uuid::from_bytes(*b"atman-operator!!");
const CONTROL_OPERATOR: Uuid = Uuid::from_bytes(*b"atman-local-user");

pub(crate) struct SessionService {
    data_dir: PathBuf,
    host: RuntimeHost,
    _permission_clients: Arc<atman_runtime::permission::PermissionClients>,
    executors: Mutex<HashMap<execution::ExecutorScope, Arc<OnceCell<atman_runtime::Executor>>>>,
    operator: Uuid,
    control_operator: Uuid,
    prompts: Option<crate::SharedPrompts>,
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
            operator: OPERATOR,
            control_operator: CONTROL_OPERATOR,
            prompts: None,
        }
    }

    pub(crate) fn shared(data_dir: PathBuf, services: crate::SharedHostServices) -> Self {
        Self {
            data_dir,
            host: services.host,
            _permission_clients: services.permissions,
            executors: Default::default(),
            operator: services.operator,
            control_operator: services.operator,
            prompts: services.prompts,
        }
    }

    async fn attach(&self, id: Uuid, create: Option<SessionDescriptor>) -> Result<SessionHandle> {
        anyhow::ensure!(!id.is_nil(), "session identity must not be nil");
        // Unavailable management authority must not prevent unrelated work. Initialization reports its diagnostic once and management endpoints return the error.
        let _ = self.profile().await;
        let session = match create {
            Some(descriptor) => self.host.create_session(&self.data_dir, descriptor).await?,
            None => self.host.open_session(&self.data_dir, id).await?,
        };
        // Opening history and recovering an accepted request remain possible with invalid settings.
        let _ = session.refresh_input_preferences().await;
        Ok(session)
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
    if let Request::ExecutionControl(request) = request {
        return catalog::execution_control(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::Overview(request) = request {
        return overview::snapshot(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::Composer(request) = request {
        return composer::snapshot(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::ComposerPreferenceStatus(request) = request {
        return composer::status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::Inbox(request) = request {
        return inbox::snapshot(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::InputStatus(request) = request {
        return catalog::input_status(&state, request)
            .await
            .map_err(|error| JsonRpcError::application(error.to_string()));
    }
    if let Request::HistoryRecoverInput(request) = request {
        return history::recover_input(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::HistoryStatus(request) = request {
        return history::status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionStatus(request) = request {
        return questions::status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionTranscript(request) = request {
        return questions::transcript(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionObserve(request) = request {
        return questions::observe(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionFollowupStatus(request) = request {
        return questions::followup_status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionExecution(request) = request {
        return questions::observe_execution(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionDiscardStatus(request) = request {
        return questions::discard_status(&state, request).map_err(wire::application_error);
    }
    if let Request::QuestionRefreshStatus(request) = request {
        return questions::refresh_status(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionKeepStatus(request) = request {
        return questions::keep_status(&state, request).map_err(wire::application_error);
    }
    if let Request::QuestionModelCheck(request) = request {
        return questions::check_model(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::QuestionModelOptions(request) = request {
        return questions::model_options(&state, request)
            .await
            .map_err(wire::application_error);
    }
    if let Request::Conversation(request) = request {
        // Store the read state only for conversation requests.
        return Box::pin(conversation::read(&state, request)).await;
    }
    if let Request::ConversationArtifact(request) = request {
        return Box::pin(conversation::artifact(&state, request)).await;
    }
    if let Request::ConversationImage(request) = request {
        return Box::pin(conversation::image(&state, request)).await;
    }
    if let Request::ApprovalQueue(request) = request {
        return approvals::snapshot(&state, request);
    }
    if let Request::Preview(request) = request {
        return preview::read(&state, request).await;
    }
    if let Request::Drafts(request) = request {
        return live::read(&state, request).await;
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
    Bootstrap(BootstrapSessionRequest),
    List(ListFlowsRequest),
    Select(FlowRequest),
    Get(FlowRequest),
    ExecutionControl(atman_proto::execution::FlowExecutionRequest),
    Composer(FlowComposerRequest),
    PrepareComposer(FlowRequest),
    SetComposerPreference(SetComposerPreferenceRequest),
    ComposerPreferenceStatus(ComposerPreferenceStatusRequest),
    Overview(FlowRequest),
    Control(ControlFlowRequest),
    ControlBatch(ControlFlowBatchRequest),
    ControlStatus(ControlFlowStatusRequest),
    Launch(LaunchFlowRequest),
    Bind(FlowRequest),
    Invoke(InvokeFlowRequest),
    Submit(SubmitFlowRequest),
    QuestionStart(atman_proto::questions::StartQuestionRequest),
    QuestionStatus(atman_proto::questions::QuestionStatusRequest),
    QuestionTranscript(atman_proto::questions::ReadQuestionTranscriptRequest),
    QuestionObserve(atman_proto::questions::ReadQuestionObservationRequest),
    QuestionFollowup(atman_proto::questions::QuestionFollowupRequest),
    QuestionFollowupStatus(atman_proto::questions::QuestionFollowupStatusRequest),
    QuestionExecution(atman_proto::questions::QuestionExecutionRequest),
    QuestionStop(atman_proto::questions::QuestionStopRequest),
    QuestionDiscard(atman_proto::questions::QuestionDiscardRequest),
    QuestionDiscardStatus(atman_proto::questions::QuestionDiscardStatusRequest),
    QuestionRefresh(atman_proto::questions::QuestionRefreshRequest),
    QuestionRefreshStatus(atman_proto::questions::QuestionRefreshStatusRequest),
    QuestionKeep(atman_proto::questions::QuestionKeepRequest),
    QuestionKeepStatus(atman_proto::questions::QuestionKeepStatusRequest),
    QuestionKeepInstalled(atman_proto::questions::QuestionKeepInstalledRequest),
    QuestionModelCheck(atman_proto::questions::QuestionModelCheckRequest),
    QuestionModelOptions(atman_proto::questions::QuestionModelOptionsRequest),
    InputStatus(InputStatusRequest),
    Inbox(atman_proto::inbox::ReadFlowInboxRequest),
    InboxEdit(atman_proto::inbox::EditFlowInboxRequest),
    Conversation(atman_proto::conversation::ReadConversationRequest),
    ConversationArtifact(atman_proto::conversation::ReadConversationArtifactRequest),
    ConversationImage(atman_proto::conversation::ReadConversationImageRequest),
    Preview(atman_proto::preview::ReadFlowPreviewRequest),
    Drafts(atman_proto::live::ReadFlowDraftsRequest),
    ApprovalQueue(atman_proto::approvals::ReadFlowApprovalQueueRequest),
    HistoryBranches(atman_proto::history::HistoryBranchesRequest),
    HistoryReview(atman_proto::history::HistoryReviewRequest),
    HistoryBegin(atman_proto::history::HistoryOperationRequest),
    HistoryAction(Method, atman_proto::history::HistoryOperationQuery),
    HistoryStatus(atman_proto::history::HistoryOperationQuery),
    HistoryRecoverInput(atman_proto::history::HistoryRecoverInputRequest),
    Output(ReadFlowOutputRequest),
    OutputDetail(ReadFlowOutputDetailRequest),
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
            Method::SessionBootstrap => Self::Bootstrap(decode(params)?),
            Method::SessionFlows => Self::List(decode(params)?),
            Method::SessionSelectFlow => Self::Select(decode(params)?),
            Method::FlowGet => Self::Get(decode(params)?),
            Method::FlowExecution => Self::ExecutionControl(decode(params)?),
            Method::FlowOverview => Self::Overview(decode(params)?),
            Method::FlowComposer => Self::Composer(decode(params)?),
            Method::FlowComposerPrepare => Self::PrepareComposer(decode(params)?),
            Method::FlowComposerPreferenceSet => Self::SetComposerPreference(decode(params)?),
            Method::FlowComposerPreferenceStatus => Self::ComposerPreferenceStatus(decode(params)?),
            Method::FlowControl => Self::Control(decode(params)?),
            Method::FlowLaunch => {
                let mut request: LaunchFlowRequest = decode(params)?;
                validate_working_directory(&mut request)?;
                Self::Launch(request)
            }
            Method::FlowBind => Self::Bind(decode(params)?),
            Method::FlowInvoke => Self::Invoke(decode(params)?),
            Method::FlowSubmit => Self::Submit(decode(params)?),
            Method::QuestionStart => Self::QuestionStart(decode(params)?),
            Method::QuestionStatus => Self::QuestionStatus(decode(params)?),
            Method::QuestionTranscript => Self::QuestionTranscript(decode(params)?),
            Method::QuestionObserve => Self::QuestionObserve(decode(params)?),
            Method::QuestionFollowup => Self::QuestionFollowup(decode(params)?),
            Method::QuestionFollowupStatus => Self::QuestionFollowupStatus(decode(params)?),
            Method::QuestionExecution => Self::QuestionExecution(decode(params)?),
            Method::QuestionStop => Self::QuestionStop(decode(params)?),
            Method::QuestionDiscard => Self::QuestionDiscard(decode(params)?),
            Method::QuestionDiscardStatus => Self::QuestionDiscardStatus(decode(params)?),
            Method::QuestionRefresh => Self::QuestionRefresh(decode(params)?),
            Method::QuestionRefreshStatus => Self::QuestionRefreshStatus(decode(params)?),
            Method::QuestionKeep => Self::QuestionKeep(decode(params)?),
            Method::QuestionKeepStatus => Self::QuestionKeepStatus(decode(params)?),
            Method::QuestionKeepInstalled => Self::QuestionKeepInstalled(decode(params)?),
            Method::QuestionModelCheck => Self::QuestionModelCheck(decode(params)?),
            Method::QuestionModelOptions => Self::QuestionModelOptions(decode(params)?),
            Method::FlowInbox => Self::Inbox(decode(params)?),
            Method::FlowInboxEdit => Self::InboxEdit(decode(params)?),
            Method::FlowInputStatus => Self::InputStatus(decode(params)?),
            Method::FlowConversation => Self::Conversation(decode(params)?),
            Method::FlowConversationArtifact => Self::ConversationArtifact(decode(params)?),
            Method::FlowConversationImage => Self::ConversationImage(decode(params)?),
            Method::FlowPreview => Self::Preview(decode(params)?),
            Method::FlowDrafts => Self::Drafts(decode(params)?),
            Method::FlowApprovalQueue => Self::ApprovalQueue(decode(params)?),
            Method::FlowHistoryBranches => Self::HistoryBranches(decode(params)?),
            Method::FlowHistoryReview => Self::HistoryReview(decode(params)?),
            Method::FlowHistoryBegin => Self::HistoryBegin(decode(params)?),
            Method::FlowHistoryStop | Method::FlowHistoryApply | Method::FlowHistoryAbort => {
                Self::HistoryAction(method, decode(params)?)
            }
            Method::FlowHistoryStatus => Self::HistoryStatus(decode(params)?),
            Method::FlowHistoryRecoverInput => Self::HistoryRecoverInput(decode(params)?),
            Method::FlowOutput => Self::Output(decode(params)?),
            Method::FlowOutputDetail => Self::OutputDetail(decode(params)?),
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
            Self::Bootstrap(request) => {
                if request.expected_catalog.journal_id.is_nil()
                    || request.expected_catalog.sequence == 0
                {
                    return Err(JsonRpcError::invalid_params(
                        "bootstrap requires an observed catalog receipt",
                    ));
                }
                (request.session_id.0, None)
            }
            Self::List(request) => {
                if !(1..=50).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "instance page limit must be between 1 and 50",
                    ));
                }
                (request.session_id.0, request.before)
            }
            Self::Select(request)
            | Self::Get(request)
            | Self::Bind(request)
            | Self::PrepareComposer(request)
            | Self::Overview(request) => {
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::ExecutionControl(request) => {
                if let Some(request_id) = request.request_id {
                    controls::validate_identity(request_id)?;
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Composer(request) => {
                composer::validate(request)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::SetComposerPreference(request) => {
                controls::validate_identity(request.request_id)?;
                composer::validate_edit(request)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::ComposerPreferenceStatus(request) => {
                controls::validate_identity(request.request_id)?;
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
            Self::Submit(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::QuestionStart(request) => {
                questions::validate_start(request)?;
                let flow = &request.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionStatus(request) => {
                questions::validate_identity(&request.identity, request.owner_generation)?;
                let flow = &request.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionTranscript(request) => {
                questions::validate_transcript(request)?;
                let flow = &request.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionObserve(request) => {
                questions::validate_observation(request)?;
                let flow = &request.page.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionFollowup(request) => {
                questions::validate_followup(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionFollowupStatus(request) => {
                questions::validate_target(&request.question, Some(request.followup_request_id))?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionExecution(request) => {
                questions::validate_target(&request.question, request.stop_request_id)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionStop(request) => {
                questions::validate_stop(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionDiscard(request) => {
                questions::validate_discard(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionDiscardStatus(request) => {
                questions::validate_target(&request.question, Some(request.discard_request_id))?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionRefresh(request) => {
                questions::validate_refresh(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionRefreshStatus(request) => {
                questions::validate_target(&request.question, Some(request.refresh_request_id))?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionKeep(request) => {
                questions::validate_keep(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionKeepStatus(request) => {
                questions::validate_target(&request.question, Some(request.keep_request_id))?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionKeepInstalled(request) => {
                questions::validate_keep_installed(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionModelCheck(request) => {
                questions::validate_model_check(request)?;
                let flow = &request.question.identity.source;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::QuestionModelOptions(request) => {
                questions::validate_model_options(request)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Inbox(request) => (request.flow.session_id.0, Some(request.flow.instance_id)),
            Self::InboxEdit(request) => {
                inbox::validate(request)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::InputStatus(request) => {
                controls::validate_identity(request.request_id)?;
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Conversation(request) => {
                if !(1..=32).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "conversation page limit must be between 1 and 32",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::ConversationArtifact(request) => {
                conversation::validate_artifact(request)?;
                let flow = &request.locator.source.flow;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::ConversationImage(request) => {
                conversation::validate_image(request)?;
                let flow = &request.locator.source.flow;
                (flow.session_id.0, Some(flow.instance_id))
            }
            Self::ApprovalQueue(request) => {
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Preview(request) => {
                if request.known.as_ref().is_some_and(|edition| {
                    edition.host_generation.is_nil() || edition.worker_incarnation.is_nil()
                }) {
                    return Err(JsonRpcError::invalid_params(
                        "preview edition requires an owner identity",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Drafts(request) => {
                if request.head.branch.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "history branch must not be nil",
                    ));
                }
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
            Self::HistoryRecoverInput(request) => {
                controls::validate_identity(request.request_id)?;
                controls::validate_identity(request.input)?;
                controls::validate_identity(request.applied_head.branch)?;
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
            Self::OutputDetail(request) => {
                if request.publication_id.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "publication identity must not be nil",
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

pub(crate) fn runtime_host(state: &DaemonState) -> RuntimeHost {
    service(state).host.clone()
}

pub(crate) async fn install_handoff(
    state: &Arc<DaemonState>,
    receipt: &atman_runtime::flow_runtime::host::SessionHandoffReceipt,
) -> std::io::Result<()> {
    let service = service(state);
    let session = service
        .host
        .acquire_handoff(state.data_dir(), receipt)
        .await?;
    for flow in &receipt.flows {
        if matches!(
            flow.scheduling,
            Some(atman_runtime::flow_runtime::host::HandoffScheduling::Running)
        ) && session
            .scheduling_status(flow.instance_id.clone())
            .await?
            .is_none()
        {
            service
                .bind(
                    state,
                    FlowRequest {
                        flow: address(&FlowRef {
                            session_id: receipt.session_id.into(),
                            instance_id: flow.instance_id.clone(),
                        }),
                    },
                )
                .await
                .map_err(std::io::Error::other)?;
        }
    }
    Ok(())
}

fn service(state: &DaemonState) -> &SessionService {
    let config_dir = state
        .launcher()
        .and_then(|launcher| launcher.config_dir.clone());
    service_with_config(state, config_dir.as_deref())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) async fn bootstrap_workspace(
    state: &Arc<DaemonState>,
    catalog: atman_runtime::event_log::journal::CommitReceipt,
    proof: atman_runtime::flow_store::profile::workspace_open::WorkspaceBootstrap,
    reader: atman_runtime::project_catalog::workspace::WorkspaceReader,
) -> Result<atman_proto::responses::LaunchFlowResponse> {
    let service = service(state);
    let id = catalog.journal_id;
    proof.verify(reader.clone()).await?;
    let session = service.host.open_session(state.data_dir(), id).await?;
    let snapshot = session.snapshot().await?;
    anyhow::ensure!(
        proof.matches(&snapshot.catalog),
        "Workspace Session binding changed"
    );
    let result = service
        .bootstrap_scoped(
            state,
            BootstrapSessionRequest {
                session_id: atman_proto::SessionId(id),
                expected_catalog: events::cursor(&catalog),
            },
            human::BootstrapScope::Workspace {
                proof: Box::new(proof),
                reader,
            },
        )
        .await?;
    Ok(serde_json::from_value(result)?)
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
        Request::Submit(request) => service.submit(state, request).await,
        Request::QuestionStart(request) => service.start_question(state, request).await,
        Request::QuestionStatus(_) => unreachable!("question start status is read-only"),
        Request::QuestionTranscript(_) => unreachable!("question transcript is read-only"),
        Request::QuestionObserve(_) => unreachable!("question observation is read-only"),
        Request::QuestionFollowup(request) => service.question_followup(request).await,
        Request::QuestionFollowupStatus(_) => {
            unreachable!("question follow-up status is read-only")
        }
        Request::QuestionExecution(_) => unreachable!("question execution is read-only"),
        Request::QuestionStop(request) => service.stop_question(request).await,
        Request::QuestionDiscard(request) => service.discard_question(request).await,
        Request::QuestionDiscardStatus(_) => unreachable!("question discard status is read-only"),
        Request::QuestionRefresh(request) => service.refresh_question(state, request).await,
        Request::QuestionRefreshStatus(_) => unreachable!("question refresh status is read-only"),
        Request::QuestionKeep(request) => service.keep_question(request).await,
        Request::QuestionKeepStatus(_) => unreachable!("question keep status is read-only"),
        Request::QuestionKeepInstalled(request) => service.install_kept_question(request).await,
        Request::QuestionModelCheck(_) => unreachable!("question model check is read-only"),
        Request::QuestionModelOptions(_) => unreachable!("question model options are read-only"),
        Request::Bootstrap(request) => service.bootstrap(state, request).await,
        Request::Composer(_) => {
            unreachable!("read-only composer settings are dispatched without ownership")
        }
        Request::SetComposerPreference(request) => composer::set(state, request).await,
        Request::PrepareComposer(request) => composer::prepare(state, service, request).await,
        Request::ComposerPreferenceStatus(_) => {
            unreachable!("read-only preference status is dispatched without ownership")
        }
        Request::Overview(_) => unreachable!("overview is dispatched without ownership"),
        Request::ExecutionControl(_) => {
            unreachable!("execution status is dispatched without ownership")
        }
        Request::Inbox(_) => unreachable!("read-only inbox is dispatched without ownership"),
        Request::InboxEdit(request) => service.edit_inbox(request).await,
        Request::InputStatus(_) => unreachable!("read-only status is dispatched without ownership"),
        Request::Conversation(_)
        | Request::ConversationArtifact(_)
        | Request::ConversationImage(_) => {
            unreachable!("conversation is dispatched without ownership")
        }
        Request::Preview(_) => unreachable!("preview is dispatched without ownership"),
        Request::Drafts(_) => unreachable!("drafts are dispatched without ownership"),
        Request::ApprovalQueue(_) => {
            unreachable!("approval observation is dispatched without ownership")
        }
        Request::HistoryRecoverInput(_) => unreachable!("history recovery is read-only"),
        Request::HistoryStatus(_) => {
            unreachable!("read-only history status is dispatched without ownership")
        }
        Request::HistoryBranches(request) => service.history_branches(request).await,
        Request::HistoryReview(request) => service.history_review(request).await,
        Request::HistoryBegin(request) => service.history_begin(request).await,
        Request::HistoryAction(method, request) => service.history_action(method, request).await,
        Request::Output(request) => service.output(request).await,
        Request::OutputDetail(request) => service.output_detail(request).await,
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
                            map: row.state.as_ref().map(|state| {
                                atman_proto::responses::FlowMapNode {
                                    parent: row.parent.as_ref().map(address),
                                    phase: atman_proto::execution::FlowExecutionPhase::observe(
                                        state.state(),
                                        row.loaded,
                                    ),
                                    paused: !state.pauses().is_empty(),
                                }
                            }),
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
            let request = controls::operator_control(request, service.control_operator);
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

fn validate_working_directory(request: &mut LaunchFlowRequest) -> Result<(), JsonRpcError> {
    let Some(value) = request.working_directory.as_mut() else {
        return Ok(());
    };
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(JsonRpcError::invalid_params(
            "working_directory must be an absolute existing directory",
        ));
    }
    let canonical = path.canonicalize().map_err(|error| {
        JsonRpcError::invalid_params(format!("working_directory is unavailable: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(JsonRpcError::invalid_params(
            "working_directory must be an existing directory",
        ));
    }
    *value = canonical
        .to_str()
        .ok_or_else(|| JsonRpcError::invalid_params("working_directory must be valid UTF-8"))?
        .to_owned();
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn launch_request(working_directory: Option<String>) -> LaunchFlowRequest {
        LaunchFlowRequest {
            flow: FlowAddress {
                session_id: atman_proto::SessionId(Uuid::now_v7()),
                instance_id: Uuid::now_v7(),
            },
            request_id: Uuid::now_v7(),
            expected_catalog_sequence: 0,
            path: "/tmp/entry.atman".into(),
            entry: "main".into(),
            working_directory,
            start: FlowStart::Waiting,
            initial_goal: None,
            arguments: BTreeMap::new(),
            invocation: BTreeMap::new(),
            user: None,
        }
    }

    #[test]
    fn launch_working_directory_is_canonicalized_and_must_be_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let alias = nested.join("..").join("nested");
        let mut request = launch_request(Some(alias.to_string_lossy().into_owned()));
        validate_working_directory(&mut request).unwrap();
        assert_eq!(
            request.working_directory.as_deref(),
            Some(nested.canonicalize().unwrap().to_str().unwrap())
        );

        let mut request = launch_request(Some(
            root.path().join("missing").to_string_lossy().into_owned(),
        ));
        assert!(validate_working_directory(&mut request).is_err());

        let file = root.path().join("file");
        std::fs::write(&file, b"entry").unwrap();
        let mut request = launch_request(Some(file.to_string_lossy().into_owned()));
        assert!(validate_working_directory(&mut request).is_err());
    }
}
