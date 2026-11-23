//! Independent bounded snapshot transfer; no filesystem paths or execution owners cross the port.

use super::*;
use crate::{
    flow_runtime::source_capture::{
        BoundaryReferenceRequest, CapturedOrigin, ReferenceOmissions, ReferenceRequest,
        ReferenceSnapshot, ResolvedConversationBoundary, SourceCapture, SourceSelection,
    },
    flow_store::{
        source_capture::{FullReadKind, FullReadPlan, ReferencePlan},
        transient::TransientAllocation,
    },
    peer_authority::{AuthorityGuard, AuthorityKey, AuthorityMode, AuthorityPlan},
    tools::tool_output::OutputStore,
};
use std::{
    collections::BTreeMap,
    io::Write,
    time::{Duration, Instant},
};

const CHUNK: usize = 64 * 1024;
const MAX_BYTES: usize = 128 * 1024 * 1024 - 16 * 1024;
pub(super) mod events;

#[cfg(test)]
#[path = "capture/tests.rs"]
mod architecture_tests;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum CaptureRequest {
    Reference(Box<ReferenceRequest>),
    Boundary(Box<BoundaryReferenceRequest>),
    Context(FlowRef),
    Output(FlowRef),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadKind {
    Reference,
    Context,
    Output,
    Events,
}

impl ReadKind {
    pub(super) fn action(self) -> FlowAction {
        if self == Self::Events {
            return FlowAction::Watch;
        }
        if self == Self::Output {
            FlowAction::ReadOutput
        } else {
            FlowAction::ReadContext
        }
    }
}

impl CaptureRequest {
    pub(super) fn flow(&self) -> &FlowRef {
        match self {
            Self::Reference(request) => &request.flow,
            Self::Boundary(request) => &request.observed.flow,
            Self::Context(flow) | Self::Output(flow) => flow,
        }
    }
    pub(super) fn kind(&self) -> ReadKind {
        match self {
            Self::Reference(_) | Self::Boundary(_) => ReadKind::Reference,
            Self::Context(_) => ReadKind::Context,
            Self::Output(_) => ReadKind::Output,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Transfer {
    owner: PeerIdentity,
    token: Uuid,
    length: usize,
    digest: String,
}

pub(in crate::flow_runtime) struct CapturedReference {
    pub reference: ReferenceSnapshot,
    pub boundary: Option<ResolvedConversationBoundary>,
}

#[derive(Clone)]
pub struct NativeSnapshot {
    source: SourceSelection,
    origin: CapturedOrigin,
    data: NativeData,
}

#[derive(Clone)]
enum NativeData {
    Reference {
        reference: ReferenceSnapshot,
        boundary: Option<Box<ResolvedConversationBoundary>>,
    },
    Context(Vec<Message>),
    Output(Vec<crate::flow_store::publication::PublishedMessage>),
}

impl std::fmt::Debug for NativeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSnapshot")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl NativeSnapshot {
    fn kind(&self) -> ReadKind {
        match self.data {
            NativeData::Reference { .. } => ReadKind::Reference,
            NativeData::Context(_) => ReadKind::Context,
            NativeData::Output(_) => ReadKind::Output,
        }
    }

    fn into_wire(self) -> io::Result<Payload> {
        let (kind, messages, output, outputs, omissions, boundary) = match self.data {
            NativeData::Reference {
                reference,
                boundary,
            } => (
                ReadKind::Reference,
                reference.messages.to_vec(),
                None,
                reference.outputs.captured_contents()?.clone(),
                reference.omissions,
                boundary.map(|value| *value),
            ),
            NativeData::Context(messages) => (
                ReadKind::Context,
                messages,
                None,
                BTreeMap::new(),
                ReferenceOmissions::default(),
                None,
            ),
            NativeData::Output(output) => (
                ReadKind::Output,
                Vec::new(),
                Some(output),
                BTreeMap::new(),
                ReferenceOmissions::default(),
                None,
            ),
        };
        Ok(Payload {
            source: self.source,
            origin: self.origin,
            kind,
            messages,
            output,
            outputs,
            omissions,
            boundary,
        })
    }
}

impl Payload {
    fn into_native(self) -> io::Result<NativeSnapshot> {
        let data = match self.kind {
            ReadKind::Reference => NativeData::Reference {
                reference: ReferenceSnapshot {
                    messages: self.messages.into(),
                    outputs: OutputStore::retained(self.outputs).map_err(io::Error::other)?,
                    omissions: self.omissions,
                },
                boundary: self.boundary.map(Box::new),
            },
            ReadKind::Context => NativeData::Context(self.messages),
            ReadKind::Output => NativeData::Output(
                self.output
                    .ok_or_else(|| protocol("output snapshot body is absent"))?,
            ),
            ReadKind::Events => {
                return Err(protocol("observable events are not conversation snapshots"));
            }
        };
        Ok(NativeSnapshot {
            source: self.source,
            origin: self.origin,
            data,
        })
    }
}

#[derive(Deserialize, Serialize)]
struct Payload {
    kind: ReadKind,
    source: SourceSelection,
    origin: CapturedOrigin,
    messages: Vec<Message>,
    outputs: BTreeMap<String, String>,
    omissions: ReferenceOmissions,
    boundary: Option<ResolvedConversationBoundary>,
    output: Option<Vec<crate::flow_store::publication::PublishedMessage>>,
}

pub(in crate::flow_runtime) struct Captured {
    owner: Uuid,
    nonce: Uuid,
    target: FlowRef,
    kind: ReadKind,
    bytes: Vec<u8>,
    _allocation: TransientAllocation,
}

struct Bounded(Vec<u8>);

enum Plan {
    Reference(ReferencePlan),
    Full(FullReadPlan),
    Recorded {
        data: PathBuf,
        flow: FlowRef,
        kind: FullReadKind,
        catalog: crate::event_log::journal::CommitReceipt,
    },
}

enum Prepared {
    Native(Box<NativeSnapshot>),
    Wire(Vec<u8>),
}

struct Release {
    runtime: tokio::runtime::Handle,
    host: RuntimeHost,
    port: Arc<dyn PeerPort>,
    owner: PeerIdentity,
    principal: InvocationPrincipal,
    target: FlowRef,
    token: Uuid,
    kind: ReadKind,
}

impl Drop for Release {
    fn drop(&mut self) {
        let (host, port, owner, principal, target, token, read) = (
            self.host.clone(),
            self.port.clone(),
            self.owner.clone(),
            self.principal.clone(),
            self.target.clone(),
            self.token,
            self.kind,
        );
        self.runtime.spawn(async move {
            let operation = PeerOperation::ReleaseCapture {
                target,
                token,
                read,
            };
            let Ok(binding) = operation.binding(owner.runtime_generation) else {
                return;
            };
            let call = PeerCall::Execute {
                principal: PeerPrincipal::Invocation(principal),
                binding,
                operation: Box::new(operation),
            };
            if owner.runtime_generation == host.generation() {
                if let Ok(caller) = host.peer_identity() {
                    let _ = handle_peer_call(host, caller, call).await;
                }
            } else {
                let _ = port.call(owner, call).await;
            }
        });
    }
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(protocol("captured snapshot exceeds its transfer capacity"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn register_transfer(
    host: &RuntimeHost,
    owner: Uuid,
    nonce: Uuid,
    target: FlowRef,
    kind: ReadKind,
    bytes: Vec<u8>,
) -> io::Result<Transfer> {
    let token = Uuid::new_v4();
    let allocation = host.peer_transfer_retention(token)?.reserve(bytes.len())?;
    let transfer = Transfer {
        owner: host.peer_identity()?,
        token,
        length: bytes.len(),
        digest: blake3::hash(&bytes).to_hex().to_string(),
    };
    host.invocation_registry().captures.lock().unwrap().insert(
        token,
        Arc::new(Captured {
            owner,
            nonce,
            target,
            kind,
            bytes,
            _allocation: allocation,
        }),
    );
    let weak = host.downgrade();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(300)).await;
        if let Ok(host) = weak.upgrade() {
            drop(
                host.invocation_registry()
                    .captures
                    .lock()
                    .unwrap()
                    .remove(&token),
            );
        }
    });
    Ok(transfer)
}

fn retire_transfer(
    host: &RuntimeHost,
    owner: Uuid,
    nonce: Uuid,
    target: &FlowRef,
    token: Uuid,
    kind: ReadKind,
) {
    let mut captures = host.invocation_registry().captures.lock().unwrap();
    if captures.get(&token).is_some_and(|value| {
        value.owner == owner
            && value.nonce == nonce
            && &value.target == target
            && value.kind == kind
    }) {
        let retired = captures.remove(&token);
        drop(captures);
        drop(retired);
    }
}

fn read_chunk(
    host: &RuntimeHost,
    owner: Uuid,
    nonce: Uuid,
    target: &FlowRef,
    token: Uuid,
    offset: usize,
    kind: ReadKind,
) -> io::Result<PeerReply> {
    let capture = host
        .invocation_registry()
        .captures
        .lock()
        .unwrap()
        .get(&token)
        .cloned()
        .ok_or_else(|| protocol("snapshot transfer is no longer available"))?;
    if capture.owner != owner
        || capture.nonce != nonce
        || &capture.target != target
        || capture.kind != kind
        || offset > capture.bytes.len()
    {
        return Err(denied("snapshot chunk belongs to another original request"));
    }
    let end = offset.saturating_add(CHUNK).min(capture.bytes.len());
    Ok(PeerReply::CaptureChunk {
        offset,
        bytes: capture.bytes[offset..end].to_vec(),
    })
}

async fn authorized(
    host: &RuntimeHost,
    principal: &PeerPrincipal,
    binding: &EffectBinding,
) -> io::Result<Arc<AuthorityGuard>> {
    let PeerPrincipal::Invocation(principal_source) = principal else {
        return Err(denied(
            "reference capture requires a live original invocation",
        ));
    };
    let AccessTarget::Flow(target) = &binding.target else {
        return Err(protocol("reference transfer requires one Flow"));
    };
    let plan = AuthorityPlan::new([
        (AuthorityKey::Profile, AuthorityMode::Shared),
        (
            AuthorityKey::Session(principal_source.source.session_id.0),
            AuthorityMode::Shared,
        ),
        (
            AuthorityKey::Session(target.session_id.0),
            AuthorityMode::Shared,
        ),
    ])?;
    let guard = Arc::new(
        host.peer_authority(target.session_id.0)?
            .acquire(
                plan,
                tokio_util::sync::CancellationToken::new(),
                Instant::now() + Duration::from_secs(15),
            )
            .await?,
    );
    let witness = super::dispatch::validate_source(host, principal, binding, guard.clone()).await?;
    if witness.source != principal_source.source || witness.binding != *binding {
        return Err(denied("snapshot witness differs from its original binding"));
    }
    let action = *binding
        .actions
        .first()
        .ok_or_else(|| protocol("snapshot action is absent"))?;
    if !witness.source_grants.contains(&action)
        && !super::dispatch::profile_allows(host, &witness.source, &binding.target, action).await?
    {
        return Err(denied("reference is outside the current ReadContext grant"));
    }
    Ok(guard)
}

pub(super) async fn execute(
    host: RuntimeHost,
    principal: PeerPrincipal,
    binding: EffectBinding,
    operation: PeerOperation,
) -> io::Result<PeerReply> {
    let PeerPrincipal::Invocation(actor) = &principal else {
        return Err(denied("snapshot transfer requires a live invocation"));
    };
    match operation {
        PeerOperation::ReleaseCapture {
            target,
            token,
            read,
        } => {
            // Retiring an exact capability does not need a permission that may have been revoked.
            retire_transfer(&host, actor.owner, actor.nonce, &target, token, read);
            Ok(PeerReply::CaptureReleased)
        }
        PeerOperation::CaptureChunk {
            target,
            token,
            offset,
            read,
        } => {
            let _guard = authorized(&host, &principal, &binding).await?;
            read_chunk(
                &host,
                actor.owner,
                actor.nonce,
                &target,
                token,
                offset,
                read,
            )
        }
        PeerOperation::Capture { request } => {
            let kind = request.kind();
            let target = request.flow().clone();
            let session = host.session(target.session_id.0)?;
            let retention = session.retain_source().await?;
            let worker = session.loaded_flow(&target.instance_id);
            if worker.as_ref().is_some_and(|worker| worker.is_transient()) && actor.source != target
            {
                return Err(denied(
                    "temporary source cannot be captured by another flow",
                ));
            }
            let guard = authorized(&host, &principal, &binding).await?;
            let catalog = session.snapshot_with_authority(guard.clone()).await?;
            let (plan, boundary, origin) = if let Some(worker) = worker {
                worker
                    .query_with_authority(guard.clone(), move |store| {
                        let (plan, boundary) = match request {
                            CaptureRequest::Reference(request) => (
                                Plan::Reference(store.capture_reference_plan(&request)?),
                                None,
                            ),
                            CaptureRequest::Boundary(request) => {
                                let (plan, boundary) =
                                    store.capture_reference_boundary_plan(&request)?;
                                (Plan::Reference(plan), Some(boundary))
                            }
                            CaptureRequest::Context(_) => (
                                Plan::Full(
                                    store
                                        .capture_full_read_plan(FullReadKind::Context, MAX_BYTES)?,
                                ),
                                None,
                            ),
                            CaptureRequest::Output(_) => (
                                Plan::Full(
                                    store
                                        .capture_full_read_plan(FullReadKind::Output, MAX_BYTES)?,
                                ),
                                None,
                            ),
                        };
                        Ok((
                            plan,
                            boundary,
                            Some(
                                store
                                    .source_capture_origin(catalog.cursor, catalog.catalog.project),
                            ),
                        ))
                    })
                    .await?
            } else {
                let kind = match request {
                    CaptureRequest::Context(_) => FullReadKind::Context,
                    CaptureRequest::Output(_) => FullReadKind::Output,
                    _ => {
                        return Err(protocol(
                            "open the source Flow before capturing its reference",
                        ));
                    }
                };
                (
                    Plan::Recorded {
                        data: host.peer_identity()?.canonical_data_root,
                        flow: target.clone(),
                        kind,
                        catalog: catalog.cursor,
                    },
                    None,
                    None,
                )
            };
            drop(guard);
            let native = actor.owner == host.generation();
            let (prepared, retention) = crate::read_work::capture(move || {
                let captured = match plan {
                    Plan::Reference(plan) => {
                        let origin =
                            origin.ok_or_else(|| protocol("reference origin is absent"))?;
                        let (source, reference) = ReferencePlan::prepare(plan)?;
                        NativeSnapshot {
                            source,
                            origin,
                            data: NativeData::Reference {
                                reference,
                                boundary: boundary.map(Box::new),
                            },
                        }
                    }
                    Plan::Full(plan) => {
                        let origin =
                            origin.ok_or_else(|| protocol("conversation origin is absent"))?;
                        let (source, messages, output) = plan.prepare()?;
                        let data = match kind {
                            ReadKind::Context => NativeData::Context(messages),
                            ReadKind::Output => NativeData::Output(
                                output.ok_or_else(|| protocol("output body is absent"))?,
                            ),
                            ReadKind::Reference | ReadKind::Events => {
                                return Err(protocol("unexpected full reference plan"));
                            }
                        };
                        NativeSnapshot {
                            source,
                            origin,
                            data,
                        }
                    }
                    Plan::Recorded {
                        data,
                        flow,
                        kind: full_kind,
                        catalog,
                    } => {
                        let captured = crate::flow_store::RuntimeStore::capture_recorded_full_read(
                            &data, &flow, full_kind, MAX_BYTES, &catalog,
                        )?;
                        let data = match kind {
                            ReadKind::Context => NativeData::Context(captured.messages),
                            ReadKind::Output => NativeData::Output(
                                captured
                                    .output
                                    .ok_or_else(|| protocol("output body is absent"))?,
                            ),
                            ReadKind::Reference | ReadKind::Events => {
                                return Err(protocol("unexpected recorded reference plan"));
                            }
                        };
                        NativeSnapshot {
                            source: captured.source,
                            origin: captured.origin,
                            data,
                        }
                    }
                };
                let prepared = if native {
                    Prepared::Native(Box::new(captured))
                } else {
                    let mut bytes = Bounded(Vec::new());
                    serde_json::to_writer(&mut bytes, &captured.into_wire()?)
                        .map_err(io::Error::other)?;
                    Prepared::Wire(bytes.0)
                };
                Ok::<_, io::Error>((prepared, retention))
            })
            .await
            .map_err(io::Error::other)??;
            let _guard = authorized(&host, &principal, &binding).await?;
            let payload = match prepared {
                Prepared::Native(snapshot) => {
                    return Ok(PeerReply::NativeSnapshot(snapshot));
                }
                Prepared::Wire(bytes) => bytes,
            };
            let transfer =
                register_transfer(&host, actor.owner, actor.nonce, target, kind, payload)?;
            drop(retention);
            Ok(PeerReply::Capture(transfer))
        }
        _ => Err(protocol("unexpected snapshot operation")),
    }
}

impl FlowClient {
    pub(in crate::flow_runtime) async fn peer_request_status(
        &self,
        target: FlowRef,
        request_id: Uuid,
    ) -> io::Result<Option<serde_json::Value>> {
        if self.host.peer_port().is_none() {
            return Ok(None);
        }
        self.authenticate()?;
        let worker = self.host.flow(self.source()).await?;
        let guard = super::client::read_guard(&self.host, self.source().session_id.0).await?;
        let client = self.clone();
        let (sent, source_receipt) = worker
            .query_with_authority(guard, move |store| {
                client.authenticate()?;
                Ok((
                    store
                        .send_summary(request_id)
                        .cloned()
                        .ok_or_else(|| protocol("request does not belong to the current sender"))?,
                    store.cursor().clone(),
                ))
            })
            .await?;
        if sent.recipient != target {
            return Err(protocol(
                "request recipient does not match the selected flow",
            ));
        }
        let input = match self
            .peer_operation(PeerOperation::InputStatus { target, request_id })
            .await?
        {
            Some(PeerReply::InputStatus(input)) => input,
            _ => return Err(protocol("unexpected input status reply")),
        };
        if let crate::flow_store::outbox::SendStatus::Accepted { delivery, .. } = &sent.status {
            let observed = input
                .as_ref()
                .ok_or_else(|| protocol("accepted request is absent from the receiver journal"))?;
            if observed.message_id != delivery.message_id || observed.accepted != delivery.receipt {
                return Err(protocol(
                    "receiver acceptance differs from the recorded delivery receipt",
                ));
            }
        }
        Ok(Some(
            serde_json::json!({"request_id": request_id, "delivery": sent.status, "source_receipt": source_receipt, "receiver": input}),
        ))
    }

    async fn transfer_snapshot(
        &self,
        request: CaptureRequest,
    ) -> io::Result<Option<NativeSnapshot>> {
        let target = request.flow().clone();
        let kind = request.kind();
        let expected_boundary = match &request {
            CaptureRequest::Boundary(request) => Some(request.cutoff.clone()),
            _ => None,
        };
        let Some(reply) = self
            .peer_operation(PeerOperation::Capture { request })
            .await?
        else {
            return Ok(None);
        };
        if let PeerReply::NativeSnapshot(snapshot) = reply {
            if snapshot.source.flow != target || snapshot.kind() != kind {
                return Err(protocol("native snapshot source or kind changed"));
            }
            if let NativeData::Reference { boundary, .. } = &snapshot.data
                && boundary.as_ref().map(|boundary| &boundary.selected)
                    != expected_boundary.as_ref()
            {
                return Err(protocol("native snapshot boundary changed"));
            }
            return Ok(Some(*snapshot));
        }
        let PeerReply::Capture(transfer) = reply else {
            return Err(protocol("unexpected snapshot transfer reply"));
        };
        let identity = self.host.peer_identity()?;
        if transfer.owner.profile_id != identity.profile_id
            || transfer.owner.canonical_data_root != identity.canonical_data_root
        {
            return Err(protocol(
                "snapshot transfer belongs to another authority domain",
            ));
        }
        let _release = Release {
            runtime: tokio::runtime::Handle::current(),
            host: self.host.clone(),
            port: self
                .host
                .peer_port()
                .ok_or_else(|| protocol("snapshot port stopped"))?,
            owner: transfer.owner.clone(),
            principal: self.peer_principal(),
            target: target.clone(),
            token: transfer.token,
            kind,
        };
        if transfer.length > MAX_BYTES {
            return Err(protocol("snapshot transfer exceeds its capacity"));
        }
        let mut bytes = Vec::with_capacity(transfer.length);
        while bytes.len() < transfer.length {
            let offset = bytes.len();
            match self
                .peer_operation_at(
                    PeerOperation::CaptureChunk {
                        target: target.clone(),
                        token: transfer.token,
                        offset,
                        read: kind,
                    },
                    Some(transfer.owner.clone()),
                )
                .await?
            {
                Some(PeerReply::CaptureChunk {
                    offset: received,
                    bytes: chunk,
                }) if received == offset
                    && !chunk.is_empty()
                    && chunk.len() <= CHUNK
                    && chunk.len() <= transfer.length - offset =>
                {
                    bytes.extend(chunk)
                }
                _ => {
                    return Err(protocol(
                        "snapshot chunk changed its original transfer position",
                    ));
                }
            }
        }
        if blake3::hash(&bytes).to_hex().as_str() != transfer.digest {
            return Err(protocol("snapshot transfer digest changed"));
        }
        crate::read_work::capture(move || {
            let payload: Payload = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if payload.kind != kind
                || payload.source.flow != target
                || payload.boundary.as_ref().map(|boundary| &boundary.selected)
                    != expected_boundary.as_ref()
            {
                return Err(protocol(
                    "snapshot source, kind or boundary changed during transfer",
                ));
            }
            payload.into_native().map(Some)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(in crate::flow_runtime) async fn peer_capture(
        &self,
        request: CaptureRequest,
    ) -> io::Result<Option<SourceCapture<CapturedReference>>> {
        let Some(payload) = self.transfer_snapshot(request).await? else {
            return Ok(None);
        };
        let NativeData::Reference {
            reference,
            boundary,
        } = payload.data
        else {
            return Err(protocol("reference snapshot body changed"));
        };
        Ok(Some(SourceCapture {
            source: payload.source,
            origin: payload.origin,
            product: CapturedReference {
                boundary: boundary.map(|value| *value),
                reference,
            },
        }))
    }

    pub(in crate::flow_runtime) async fn peer_context_snapshot(
        &self,
        target: FlowRef,
    ) -> io::Result<Option<Vec<Message>>> {
        self.transfer_snapshot(CaptureRequest::Context(target))
            .await
            .and_then(|value| {
                value
                    .map(|payload| match payload.data {
                        NativeData::Context(messages) => Ok(messages),
                        _ => Err(protocol("context snapshot body changed")),
                    })
                    .transpose()
            })
    }

    pub(in crate::flow_runtime) async fn peer_output_snapshot(
        &self,
        target: FlowRef,
    ) -> io::Result<Option<Vec<crate::flow_store::publication::PublishedMessage>>> {
        self.transfer_snapshot(CaptureRequest::Output(target))
            .await?
            .map(|payload| match payload.data {
                NativeData::Output(output) => Ok(output),
                _ => Err(protocol("output snapshot body changed")),
            })
            .transpose()
    }
}

#[cfg(test)]
mod cold_read_contracts {
    use super::*;
    use crate::flow_runtime::peer::tests::Peers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_cold_peer_reads_keep_images_and_receipts_without_creating_an_execution_owner() {
        let peers = Peers::new().await;
        let target = peers.target.address().clone();
        peers
            .target
            .transition(
                peers.target.state().await.unwrap().revision(),
                crate::flow_store::FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )
            .await
            .unwrap();
        let state = peers.target.state().await.unwrap();
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
            state.state()
        else {
            panic!("target turn is active");
        };
        let lease = active.lease.clone();
        let bytes = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        ];
        let original = peers.data.path().join("original.png");
        std::fs::write(&original, bytes).unwrap();
        let image = crate::message::ImageSource {
            media_type: "image/png".into(),
            detail: crate::provider::ImageDetail::Auto,
            data: crate::message::ImageData::Path {
                path: original.clone(),
            },
        };
        let mut message = Message::assistant_text(lease.turn.clone(), "x".repeat(70 * 1024));
        message.origin = crate::message::MessageOrigin::FinalAnswer;
        message
            .parts
            .push(crate::message::MessagePart::Image { source: image });
        let committed = peers
            .target
            .request(move |store| {
                store.publish_message(crate::flow_store::publication::MessagePublication {
                    tool_artifacts: Vec::new(),
                    id: Uuid::now_v7(),
                    lease: lease.clone(),
                    run: lease.run.clone(),
                    context_run: None,
                    message,
                })
            })
            .await
            .unwrap();
        std::fs::remove_file(original).unwrap();
        drop(peers.target.clone().shutdown().await.unwrap());
        let source_log = peers
            .data
            .path()
            .join("runtime-v1/sessions")
            .join(target.session_id.to_string())
            .join("flows")
            .join(target.instance_id.to_string())
            .join("events.jsonl");
        let before = std::fs::read(&source_log).unwrap();
        let context = peers.client.read_context(target.clone()).await.unwrap();
        let output = peers.client.read_output(target.clone()).await.unwrap();
        assert_eq!(context.len(), 1);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].receipt, committed.receipt);
        assert_eq!(context[0].text_concat().len(), 70 * 1024);
        assert_eq!(output[0].publication.message, context[0]);
        for message in [&context[0], &output[0].publication.message] {
            let image = message
                .parts
                .iter()
                .find_map(|part| match part {
                    crate::message::MessagePart::Image { source } => Some(source),
                    _ => None,
                })
                .unwrap();
            assert!(matches!(
                image.data,
                crate::message::ImageData::Base64 { .. }
            ));
            assert_eq!(crate::attachment_store::image_bytes(image).unwrap(), bytes);
        }
        assert_eq!(std::fs::read(source_log).unwrap(), before);
        assert!(
            peers
                .target_host
                .session(target.session_id.0)
                .unwrap()
                .loaded_flow(&target.instance_id)
                .is_none()
        );
        let source_state = peers.source.state().await.unwrap();
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
            source_state.state()
        else {
            panic!("source turn is active");
        };
        peers
            .source
            .transition(
                source_state.revision(),
                crate::flow_store::FlowTransition::TurnFinished {
                    lease: active.lease.clone(),
                    outcome: atman_rt::TurnOutcome::Completed,
                },
            )
            .await
            .unwrap();
        peers.shutdown().await;
    }
}
