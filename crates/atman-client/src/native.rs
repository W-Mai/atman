//! Trusted in-process access to the original host, without a JSON-RPC dispatch round trip.

use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};

use atman_proto::{
    capabilities::{CapabilitiesRequest, CapabilitiesResponse, PROTOCOL_REVISION, SCHEMA_REVISION},
    client::CapabilitySet,
    flows::{
        ControlFlowRequest, FlowAddress, FlowControl, FlowCursor, FlowRequest, InputStatusRequest,
        InvokeFlowRequest, ListFlowsRequest, ReadFlowEventsRequest, SessionRequest,
    },
    methods::Method,
    responses::{
        ControlFlowResponse, FlowAcceptance, FlowEventBatch, FlowEventPage, FlowListEntry,
        FlowListPage, InputConsumption, InputDisposition, InputStatusResponse, InputTurn,
        InvokeFlowResponse, SchedulingView, SessionView,
    },
};
use atman_runtime::{
    event_log::journal::CommitReceipt,
    flow_runtime::{
        FlowViewReader, FlowWorker,
        catalog::SessionCatalog,
        host::{RuntimeHost, SessionHandle},
        live::LiveOutputReader,
        scheduler::SchedulingStatus,
    },
    flow_store::{
        FlowRef,
        control::{ControlAction, ControlRequest},
        inbox::{DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, SenderIdentity},
        input_observation::{InputDisposition as CoreInputDisposition, InputObservation},
        local_operator::LocalOperator,
        position::{FlowPage, FlowPosition},
        view::FlowUpdate,
    },
};
use futures::{StreamExt, stream::BoxStream};
use uuid::Uuid;

use crate::{
    ClientError, EventSubscription, ExecutionView, FlowObservation, Result, flow_identity, limit,
};

#[derive(Clone, Copy)]
enum Operator {
    Native,
    LegacyRpc,
}

impl Operator {
    fn binding(self) -> LocalOperator {
        match self {
            Self::Native => LocalOperator::NATIVE,
            Self::LegacyRpc => LocalOperator::LEGACY_RPC,
        }
    }

    fn input(self) -> SenderIdentity {
        self.binding().sender()
    }
}

/// A borrowed host capability. Dropping the client never shuts down the runtime.
#[derive(Clone)]
pub struct NativeClient {
    host: RuntimeHost,
    data: Arc<PathBuf>,
    generation: Arc<str>,
    operator: Operator,
}

impl NativeClient {
    /// Bind the original terminal operator. Host ownership is supplied by the trusted caller.
    pub fn new(host: RuntimeHost, data: PathBuf, generation: impl Into<Arc<str>>) -> Self {
        Self {
            host,
            data: Arc::new(data),
            generation: generation.into(),
            operator: Operator::Native,
        }
    }

    /// Retain the existing standalone RPC request namespace on a trusted host.
    ///
    /// Use this binding when that namespace owns an existing request. An absent status lookup must never cause a retry to be submitted under a different binding.
    pub fn legacy_rpc(host: RuntimeHost, data: PathBuf, generation: impl Into<Arc<str>>) -> Self {
        Self {
            host,
            data: Arc::new(data),
            generation: generation.into(),
            operator: Operator::LegacyRpc,
        }
    }

    pub fn host(&self) -> &RuntimeHost {
        &self.host
    }

    pub fn session(&self, session: Uuid) -> Result<SessionHandle> {
        Ok(self.host.session(session)?)
    }

    pub async fn flow(&self, flow: &FlowAddress) -> Result<FlowWorker> {
        flow_identity(flow)?;
        Ok(self.host.flow(&core_address(flow)).await?)
    }

    pub async fn view(
        &self,
        flow: &FlowAddress,
        page_size: NonZeroUsize,
    ) -> Result<FlowViewReader> {
        Ok(self.flow(flow).await?.subscribe_view(page_size).await?)
    }

    pub async fn live(&self, flow: &FlowAddress) -> Result<LiveOutputReader> {
        Ok(self.flow(flow).await?.subscribe_live_output())
    }

    /// Native committed observation retains typed runtime facts and original positions.
    pub async fn native_events(
        &self,
        flow: &FlowAddress,
        after: Option<FlowPosition>,
        page_size: NonZeroUsize,
    ) -> Result<BoxStream<'static, std::io::Result<FlowPage<FlowUpdate>>>> {
        Ok(self
            .flow(flow)
            .await?
            .subscribe_updates(after, page_size)
            .await?)
    }

    pub async fn capabilities(&self, request: CapabilitiesRequest) -> Result<CapabilitiesResponse> {
        if request.methods.len() > 128 || request.methods.keys().any(|method| method.len() > 128) {
            return Err(ClientError::InvalidRequest(
                "capability requirements are too large",
            ));
        }
        let response = CapabilitiesResponse {
            protocol_revision: PROTOCOL_REVISION,
            schema_revision: SCHEMA_REVISION,
            host_generation: self.generation.to_string(),
            methods: [
                Method::Capabilities,
                Method::SessionOpen,
                Method::SessionFlows,
                Method::FlowGet,
                Method::FlowInvoke,
                Method::FlowInputStatus,
                Method::FlowControl,
                Method::FlowEvents,
            ]
            .into_iter()
            .map(|method| (method.name().to_owned(), method.capability()))
            .collect(),
        };
        CapabilitySet::new(response.clone()).require(&request)?;
        Ok(response)
    }

    pub async fn open_session(&self, request: SessionRequest) -> Result<SessionView> {
        if request.session_id.0.is_nil() {
            return Err(ClientError::InvalidRequest(
                "session identity must not be nil",
            ));
        }
        let session = self
            .host
            .open_session(&self.data, request.session_id.0)
            .await?;
        let snapshot = session.snapshot().await?;
        Ok(SessionView {
            session_id: request.session_id,
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
                        .ok_or(ClientError::InvalidRequest("project root is not UTF-8"))
                })
                .transpose()?,
            default_flow: snapshot.catalog.default_flow.map(|flow| flow.0.0),
            flow_count: snapshot.catalog.flows.len(),
            catalog: cursor(&snapshot.cursor),
        })
    }

    pub async fn list_flows(&self, request: ListFlowsRequest) -> Result<FlowListPage> {
        limit(request.limit, 50)?;
        if request.session_id.0.is_nil() {
            return Err(ClientError::InvalidRequest(
                "session identity must not be nil",
            ));
        }
        let page = self
            .catalog()
            .instances(
                request.session_id.0,
                request.before.map(|id| atman_rt::FlowInstanceId(id.into())),
                request.limit,
                |_| Ok(()),
            )
            .await?;
        Ok(FlowListPage {
            instances: page
                .instances
                .into_iter()
                .map(|row| {
                    Ok(FlowListEntry {
                        flow: address(&row.address),
                        entry: row.entry,
                        execution: row
                            .state
                            .as_ref()
                            .map(|state| serde_json::to_value(execution(state)))
                            .transpose()?,
                        unavailable: row.unavailable,
                        loaded: row.loaded,
                        trailing_bytes: row.trailing_bytes,
                        archived: row.archived,
                        activity: row.activity.map(serde_json::to_value).transpose()?,
                    })
                })
                .collect::<Result<_>>()?,
            next: page.next.map(|flow| flow.0.0),
            total: page.total,
            catalog: cursor(&page.catalog),
        })
    }

    pub async fn get_flow(&self, request: FlowRequest) -> Result<FlowObservation> {
        let worker = self.flow(&request.flow).await?;
        let view = worker.status().await?;
        let scheduler = self
            .session(request.flow.session_id.0)?
            .scheduling_status(core_address(&request.flow).instance_id)
            .await?;
        Ok(FlowObservation {
            flow: request.flow,
            history_head: view
                .history_head
                .map(|head| atman_proto::history::HistoryHead {
                    branch: *head.branch.as_uuid(),
                    version: head.version.get(),
                }),
            execution: execution(&view.execution),
            goal: view.goal,
            last_completion: view.completion,
            cursor: position(&view.cursor)?,
            scheduler: scheduler.map(|status| scheduling(&status.borrow())),
            close_error: worker.subscribe_close_error().borrow().clone(),
        })
    }

    /// Explicit invocation only. Human-text routing and scheduler binding remain host operations.
    pub async fn invoke(&self, request: InvokeFlowRequest) -> Result<InvokeFlowResponse> {
        if request.request_id.is_nil() {
            return Err(ClientError::InvalidRequest(
                "request identity must not be nil",
            ));
        }
        let worker = self.flow(&request.flow).await?;
        let user = request
            .user
            .map(|user| {
                let attachments =
                    atman_runtime::attachment_store::AttachmentStore::at(std::path::Path::new(""));
                Ok::<_, ClientError>(atman_runtime::flow_store::inbox::UserContent {
                    text: user.text,
                    images: user
                        .images
                        .iter()
                        .map(|image| {
                            attachments.import_base64(&image.data_base64, image.name.as_deref())
                        })
                        .collect::<std::result::Result<_, _>>()
                        .map_err(|error| ClientError::Attachment(Box::new(error)))?,
                    presentation: None,
                })
            })
            .transpose()?;
        let accepted = worker
            .accept_operator_input(
                self.operator.binding(),
                DeliveryRequest {
                    expected_head: request.expected_head.map(|head| {
                        atman_runtime::flow_store::branches::HistoryHead {
                            branch: head.branch.into(),
                            version: head.version.into(),
                        }
                    }),
                    request_id: request.request_id,
                    sender: self.operator.input(),
                    recipient: core_address(&request.flow),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Invocation {
                        user,
                        arguments: request.arguments,
                        invocation: request.invocation,
                    },
                },
                |_| Ok(()),
            )
            .await?;
        Ok(InvokeFlowResponse {
            acceptance: FlowAcceptance {
                message_id: accepted.message_id,
                receipt: position(&accepted.receipt)?,
                disposition: match accepted.disposition {
                    InboxDisposition::Pending => InputDisposition::Pending,
                    InboxDisposition::InputCommitted { by } => {
                        InputDisposition::InputCommitted { by: position(&by)? }
                    }
                    InboxDisposition::Cancelled { by } => {
                        InputDisposition::Cancelled { by: position(&by)? }
                    }
                },
            },
        })
    }

    /// Observe the authenticated operator's original receipt in one owner snapshot.
    pub async fn input_status(&self, request: InputStatusRequest) -> Result<InputStatusResponse> {
        flow_identity(&request.flow)?;
        if request.request_id.is_nil() {
            return Err(ClientError::InvalidRequest(
                "request identity must not be nil",
            ));
        }
        let observation = self
            .catalog()
            .observe_operator_input(
                &core_address(&request.flow),
                self.operator.binding(),
                request.request_id,
                |_| Ok(()),
            )
            .await?;
        Ok(InputStatusResponse {
            flow: request.flow,
            request_id: request.request_id,
            observation: observation.map(input_observation).transpose()?,
        })
    }

    /// Observe retained local receipts without resubmitting or opening execution.
    pub async fn retained_input_status(
        &self,
        request: InputStatusRequest,
    ) -> Result<InputStatusResponse> {
        self.input_status(request).await
    }

    pub async fn control(&self, request: ControlFlowRequest) -> Result<ControlFlowResponse> {
        flow_identity(&request.flow)?;
        if request.request_id.is_nil() || request.pause_key.is_some_and(|key| key.is_nil()) {
            return Err(ClientError::InvalidRequest(
                "control identity must not be nil",
            ));
        }
        match (request.action, request.pause_key) {
            (FlowControl::ReleasePause, Some(_))
            | (
                FlowControl::Pause
                | FlowControl::Resume
                | FlowControl::CancelTurn
                | FlowControl::Recover
                | FlowControl::Close,
                None,
            ) => (),
            _ => {
                return Err(ClientError::InvalidRequest(
                    "only release_pause requires a selected pause key",
                ));
            }
        }
        let worker = self.flow(&request.flow).await?;
        let receipt = worker
            .control(
                ControlRequest {
                    id: request.request_id,
                    sender: LocalOperator::NATIVE.sender(),
                    target: core_address(&request.flow),
                    expected_revision: request.expected_revision,
                    pause_key: request
                        .pause_key
                        .unwrap_or(Uuid::from_bytes(*b"atman-user-pause"))
                        .into(),
                    action: match request.action {
                        FlowControl::Pause => ControlAction::Pause,
                        FlowControl::Resume => ControlAction::Resume,
                        FlowControl::ReleasePause => ControlAction::ReleasePause,
                        FlowControl::CancelTurn => ControlAction::CancelTurn,
                        FlowControl::Recover => ControlAction::Recover,
                        FlowControl::Close => ControlAction::Close,
                    },
                },
                |_| Ok(()),
            )
            .await?;
        Ok(ControlFlowResponse {
            request_id: request.request_id,
            receipt: position(&receipt.receipt)?,
        })
    }

    pub async fn events(&self, request: ReadFlowEventsRequest) -> Result<FlowEventPage> {
        flow_identity(&request.flow)?;
        let page = self
            .catalog()
            .events_page(
                &core_address(&request.flow),
                request.after.map(|after| receipt(&after).into()),
                limit(request.limit, 128)?,
                |_| Ok(()),
            )
            .await?;
        event_page(page)
    }

    pub async fn subscribe_events(
        &self,
        request: ReadFlowEventsRequest,
    ) -> Result<EventSubscription> {
        let page_size = limit(request.limit, 128)?;
        let stream = self
            .flow(&request.flow)
            .await?
            .subscribe_updates(
                request.after.as_ref().map(|after| receipt(after).into()),
                page_size,
            )
            .await?;
        let stream = stream
            .map(|page| page.map_err(ClientError::from).and_then(event_page))
            .boxed();
        Ok(EventSubscription::new(request.flow, request.after, stream))
    }

    fn catalog(&self) -> SessionCatalog {
        SessionCatalog::new(Some(self.host.clone()), Some(self.data.as_ref().clone()))
    }
}

fn execution(state: &atman_rt::FlowInstance<atman_runtime::event::AtmanUuid>) -> ExecutionView {
    ExecutionView {
        revision: state.revision(),
        state: state.state().clone(),
        pause_count: state.pauses().len(),
        pause_keys: state.pauses().to_vec(),
        can_start_turn: state.can_start_turn(),
    }
}

fn scheduling(state: &SchedulingStatus) -> SchedulingView {
    match state {
        SchedulingStatus::Running => SchedulingView::Running,
        SchedulingStatus::Stopped => SchedulingView::Stopped,
        SchedulingStatus::Failed(error) => SchedulingView::Failed {
            error: error.clone(),
        },
    }
}

fn core_address(flow: &FlowAddress) -> FlowRef {
    FlowRef {
        session_id: flow.session_id.0.into(),
        instance_id: atman_rt::FlowInstanceId(flow.instance_id.into()),
    }
}

fn address(flow: &FlowRef) -> FlowAddress {
    FlowAddress {
        session_id: atman_proto::SessionId(flow.session_id.0),
        instance_id: flow.instance_id.0.0,
    }
}

fn cursor(value: &CommitReceipt) -> FlowCursor {
    FlowCursor {
        journal_id: value.journal_id,
        sequence: value.sequence,
        digest: value.digest.clone(),
    }
}

fn receipt(value: &FlowCursor) -> CommitReceipt {
    CommitReceipt {
        journal_id: value.journal_id,
        sequence: value.sequence,
        digest: value.digest.clone(),
    }
}

fn position(value: &FlowPosition) -> Result<FlowCursor> {
    Ok(cursor(value.require_durable()?))
}

fn input_observation(
    value: InputObservation,
) -> Result<atman_proto::responses::FlowInputObservation> {
    Ok(atman_proto::responses::FlowInputObservation {
        message_id: value.message_id,
        accepted: position(&value.accepted)?,
        observed_receipt: position(&value.observed_receipt)?,
        input: match value.input {
            CoreInputDisposition::Pending => InputConsumption::Pending,
            CoreInputDisposition::Cancelled { receipt } => InputConsumption::Cancelled {
                receipt: position(&receipt)?,
            },
            CoreInputDisposition::Committed { lease, receipt } => InputConsumption::Committed {
                receipt: position(&receipt)?,
                lease: InputTurn {
                    flow: address(&lease.flow),
                    turn: lease.turn.0.0,
                    run: lease.run.0.0,
                    epoch: lease.epoch,
                },
            },
        },
    })
}

fn event_page(page: FlowPage<FlowUpdate>) -> Result<FlowEventPage> {
    Ok(FlowEventPage {
        batches: page
            .batches
            .into_iter()
            .map(|batch| {
                Ok(FlowEventBatch {
                    receipt: position(&batch.receipt)?,
                    facts: batch
                        .facts
                        .into_iter()
                        .map(serde_json::to_value)
                        .collect::<std::result::Result<_, _>>()?,
                })
            })
            .collect::<Result<_>>()?,
        next: page.next.as_ref().map(position).transpose()?,
        has_more: page.has_more,
    })
}
