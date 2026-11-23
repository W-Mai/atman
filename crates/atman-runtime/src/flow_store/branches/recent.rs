//! Bounded original conversation reads from a fixed, integrity-checked journal observation.

use std::{
    io::{self, Write},
    num::NonZeroUsize,
    path::PathBuf,
    sync::Arc,
};

use super::{inspection::BranchInspection, *};
use crate::{
    event::{AtmanUuid, FlowRunId},
    event_log::journal::{
        InspectionBudget, InspectionCapacityKind, InspectionProgress, InspectionStep,
        JournalInspectionReader, JournalReader, ReadStamp,
    },
    flow_store::{
        FlowFact, SessionInspection,
        conversation::ConversationPlan,
        inbox::InputContent,
        publication::ToolResultArtifact,
        source_capture::{ConversationCutoff, SourceCaptureControl},
    },
    message::{ImageData, ImageSource, Message, MessageOrigin, MessagePart, MessageRole},
    projection::flow::{AuthoredTurnInputs, FlowRecordId},
};

/// Stable identity inside one original display record; `record.item` retains its native meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationItemId {
    pub record: FlowRecordId,
    pub message: usize,
}

/// File integrity and owner acknowledgement are distinct evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationEvidence {
    OwnerAcknowledged,
    DiskPrefix { trailing_bytes: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationBoundary {
    pub source: SourceSelection,
    pub observed_head: HistoryHead,
    pub evidence: ConversationEvidence,
}

#[derive(Clone, Debug)]
pub struct RecentConversationRequest {
    pub flow: FlowRef,
    pub expected: Option<SourceSelection>,
    pub expected_head: Option<HistoryHead>,
    pub before: Option<ConversationItemId>,
    pub limit: NonZeroUsize,
}

impl RecentConversationRequest {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.limit.get() > 32 {
            return Err(invalid("conversation page limit must be between 1 and 32"));
        }
        if self
            .expected
            .as_ref()
            .is_some_and(|source| source.flow != self.flow)
        {
            return Err(invalid("conversation selection belongs to another Flow"));
        }
        if self.before.is_some() && (self.expected.is_none() || self.expected_head.is_none()) {
            return Err(invalid(
                "conversation continuation requires its original full source and head",
            ));
        }
        Ok(())
    }
}

/// Host-admitted limits. Raising them is an explicit admission, never an automatic retry.
#[derive(Clone, Copy, Debug)]
pub struct RecentConversationLimits {
    pub retained_positions: NonZeroUsize,
    pub markers: NonZeroUsize,
    pub head_changes: NonZeroUsize,
    pub selection_work: NonZeroUsize,
    pub source_record_bytes: NonZeroUsize,
    pub page_source_bytes: NonZeroUsize,
    pub inline_bytes: NonZeroUsize,
}

impl Default for RecentConversationLimits {
    fn default() -> Self {
        let nonzero = |value| NonZeroUsize::new(value).expect("nonzero constant");
        Self {
            retained_positions: nonzero(16_384),
            markers: nonzero(16_384),
            head_changes: nonzero(256),
            selection_work: nonzero(4_096),
            source_record_bytes: nonzero(4 * 1024 * 1024),
            page_source_bytes: nonzero(8 * 1024 * 1024),
            inline_bytes: nonzero(64 * 1024),
        }
    }
}

impl RecentConversationLimits {
    fn validate(self) -> io::Result<()> {
        if self.retained_positions.get() > 65_536
            || self.markers.get() > 65_536
            || self.head_changes.get() > 1024
            || self.selection_work.get() > 16_384
            || self.source_record_bytes.get() > 4 * 1024 * 1024
            || self.page_source_bytes.get() > 8 * 1024 * 1024
            || self.inline_bytes.get() > 256 * 1024
        {
            return Err(invalid("conversation admission exceeds its hard capacity"));
        }
        Ok(())
    }

    fn admit(self, previous: Self) -> io::Result<()> {
        self.validate()?;
        if self.retained_positions < previous.retained_positions
            || self.markers < previous.markers
            || self.head_changes < previous.head_changes
            || self.selection_work < previous.selection_work
            || self.source_record_bytes < previous.source_record_bytes
            || self.page_source_bytes < previous.page_source_bytes
            || self.inline_bytes < previous.inline_bytes
        {
            return Err(invalid(
                "conversation capacity admission cannot reduce a retained allowance",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationCapacityKind {
    ReadersBusy,
    ScanBytes,
    SourceRecord,
    DecodeWork,
    RetainedIndex,
    BranchMetadata,
    SelectionWork,
    PageSourceBytes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationCapacity {
    pub kind: ConversationCapacityKind,
    pub required_at_least: usize,
    pub allowed: usize,
    pub restart_required: bool,
}

impl std::fmt::Display for ConversationCapacity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "conversation {:?} requires at least {}, allowed {}",
            self.kind, self.required_at_least, self.allowed
        )
    }
}
impl std::error::Error for ConversationCapacity {}

pub enum ConversationPreparationStep {
    Loading(InspectionProgress),
    Capacity(ConversationCapacity),
    Ready(Box<PreparedConversationSource>),
}

pub struct RecentConversationPreparation {
    pending: Option<ColdPreparation>,
}

struct ColdPreparation {
    scanner: JournalInspectionReader<FlowFact>,
    stamp: Option<ReadStamp>,
    branches: BranchInspection,
    markers: ConversationMarkers,
    directory: PathBuf,
    request: RecentConversationRequest,
    limits: RecentConversationLimits,
    control: SourceCaptureControl,
}

#[derive(Clone, Copy)]
struct ConversationMarker {
    sequence: u64,
    shared: bool,
}

#[derive(Default)]
struct ConversationMarkers {
    activation: Option<u64>,
    inputs: Vec<ConversationMarker>,
    publications: Vec<ConversationMarker>,
    authored: AuthoredTurnInputs,
    cutoffs: Vec<(u64, usize, Uuid)>,
}

impl ConversationMarkers {
    fn admit(&mut self, facts: &[FlowFact], limit: usize) -> io::Result<()> {
        let inputs = usize::from(
            facts
                .iter()
                .any(|fact| matches!(fact, FlowFact::InputCommitted(_))),
        );
        let publications = usize::from(
            facts
                .iter()
                .any(|fact| matches!(fact, FlowFact::MessagePublished(_))),
        );
        let required = self
            .inputs
            .len()
            .saturating_add(self.publications.len())
            .saturating_add(inputs)
            .saturating_add(publications);
        if required > limit {
            return Err(io::Error::other(ConversationCapacity {
                kind: ConversationCapacityKind::BranchMetadata,
                required_at_least: required,
                allowed: limit,
                restart_required: true,
            }));
        }
        reserve_markers(&mut self.inputs, inputs, limit)?;
        reserve_markers(&mut self.publications, publications, limit)?;
        Ok(())
    }

    fn apply(&mut self, sequence: u64, facts: &[FlowFact], limit: usize) -> io::Result<()> {
        let mut input = None;
        let mut publication = None;
        for (index, fact) in facts.iter().enumerate() {
            let shared = !fact.shared_messages().is_empty();
            match fact {
                FlowFact::Activated(activation) => {
                    if shared {
                        self.activation = Some(sequence);
                    }
                    self.authored
                        .accept(activation.envelope().and_then(|value| {
                            value.request.is_editable_user_input().then_some(value.id)
                        }));
                }
                FlowFact::EnvelopeAccepted(value) => {
                    self.authored
                        .accept(value.request.is_editable_user_input().then_some(value.id));
                }
                FlowFact::InboxEdited(edit)
                    if matches!(&edit.change, crate::flow_store::inbox::InboxChange::Cancel) =>
                {
                    self.authored.cancel(edit.message);
                }
                FlowFact::InputCommitted(committed) => {
                    input = Some(input.unwrap_or(false) || shared);
                    if let Some(authored) = self.authored.commit(committed).filter(|_| shared) {
                        self.cutoffs.try_reserve(1).map_err(io::Error::other)?;
                        self.cutoffs.push((sequence, index, authored));
                    }
                }
                FlowFact::MessagePublished(_) => {
                    publication = Some(publication.unwrap_or(false) || shared)
                }
                _ => {}
            }
            let required = self.authored.len().saturating_add(self.cutoffs.len());
            if required > limit {
                return Err(io::Error::other(ConversationCapacity {
                    kind: ConversationCapacityKind::BranchMetadata,
                    required_at_least: required,
                    allowed: limit,
                    restart_required: true,
                }));
            }
        }
        if let Some(shared) = input {
            self.inputs.push(ConversationMarker { sequence, shared });
        }
        if let Some(shared) = publication {
            self.publications
                .push(ConversationMarker { sequence, shared });
        }
        Ok(())
    }

    fn cutoff(
        &self,
        receipt: &CommitReceipt,
        index: usize,
        fact: &FlowFact,
    ) -> Option<ConversationCutoff> {
        let FlowFact::InputCommitted(committed) = fact else {
            return None;
        };
        let found = self
            .cutoffs
            .binary_search_by_key(&(receipt.sequence, index), |(sequence, fact, _)| {
                (*sequence, *fact)
            })
            .ok()?;
        Some(ConversationCutoff::BeforeUserTurn {
            lease: committed.lease.clone(),
            input: self.cutoffs[found].2,
            receipt: receipt.clone(),
        })
    }

    fn retained_bytes(&self) -> usize {
        self.inputs
            .capacity()
            .saturating_add(self.publications.capacity())
            .saturating_mul(std::mem::size_of::<ConversationMarker>())
            .saturating_add(self.cutoffs.capacity().saturating_mul(std::mem::size_of::<(
                u64,
                usize,
                Uuid,
            )>()))
            .saturating_add(self.authored.retained_bytes())
    }
}

impl SessionInspection {
    /// Begin a fixed-length read without opening execution, recovering a tail or replaying a model window.
    pub fn prepare_recent_conversation(
        &self,
        request: RecentConversationRequest,
        limits: RecentConversationLimits,
        control: SourceCaptureControl,
    ) -> io::Result<RecentConversationPreparation> {
        request.validate()?;
        limits.validate()?;
        if request.flow.session_id.0 != self.projection.descriptor.id {
            return Err(invalid("conversation source belongs to another Session"));
        }
        cancelled(&control)?;
        let branches = BranchInspection::new(&self.projection, &request.flow.instance_id)?;
        let directory = self
            .dir
            .join("flows")
            .join(request.flow.instance_id.to_string());
        let scanner = JournalInspectionReader::open(
            &directory.join("events.jsonl"),
            request.flow.instance_id.0.0,
        )?;
        Ok(RecentConversationPreparation {
            pending: Some(ColdPreparation {
                stamp: scanner.read_stamp().ok(),
                scanner,
                branches,
                markers: ConversationMarkers::default(),
                directory,
                request,
                limits,
                control,
            }),
        })
    }
}

impl RecentConversationPreparation {
    pub(crate) fn captured_bytes(&self) -> io::Result<u64> {
        self.pending
            .as_ref()
            .map(|pending| pending.scanner.progress().captured_bytes)
            .ok_or_else(|| invalid("conversation preparation is retired"))
    }

    /// Explicitly admit a larger allowance after a resumable capacity response.
    pub fn admit_limits(&mut self, limits: RecentConversationLimits) -> io::Result<()> {
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| invalid("conversation preparation is retired"))?;
        limits.admit(pending.limits)?;
        pending.limits = limits;
        Ok(())
    }

    pub fn step(
        &mut self,
        mut budget: InspectionBudget,
    ) -> io::Result<ConversationPreparationStep> {
        let pending = self
            .pending
            .as_mut()
            .ok_or_else(|| invalid("conversation preparation is retired"))?;
        if let Err(error) = cancelled(&pending.control) {
            self.pending.take();
            return Err(error);
        }
        budget.max_record_bytes = budget
            .max_record_bytes
            .min(pending.limits.source_record_bytes);
        let limits = pending.limits;
        let result = pending.scanner.step_with_prefix_retained(
            budget,
            limits.retained_positions,
            |receipt, facts, prefix| {
                cancelled(&pending.control)?;
                pending.markers.admit(&facts, limits.markers.get())?;
                let changes = facts
                    .iter()
                    .filter(|fact| matches!(fact, FlowFact::HistoryHeadChanged(_)))
                    .count();
                let required = pending.branches.change_count().saturating_add(changes);
                if required > limits.head_changes.get() {
                    return Err(io::Error::other(ConversationCapacity {
                        kind: ConversationCapacityKind::BranchMetadata,
                        required_at_least: required,
                        allowed: limits.head_changes.get(),
                        restart_required: true,
                    }));
                }
                pending.branches.apply(&receipt, &facts, &prefix)?;
                pending
                    .markers
                    .apply(receipt.sequence, &facts, limits.markers.get())
            },
        );
        match result {
            Err(error) => {
                self.pending.take();
                if let Some(capacity) = error
                    .get_ref()
                    .and_then(|cause| cause.downcast_ref::<ConversationCapacity>())
                {
                    Ok(ConversationPreparationStep::Capacity(capacity.clone()))
                } else {
                    Err(error)
                }
            }
            Ok(InspectionStep::Loading(progress)) => {
                Ok(ConversationPreparationStep::Loading(progress))
            }
            Ok(InspectionStep::Capacity(capacity)) => Ok(ConversationPreparationStep::Capacity(
                ConversationCapacity {
                    kind: match capacity.kind {
                        InspectionCapacityKind::RecordBytes => {
                            ConversationCapacityKind::SourceRecord
                        }
                        InspectionCapacityKind::DecodeWork => ConversationCapacityKind::DecodeWork,
                    },
                    required_at_least: capacity.required_bytes,
                    allowed: capacity.allowed_bytes,
                    restart_required: false,
                },
            )),
            Ok(InspectionStep::RetentionCapacity(capacity)) => Ok(
                ConversationPreparationStep::Capacity(ConversationCapacity {
                    kind: ConversationCapacityKind::RetainedIndex,
                    required_at_least: capacity.required_entries,
                    allowed: capacity.allowed_entries,
                    restart_required: false,
                }),
            ),
            Ok(InspectionStep::Done(_)) => {
                let pending = self.pending.take().expect("active preparation");
                cancelled(&pending.control)?;
                let (inspection, reader) = pending.scanner.finish()?;
                let history = pending.branches.finish(inspection)?;
                if pending
                    .request
                    .expected_head
                    .as_ref()
                    .is_some_and(|head| head != &history.head)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "conversation history head changed",
                    ));
                }
                let source = pending.request.expected.unwrap_or_else(|| SourceSelection {
                    flow: pending.request.flow,
                    branch: history.head.branch,
                    cursor: history.observed.clone(),
                });
                reader.validate_receipt(&source.cursor)?;
                history.branch(source.branch)?;
                if !history.contains(source.branch, source.cursor.sequence)? {
                    return Err(invalid(
                        "conversation cutoff is outside the selected branch",
                    ));
                }
                let spans = history.ancestry_through(source.branch, source.cursor.sequence)?;
                Ok(ConversationPreparationStep::Ready(Box::new(
                    PreparedConversationSource {
                        identity: Arc::new(()),
                        boundary: ConversationBoundary {
                            source,
                            observed_head: history.head.clone(),
                            evidence: ConversationEvidence::DiskPrefix {
                                trailing_bytes: history.trailing_bytes,
                            },
                        },
                        reader,
                        markers: pending.markers,
                        history,
                        spans,
                        directory: pending.directory,
                        stamp: pending.stamp,
                        limits: pending.limits,
                        control: pending.control,
                    },
                )))
            }
        }
    }
}

/// Retains only the original reader, compact metadata and validated branch selection.
pub struct PreparedConversationSource {
    identity: Arc<()>,
    boundary: ConversationBoundary,
    reader: JournalReader,
    markers: ConversationMarkers,
    history: ConversationHistory,
    spans: Vec<ConversationSpan>,
    directory: PathBuf,
    stamp: Option<ReadStamp>,
    limits: RecentConversationLimits,
    control: SourceCaptureControl,
}

#[derive(Clone, Debug)]
pub enum ConversationItemBody {
    Inline(Message),
    Deferred {
        required_at_least: usize,
        allowed: usize,
    },
}

#[derive(Clone, Debug)]
pub struct ConversationItem {
    pub id: ConversationItemId,
    pub body: ConversationItemBody,
    pub work: Option<ConversationWorkMember>,
    pub user_turn: Option<ConversationCutoff>,
    pub tool_artifacts: Vec<ToolResultArtifact>,
}

/// Original turn membership for a journal record in the selected conversation.
#[derive(Clone, Debug)]
pub struct ConversationWorkMember {
    pub lease: atman_rt::TurnLease<AtmanUuid>,
    pub role: ConversationWorkRole,
    pub publication: Option<Uuid>,
    pub publication_run: Option<FlowRunId>,
}

#[derive(Clone, Copy, Debug)]
pub enum ConversationWorkRole {
    Invocation,
    Work,
    FinalAnswer,
}

impl ConversationWorkMember {
    pub(in crate::flow_store) fn from_fact(fact: &FlowFact) -> Option<Self> {
        match fact {
            FlowFact::InputCommitted(input) => Some(Self {
                lease: input.lease.clone(),
                role: if matches!(&input.content, InputContent::InvocationInput { .. }) {
                    ConversationWorkRole::Invocation
                } else {
                    ConversationWorkRole::Work
                },
                publication: None,
                publication_run: None,
            }),
            FlowFact::MessagePublished(value) if value.context_run.is_none() => Some(Self {
                lease: value.lease.clone(),
                role: if value.message.role == MessageRole::Assistant
                    && value.message.origin == MessageOrigin::FinalAnswer
                {
                    ConversationWorkRole::FinalAnswer
                } else {
                    ConversationWorkRole::Work
                },
                publication: Some(value.id),
                publication_run: Some(value.run.clone()),
            }),
            _ => None,
        }
    }
}

pub struct RecentConversationPage {
    pub boundary: ConversationBoundary,
    pub items: Vec<ConversationItem>,
    pub before: Option<ConversationItemId>,
    pub has_earlier: bool,
    pub source_bytes: usize,
    pub inspected_metadata: usize,
}

pub enum ConversationPageStep {
    Loading,
    Capacity(ConversationCapacity),
    Ready(RecentConversationPage),
}

pub struct ConversationPagePreparation {
    identity: Arc<()>,
    source: ConversationBoundary,
    receipts: Vec<CommitReceipt>,
    next: usize,
    request: RecentConversationRequest,
    has_earlier_batches: bool,
    inspected_metadata: usize,
    before_checked: bool,
    selected: Vec<ConversationItem>,
    inline_bytes: usize,
    source_bytes: usize,
    control: SourceCaptureControl,
    finished: bool,
    extra_item: bool,
}

fn missing_record() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "part is not in the selected conversation record",
    )
}

impl PreparedConversationSource {
    pub fn boundary(&self) -> &ConversationBoundary {
        &self.boundary
    }

    pub(crate) fn storage_is_current(&self) -> bool {
        self.stamp.as_ref().is_some_and(|stamp| {
            ReadStamp::open(&self.directory.join("events.jsonl"))
                .is_ok_and(|current| current == *stamp)
        })
    }

    pub(crate) fn read_artifact_record(
        &mut self,
        item: ConversationItemId,
        part: u32,
        id: &str,
    ) -> io::Result<Result<ToolResultArtifact, ConversationCapacity>> {
        if item.message != 0 {
            return Err(missing_record());
        }
        let facts = match self.read_visible_batch(item)? {
            Ok(facts) => facts,
            Err(capacity) => return Ok(Err(capacity)),
        };
        let Some(FlowFact::MessagePublished(publication)) = facts.get(item.record.fact) else {
            return Err(missing_record());
        };
        if publication.context_run.is_some() || publication.lease.flow != self.boundary.source.flow
        {
            return Err(missing_record());
        }
        let artifact = publication
            .tool_artifacts
            .iter()
            .find(|artifact| artifact.part == part && artifact.output.id == id)
            .filter(|artifact| artifact.matches_message(&publication.message));
        Ok(Ok(artifact.cloned().ok_or_else(missing_record)?))
    }

    pub(crate) fn read_image_record(
        &mut self,
        item: ConversationItemId,
        part: u32,
    ) -> io::Result<Result<ImageSource, ConversationCapacity>> {
        let facts = match self.read_visible_batch(item)? {
            Ok(facts) => facts,
            Err(capacity) => return Ok(Err(capacity)),
        };
        let facts = super::super::images::resolve_owned(&self.directory, facts)?;
        let message = facts
            .get(item.record.fact)
            .and_then(|fact| fact.shared_messages().get(item.message))
            .filter(|message| message.role != MessageRole::System)
            .ok_or_else(missing_record)?;
        let Some(MessagePart::Image { source }) = message.parts.get(part as usize) else {
            return Err(missing_record());
        };
        Ok(Ok(source.clone()))
    }

    fn read_visible_batch(
        &mut self,
        item: ConversationItemId,
    ) -> io::Result<Result<Vec<FlowFact>, ConversationCapacity>> {
        let sequence = item.record.commit;
        if item.record.item != 0
            || sequence == 0
            || sequence > self.boundary.source.cursor.sequence
            || !self.visible(sequence)
        {
            return Err(missing_record());
        }
        let receipt = self.reader.receipt_at(sequence)?;
        let bytes = self.reader.encoded_batch_bytes(&receipt)?;
        let allowed = self.limits.source_record_bytes.get();
        if bytes > allowed {
            return Ok(Err(ConversationCapacity {
                kind: ConversationCapacityKind::SourceRecord,
                required_at_least: bytes,
                allowed,
                restart_required: false,
            }));
        }
        let batch = self.reader.read_batch::<FlowFact>(&receipt)?;
        FlowFact::validate_batch(&batch.facts)?;
        Ok(Ok(batch.facts))
    }

    pub(crate) fn artifact_store(&self) -> crate::tools::tool_output::OutputStore {
        crate::tools::tool_output::OutputStore::at(self.directory.clone())
    }

    /// Raise page allowances only after a new explicit host admission.
    pub fn admit_limits(&mut self, limits: RecentConversationLimits) -> io::Result<()> {
        limits.admit(self.limits)?;
        self.limits = limits;
        Ok(())
    }

    /// Retained allocation charge for a host observation cache; no execution owner is retained.
    pub fn retained_metadata_bytes(&self) -> usize {
        self.reader
            .retained_index_bytes()
            .saturating_add(self.markers.retained_bytes())
            .saturating_add(
                self.spans
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ConversationSpan>()),
            )
            .saturating_add(history_bytes(&self.history))
    }

    pub fn start_page(
        &self,
        request: RecentConversationRequest,
        control: SourceCaptureControl,
    ) -> io::Result<Result<ConversationPagePreparation, ConversationCapacity>> {
        request.validate()?;
        cancelled(&self.control)?;
        cancelled(&control)?;
        if request.flow != self.boundary.source.flow
            || request
                .expected
                .as_ref()
                .is_some_and(|source| source != &self.boundary.source)
            || request
                .expected_head
                .as_ref()
                .is_some_and(|head| head != &self.boundary.observed_head)
        {
            return Err(invalid(
                "conversation page does not belong to this retained source",
            ));
        }
        if let Some(before) = request.before {
            if before.record.item != 0
                || before.record.commit == 0
                || before.record.commit > self.boundary.source.cursor.sequence
                || !self.visible(before.record.commit)
            {
                return Err(invalid(
                    "conversation continuation is outside this selected source",
                ));
            }
            let shared = self.markers.activation == Some(before.record.commit)
                || [&self.markers.inputs, &self.markers.publications]
                    .into_iter()
                    .any(|markers| {
                        markers
                            .binary_search_by_key(&before.record.commit, |marker| marker.sequence)
                            .ok()
                            .is_some_and(|index| markers[index].shared)
                    });
            if !shared {
                return Err(invalid(
                    "conversation continuation is not a shared conversation record",
                ));
            }
        }
        let maximum = NonZeroUsize::new(request.limit.get() + 2).expect("nonzero page limit");
        let through = request
            .before
            .map_or(self.boundary.source.cursor.sequence, |before| {
                before.record.commit
            });
        let inputs = &self.markers.inputs[..self
            .markers
            .inputs
            .partition_point(|marker| marker.sequence <= through)];
        let publications = &self.markers.publications[..self
            .markers
            .publications
            .partition_point(|marker| marker.sequence <= through)];
        let selection = match ConversationPlan::select_recent(
            self.markers.activation,
            inputs.iter().map(|marker| (marker.sequence, marker.shared)),
            publications
                .iter()
                .map(|marker| (marker.sequence, marker.shared)),
            |sequence| {
                self.visible(sequence)
                    && request
                        .before
                        .is_none_or(|before| sequence <= before.record.commit)
            },
            maximum,
            self.limits.selection_work,
        ) {
            Ok(selection) => selection,
            Err(capacity) => {
                return Ok(Err(ConversationCapacity {
                    kind: ConversationCapacityKind::SelectionWork,
                    required_at_least: capacity.required_at_least,
                    allowed: capacity.allowed,
                    restart_required: false,
                }));
            }
        };
        let inspected_metadata = selection.inspected_metadata;
        let has_earlier_batches = selection.has_earlier_batches;
        let plan =
            selection.resolve(|sequence| self.reader.receipt_at(sequence).map(Into::into))?;
        let receipts = plan
            .positions()
            .into_iter()
            .map(|position| position.require_durable().cloned())
            .collect::<io::Result<Vec<_>>>()?;
        let next = receipts.len();
        Ok(Ok(ConversationPagePreparation {
            identity: self.identity.clone(),
            source: self.boundary.clone(),
            receipts,
            next,
            before_checked: request.before.is_none(),
            request,
            has_earlier_batches,
            inspected_metadata,
            selected: Vec::new(),
            inline_bytes: 0,
            source_bytes: 0,
            control,
            finished: false,
            extra_item: false,
        }))
    }

    fn visible(&self, sequence: u64) -> bool {
        self.spans
            .get(
                self.spans
                    .partition_point(|span| span.last_sequence < sequence),
            )
            .is_some_and(|span| span.first_sequence <= sequence)
    }
}

impl ConversationPagePreparation {
    /// Decode at most one admitted original batch per worker invocation.
    pub fn step(
        &mut self,
        source: &mut PreparedConversationSource,
        budget: InspectionBudget,
    ) -> io::Result<ConversationPageStep> {
        if self.finished {
            return Err(invalid("conversation page preparation is retired"));
        }
        let result = self.step_active(source, budget);
        if result.is_err() {
            self.finished = true;
            self.selected.clear();
        }
        result
    }

    fn step_active(
        &mut self,
        source: &mut PreparedConversationSource,
        budget: InspectionBudget,
    ) -> io::Result<ConversationPageStep> {
        cancelled(&self.control)?;
        cancelled(&source.control)?;
        if !Arc::ptr_eq(&self.identity, &source.identity) {
            return Err(invalid("conversation page source changed"));
        }
        if self.next == 0 || self.extra_item {
            return self.finish();
        }
        let receipt = &self.receipts[self.next - 1];
        let bytes = source.reader.encoded_batch_bytes(receipt)?;
        let record_limit = source
            .limits
            .source_record_bytes
            .get()
            .min(budget.max_record_bytes.get());
        let capacity = if bytes > record_limit {
            Some((ConversationCapacityKind::SourceRecord, bytes, record_limit))
        } else if bytes.saturating_mul(2) > budget.work_bytes.get() {
            Some((
                ConversationCapacityKind::DecodeWork,
                bytes.saturating_mul(2),
                budget.work_bytes.get(),
            ))
        } else if self.source_bytes.saturating_add(bytes) > source.limits.page_source_bytes.get() {
            if !self.selected.is_empty() && self.before_checked {
                return self.finish();
            }
            Some((
                ConversationCapacityKind::PageSourceBytes,
                self.source_bytes.saturating_add(bytes),
                source.limits.page_source_bytes.get(),
            ))
        } else {
            None
        };
        if let Some((kind, required_at_least, allowed)) = capacity {
            return Ok(ConversationPageStep::Capacity(ConversationCapacity {
                kind,
                required_at_least,
                allowed,
                restart_required: false,
            }));
        }
        let batch = source.reader.read_batch::<FlowFact>(receipt)?;
        FlowFact::validate_batch(&batch.facts)?;
        let facts = super::super::images::resolve_owned(&source.directory, batch.facts)?;
        cancelled(&self.control)?;
        cancelled(&source.control)?;
        self.source_bytes += bytes;
        for (fact_index, fact) in facts.iter().enumerate().rev() {
            let messages = fact.shared_messages();
            let end = if let Some(before) = self.request.before.filter(|before| {
                before.record.commit == receipt.sequence && before.record.fact == fact_index
            }) {
                if before.message >= messages.len() {
                    return Err(invalid(
                        "conversation continuation message is absent from its original record",
                    ));
                }
                self.before_checked = true;
                before.message
            } else if self.request.before.is_some_and(|before| {
                before.record.commit == receipt.sequence && fact_index > before.record.fact
            }) {
                continue;
            } else {
                messages.len()
            };
            for (message_index, message) in messages[..end].iter().enumerate().rev() {
                if self.selected.len() == self.request.limit.get() {
                    self.extra_item = true;
                    break;
                }
                let id = ConversationItemId {
                    record: FlowRecordId {
                        commit: receipt.sequence,
                        fact: fact_index,
                        item: 0,
                    },
                    message: message_index,
                };
                let allowed = source
                    .limits
                    .inline_bytes
                    .get()
                    .saturating_sub(self.inline_bytes);
                // Image bytes are admitted with the original batch and fetched by part identity.
                let image_bytes = message
                    .parts
                    .iter()
                    .fold(0_usize, |total, part| match part {
                        MessagePart::Image {
                            source:
                                ImageSource {
                                    data: ImageData::Base64 { data },
                                    ..
                                },
                        } => total.saturating_add(data.len()),
                        _ => total,
                    });
                let mut counter = LimitedCounter {
                    bytes: 0,
                    maximum: allowed.saturating_add(image_bytes),
                };
                let encoded = serde_json::to_writer(&mut counter, message);
                let projected_bytes = counter.bytes.saturating_sub(image_bytes);
                let body = match encoded {
                    Ok(()) => {
                        self.inline_bytes += projected_bytes;
                        ConversationItemBody::Inline(message.clone())
                    }
                    Err(_) if counter.bytes > counter.maximum => ConversationItemBody::Deferred {
                        required_at_least: projected_bytes,
                        allowed,
                    },
                    Err(error) => return Err(io::Error::other(error)),
                };
                self.selected.push(ConversationItem {
                    id,
                    body,
                    work: ConversationWorkMember::from_fact(fact),
                    user_turn: (message.role == MessageRole::User)
                        .then(|| source.markers.cutoff(receipt, fact_index, fact))
                        .flatten(),
                    tool_artifacts: match fact {
                        FlowFact::MessagePublished(publication)
                            if publication.lease.flow == source.boundary.source.flow
                                && publication.context_run.is_none() =>
                        {
                            publication
                                .tool_artifacts
                                .iter()
                                .filter(|artifact| artifact.matches_message(message))
                                .cloned()
                                .collect()
                        }
                        _ => Vec::new(),
                    },
                });
            }
        }
        if self
            .request
            .before
            .is_some_and(|before| before.record.commit == receipt.sequence)
            && !self.before_checked
        {
            return Err(invalid(
                "conversation continuation fact is absent from its original record",
            ));
        }
        self.next -= 1;
        if self.next == 0 || self.extra_item {
            self.finish()
        } else {
            Ok(ConversationPageStep::Loading)
        }
    }

    fn finish(&mut self) -> io::Result<ConversationPageStep> {
        if !self.before_checked {
            return Err(invalid("conversation continuation was not validated"));
        }
        self.finished = true;
        let has_earlier = self.extra_item || self.next > 0 || self.has_earlier_batches;
        self.selected.truncate(self.request.limit.get());
        self.selected.reverse();
        let before = has_earlier
            .then(|| self.selected.first().map(|item| item.id))
            .flatten();
        Ok(ConversationPageStep::Ready(RecentConversationPage {
            boundary: self.source.clone(),
            items: std::mem::take(&mut self.selected),
            before,
            has_earlier,
            source_bytes: self.source_bytes,
            inspected_metadata: self.inspected_metadata,
        }))
    }
}

struct LimitedCounter {
    bytes: usize,
    maximum: usize,
}
impl Write for LimitedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > self.maximum {
            Err(io::Error::other("conversation inline body capacity"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cancelled(control: &SourceCaptureControl) -> io::Result<()> {
    if control.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "conversation observation cancelled",
        ))
    } else {
        Ok(())
    }
}

fn history_bytes(history: &ConversationHistory) -> usize {
    let receipt = |receipt: &CommitReceipt| receipt.digest.capacity();
    let parent = |parent: &SourceSelection| receipt(&parent.cursor);
    let branches = history
        .branches
        .capacity()
        .saturating_mul(std::mem::size_of::<ConversationBranch>())
        .saturating_add(
            history
                .branches
                .iter()
                .map(|branch| receipt(&branch.created) + branch.parent.as_ref().map_or(0, parent))
                .sum::<usize>(),
        );
    let changes = history
        .changes
        .capacity()
        .saturating_mul(std::mem::size_of::<CommittedHistoryHeadChange>())
        .saturating_add(
            history
                .changes
                .iter()
                .map(|change| {
                    receipt(&change.receipt)
                        + match &change.change.selection {
                            HistorySelection::Create { parent: source, .. } => parent(source),
                            HistorySelection::Restore { .. } => 0,
                        }
                })
                .sum::<usize>(),
        );
    branches
        .saturating_add(changes)
        .saturating_add(receipt(&history.observed))
}

fn reserve_markers(
    markers: &mut Vec<ConversationMarker>,
    additional: usize,
    maximum: usize,
) -> io::Result<()> {
    let required = markers.len().saturating_add(additional);
    if required > markers.capacity() {
        let target = required
            .max(markers.capacity().saturating_mul(2))
            .min(maximum);
        markers
            .try_reserve_exact(target - markers.len())
            .map_err(io::Error::other)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
