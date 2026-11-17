//! Bounded source observations captured at a durable Flow owner's boundary.

use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowRef, FlowStore, inbox::InputContent, invalid};
use crate::{
    event_log::journal::{CapturedJournal, CommitReceipt},
    message::{ImageData, Message, MessagePart},
    tools::tool_output::OutputStore,
};

/// Cancellation handle for an uncommitted source snapshot. Cancelling stops active bounded
/// copies at their next chunk boundary and leaves no destination publication behind.
#[derive(Clone, Default)]
pub struct SourceCaptureControl {
    cancelled: Arc<AtomicBool>,
}

impl SourceCaptureControl {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub(crate) fn copy_control(&self, staging_root: PathBuf) -> crate::retained_file::CopyControl {
        crate::retained_file::CopyControl::with_cancelled(staging_root, self.cancelled.clone())
    }
}

mod assets;
mod full_read;
mod reference_record;
pub use assets::{RetainedBashLog, RetainedSourceAssets};
pub use reference_record::PreparedReference;
mod boundary;
mod fork_boundary;
mod fork_import;
mod fork_read;
mod fork_snapshot;
pub(super) use boundary::BoundarySource;
pub use boundary::{
    BoundaryReferenceRequest, BoundaryReferenceSnapshot, ConversationCutoff,
    ResolvedConversationBoundary,
};
pub use fork_boundary::{ForkBoundaryRequest, ForkCutoff, ResolvedForkBoundary};
pub use fork_import::{
    ForkImportActivation, ForkImportManifest, ForkImportPlan, ForkImportPublication,
    ForkImportReceipt, ForkImportState,
};
pub use fork_read::{ForkBaseline, ForkConversation, ForkConversationRead, ForkReadBudget};
pub use fork_snapshot::{ForkSnapshot, RetainedForkSnapshot};

/// Identity of a retained conversation branch, independent of later source appends.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ConversationBranchId(Uuid);

impl From<Uuid> for ConversationBranchId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

impl ConversationBranchId {
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    pub fn random() -> Self {
        Self(Uuid::now_v7())
    }

    pub(crate) fn is_nil(self) -> bool {
        self.0.is_nil()
    }

    pub fn initial(flow: &FlowRef, journal: Uuid) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"atman.initial-conversation-branch\0");
        hash.update(flow.session_id.0.as_bytes());
        hash.update(flow.instance_id.0.0.as_bytes());
        hash.update(journal.as_bytes());
        Self(Uuid::from_bytes(
            hash.finalize().as_bytes()[..16]
                .try_into()
                .expect("digest prefix has sixteen bytes"),
        ))
    }
}

/// Complete durable observation identity; this is not an execution grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSelection {
    pub flow: FlowRef,
    pub branch: ConversationBranchId,
    pub cursor: CommitReceipt,
}

/// Limits apply before reading source batches and independently to retained artifacts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceBudget {
    pub max_messages: usize,
    pub max_context_bytes: usize,
    pub max_source_batches: usize,
    pub max_source_bytes: usize,
    pub max_artifact_bytes: usize,
}

impl Default for ReferenceBudget {
    fn default() -> Self {
        Self {
            max_messages: 128,
            max_context_bytes: 256 * 1024,
            max_source_batches: 256,
            max_source_bytes: 16 * 1024 * 1024,
            max_artifact_bytes: 16 * 1024 * 1024,
        }
    }
}

impl ReferenceBudget {
    pub(crate) fn validate(self) -> io::Result<()> {
        if self.max_messages == 0
            || self.max_messages > 1024
            || self.max_context_bytes == 0
            || self.max_context_bytes > 4 * 1024 * 1024
            || self.max_source_batches == 0
            || self.max_source_batches > 2048
            || self.max_source_bytes == 0
            || self.max_source_bytes > 64 * 1024 * 1024
            || self.max_artifact_bytes > 128 * 1024 * 1024
        {
            return Err(invalid("invalid reference capture budget"));
        }
        Ok(())
    }
}

/// A reference capture may select an existing durable cursor or capture the current one.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReferenceRequest {
    pub flow: FlowRef,
    pub branch: Option<ConversationBranchId>,
    pub at: Option<CommitReceipt>,
    pub budget: ReferenceBudget,
}

impl ReferenceRequest {
    pub fn current(flow: FlowRef) -> Self {
        Self {
            flow,
            branch: None,
            at: None,
            budget: ReferenceBudget::default(),
        }
    }
}

/// Source directory/configuration and current catalog membership are separate observations.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapturedOrigin {
    pub catalog: CommitReceipt,
    pub session_project: Option<crate::storage::ProjectContext>,
    pub flow_project: Option<crate::storage::ProjectContext>,
    pub configuration: super::SessionConfiguration,
    pub execution: Option<super::launch::EntryBinding>,
}

/// Explicit omissions; unavailable references never acquire filesystem fallback access.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceOmissions {
    pub earlier_context: bool,
    pub incomplete_tool_parts: usize,
    pub unavailable_images: usize,
    pub unavailable_outputs: usize,
}

/// Independent read-only reference data. Images are inline and outputs have no source directory.
#[derive(Clone)]
pub struct ReferenceSnapshot {
    pub messages: Arc<[Message]>,
    pub outputs: OutputStore,
    pub omissions: ReferenceOmissions,
}

pub struct SourceCapture<Product> {
    pub source: SourceSelection,
    pub origin: CapturedOrigin,
    pub product: Product,
}

pub(crate) struct ReferencePlan {
    source: SourceSelection,
    directory: PathBuf,
    journal: CapturedJournal,
    budget: ReferenceBudget,
    truncated: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum FullReadKind {
    Context,
    Output,
}

pub(crate) struct FullReadPlan {
    pub source: SourceSelection,
    context: Option<super::conversation::ConversationPlan>,
    publications: Vec<super::publication::PublicationRef>,
    receipts: Vec<CommitReceipt>,
    journal: CapturedJournal,
    directory: PathBuf,
    max_assets: usize,
}

impl FlowStore {
    /// Freeze only read metadata at one branch/cursor; parse bodies on the caller's read worker.
    pub(crate) fn capture_full_read_plan(
        &self,
        kind: FullReadKind,
        maximum: usize,
    ) -> io::Result<FullReadPlan> {
        let cursor = self.cursor().require_durable()?.clone();
        let branch = self.history_head().branch;
        let (context, publications) = match kind {
            FullReadKind::Context => (
                Some(self.conversation_plan_at(
                    super::conversation::ConversationReadKind::ModelContext,
                    branch,
                    cursor.sequence,
                )?),
                Vec::new(),
            ),
            FullReadKind::Output => (
                None,
                self.published_messages()
                    .iter()
                    .filter(|record| record.is_shared_final())
                    .cloned()
                    .collect(),
            ),
        };
        let positions = context.as_ref().map_or_else(
            || {
                publications
                    .iter()
                    .map(|record: &super::publication::PublicationRef| record.receipt.clone())
                    .collect()
            },
            super::conversation::ConversationPlan::positions,
        );
        let mut receipts = positions
            .iter()
            .map(|position| position.require_durable().cloned())
            .collect::<io::Result<Vec<_>>>()?;
        receipts.sort_unstable_by_key(|receipt| receipt.sequence);
        receipts.dedup();
        let journal = self
            .journal
            .capture_durable_batches(&cursor, &receipts, maximum)?;
        if journal.truncated || journal.len() != receipts.len() {
            return Err(invalid("conversation snapshot exceeds its read capacity"));
        }
        Ok(FullReadPlan {
            source: SourceSelection {
                flow: self.state().address().clone(),
                branch,
                cursor,
            },
            context,
            publications,
            receipts,
            journal,
            directory: self.require_directory()?.to_path_buf(),
            max_assets: maximum,
        })
    }
}

impl FullReadPlan {
    pub(crate) fn prepare(
        self,
    ) -> io::Result<(
        SourceSelection,
        Vec<Message>,
        Option<Vec<super::publication::PublishedMessage>>,
    )> {
        let read = |position: &super::position::FlowPosition| {
            let receipt = position.require_durable()?;
            let index = self
                .receipts
                .binary_search_by_key(&receipt.sequence, |receipt| receipt.sequence)
                .map_err(|_| invalid("snapshot receipt is not captured"))?;
            if &self.receipts[index] != receipt {
                return Err(invalid("snapshot receipt identity changed"));
            }
            let batch = self.journal.read::<FlowFact>(index)?;
            super::images::resolve(&self.directory, &batch.facts)
        };
        let mut remaining = self.max_assets;
        let mut inline =
            |source: &mut crate::message::ImageSource| inline_full_image(source, &mut remaining);
        match self.context {
            Some(context) => {
                let mut messages = context.read(read)?;
                super::images::messages(&mut messages, &mut inline)?;
                Ok((self.source, messages, None))
            }
            None => {
                let mut output = self
                    .publications
                    .into_iter()
                    .map(|publication| {
                        read(&publication.receipt)?
                            .into_iter()
                            .find_map(|fact| match fact {
                                FlowFact::MessagePublished(message)
                                    if message.id == publication.id =>
                                {
                                    Some(super::publication::PublishedMessage {
                                        publication: *message,
                                        receipt: publication.receipt.clone(),
                                    })
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                invalid("published output is absent from its captured batch")
                            })
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                for publication in &mut output {
                    super::images::messages(
                        std::slice::from_mut(&mut publication.publication.message),
                        &mut inline,
                    )?;
                }
                Ok((self.source, Vec::new(), Some(output)))
            }
        }
    }
}

pub(super) fn inline_full_image(
    source: &mut crate::message::ImageSource,
    remaining: &mut usize,
) -> io::Result<()> {
    let bytes =
        crate::attachment_store::retained_image_bytes(source, *remaining).map_err(invalid)?;
    *remaining -= bytes.len();
    source.data = ImageData::Base64 {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    Ok(())
}

fn reverse_ancestry<'a, T>(
    records: &'a [T],
    spans: &'a [super::branches::ConversationSpan],
    after: u64,
    sequence: impl Fn(&T) -> u64 + Copy + 'a,
) -> impl Iterator<Item = &'a T> {
    spans.iter().rev().flat_map(move |span| {
        let end = records.partition_point(|record| sequence(record) <= span.last_sequence);
        let start = records[..end].partition_point(|record| {
            sequence(record) < span.first_sequence || sequence(record) <= after
        });
        records[start..end].iter().rev()
    })
}

impl FlowStore {
    pub(crate) fn source_capture_origin(
        &self,
        catalog: CommitReceipt,
        session_project: Option<crate::storage::ProjectContext>,
    ) -> CapturedOrigin {
        CapturedOrigin {
            catalog,
            session_project,
            flow_project: self.project().cloned(),
            configuration: self.session_descriptor().configuration.clone(),
            execution: self
                .activation
                .as_ref()
                .and_then(|(activation, _)| activation.input.execution.clone()),
        }
    }

    /// Capture compact positions only; payload parsing and retention happen on read workers.
    pub(crate) fn capture_reference_plan(
        &self,
        request: &ReferenceRequest,
    ) -> io::Result<ReferencePlan> {
        request.budget.validate()?;
        if request.flow != self.descriptor.address
            || !matches!(self.descriptor.content, super::FlowContent::Entry(_))
        {
            return Err(invalid(
                "reference capture requires the selected native Flow",
            ));
        }
        let current = self.cursor().require_durable()?;
        let cursor = request.at.as_ref().unwrap_or(current).clone();
        let branch = self.history_head().branch;
        if request.branch.is_some_and(|expected| expected != branch) {
            return Err(invalid("source conversation branch changed"));
        }
        let history = &self
            .branches
            .as_ref()
            .ok_or_else(|| invalid("reference capture requires native conversation ancestry"))?
            .history;
        if !history.contains(branch, cursor.sequence)? {
            return Err(invalid("reference cutoff is outside the selected ancestry"));
        }
        let spans = history.ancestry_through(branch, cursor.sequence)?;
        let checkpoints = self.context_checkpoints();
        let checkpoint =
            reverse_ancestry(checkpoints, &spans, 0, |record| record.receipt.sequence())
                .next()
                .map(|record| &record.receipt);
        let base = checkpoint.or_else(|| {
            self.activation
                .as_ref()
                .map(|(_, receipt)| receipt)
                .filter(|receipt| receipt.sequence() <= cursor.sequence)
        });
        let after = base.map_or(0, |receipt| receipt.sequence());
        let cap = request.budget.max_source_batches;
        let inputs = self.committed_inputs();
        let publications = self.published_messages();
        // Bound inspected metadata as well as selected payload. Isolated output cannot force a scan.
        let mut truncated = false;
        let mut receipts = Vec::new();
        for (index, record) in
            reverse_ancestry(inputs, &spans, after, |record| record.receipt.sequence())
                .take(cap + 1)
                .enumerate()
        {
            if index == cap {
                truncated = true;
                break;
            }
            if record.message_count > 0 {
                receipts.push(record.receipt.require_durable()?.clone());
            }
        }
        for (index, record) in reverse_ancestry(publications, &spans, after, |record| {
            record.receipt.sequence()
        })
        .take(cap + 1)
        .enumerate()
        {
            if index == cap {
                truncated = true;
                break;
            }
            if record.context_run.is_none() {
                receipts.push(record.receipt.require_durable()?.clone());
            }
        }
        receipts.sort_unstable_by_key(|receipt| std::cmp::Reverse(receipt.sequence));
        receipts.dedup();
        if receipts.len() > cap {
            truncated = true;
            receipts.truncate(cap);
        }
        if !truncated && let Some(base) = base {
            if receipts.len() == cap {
                truncated = true;
            } else {
                receipts.push(base.require_durable()?.clone());
            }
        }
        let journal = self.journal.capture_durable_batches(
            &cursor,
            &receipts,
            request.budget.max_source_bytes,
        )?;
        truncated |= journal.truncated;
        Ok(ReferencePlan {
            source: SourceSelection {
                flow: request.flow.clone(),
                branch,
                cursor,
            },
            directory: self.require_directory()?.to_path_buf(),
            journal,
            budget: request.budget,
            truncated,
        })
    }
}

impl ReferencePlan {
    pub(crate) fn prepare(self) -> io::Result<(SourceSelection, ReferenceSnapshot)> {
        let mut reverse = Vec::new();
        let mut bytes = 0;
        let mut omissions = ReferenceOmissions {
            earlier_context: self.truncated,
            ..Default::default()
        };
        'batches: for index in 0..self.journal.len() {
            let batch = self.journal.read::<FlowFact>(index)?;
            let facts = super::images::resolve(&self.directory, &batch.facts)?;
            let mut messages = Vec::new();
            for fact in facts {
                match fact {
                    FlowFact::MessagePublished(record) if record.context_run.is_none() => {
                        messages.push(record.message);
                    }
                    FlowFact::InputCommitted(record) => match record.content {
                        InputContent::Context { messages: content }
                        | InputContent::InvocationInput {
                            messages: content, ..
                        } => {
                            messages.extend(content);
                        }
                        InputContent::WaitingContinuation { .. } => {}
                    },
                    FlowFact::ContextCheckpoint(record) => messages.extend(record.messages),
                    FlowFact::Activated(record) => messages.extend(record.input.inherited_context),
                    _ => {}
                }
            }
            for message in messages.into_iter().rev() {
                let length = serde_json::to_vec(&message).map_err(invalid)?.len();
                if reverse.len() == self.budget.max_messages
                    || length > self.budget.max_context_bytes.saturating_sub(bytes)
                {
                    omissions.earlier_context = true;
                    break 'batches;
                }
                bytes += length;
                reverse.push(message);
            }
        }
        reverse.reverse();
        super::publication::trim_context(&mut reverse);
        let part_count = |messages: &[Message]| {
            messages
                .iter()
                .flat_map(|message| &message.parts)
                .filter(|part| {
                    matches!(
                        part,
                        MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
                    )
                })
                .count()
        };
        let before = part_count(&reverse);
        crate::message::retain_complete_tool_pairs(&mut reverse);
        omissions.incomplete_tool_parts = before - part_count(&reverse);
        let mut remaining = self.budget.max_artifact_bytes;
        let mut retained = BTreeMap::new();
        let outputs = OutputStore::at(&self.directory);
        for message in &mut reverse {
            message.parts.retain_mut(|part| {
                match part {
                    MessagePart::Image { source } => {
                        return retain_image(source, &mut remaining, &mut omissions);
                    }
                    MessagePart::ContextRecord(record) => {
                        record.retain_images(|source| {
                            retain_image(source, &mut remaining, &mut omissions)
                        });
                    }
                    MessagePart::ToolResult { content, .. } => {
                        if let Some((id, expected_bytes)) =
                            crate::tools::tool_output::reference_identity(content)
                            && !retained.contains_key(&id)
                        {
                            if expected_bytes <= remaining {
                                match outputs.retain_reference(content) {
                                    Ok(Some((id, full))) => {
                                        remaining -= full.len();
                                        retained.insert(id, full);
                                    }
                                    _ => omissions.unavailable_outputs += 1,
                                }
                            } else {
                                omissions.unavailable_outputs += 1;
                            }
                        }
                    }
                    _ => {}
                }
                true
            });
        }
        reverse.retain(|message| !message.parts.is_empty());
        Ok((
            self.source,
            ReferenceSnapshot {
                messages: reverse.into(),
                outputs: OutputStore::retained(retained).map_err(invalid)?,
                omissions,
            },
        ))
    }
}

fn retain_image(
    source: &mut crate::message::ImageSource,
    remaining: &mut usize,
    omissions: &mut ReferenceOmissions,
) -> bool {
    if *remaining > 0
        && let Ok(bytes) = crate::attachment_store::retained_image_bytes(source, *remaining)
    {
        *remaining -= bytes.len();
        source.data = ImageData::Base64 {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        true
    } else {
        omissions.unavailable_images += 1;
        false
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::{
        event::AtmanUuid,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowTransition, RuntimeStore,
            SessionDescriptor,
            context::{ContextCheckpoint, context_digest},
            publication::MessagePublication,
        },
        message::{MessageOrigin, MessageRole},
        source_program::RevisionBundle,
        tools::tool_output::{ToolOutputBudget, truncate_tool_result_content_with_budget},
    };
    use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnLease};

    pub(in crate::flow_store) fn prepared_fixture() -> (tempfile::TempDir, FlowStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let id = Uuid::new_v4();
        let mut session = root
            .create_session(SessionDescriptor {
                configuration: Default::default(),
                id,
                project: None,
                title: "Source".into(),
            })
            .unwrap();
        let flow = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::new_v4(),
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "/missing/project/main.at".into(),
                    bundle: RevisionBundle {
                        entry: "entry:main.at".into(),
                        sources: BTreeMap::from([(
                            "entry:main.at".into(),
                            "flow main() -> int { return 7 }".into(),
                        )]),
                        edges: BTreeMap::new(),
                    },
                }
                .into(),
                created_by: None,
            })
            .unwrap();
        (dir, flow)
    }

    pub(in crate::flow_store) fn fixture() -> (tempfile::TempDir, FlowStore, TurnLease<AtmanUuid>) {
        let (dir, mut flow) = prepared_fixture();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
        flow.transition(
            flow.state().revision(),
            FlowTransition::TurnStarted {
                turn: TurnId::now(),
                run: RunId::now(),
            },
        )
        .unwrap();
        let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
            panic!("fixture must have an active source turn");
        };
        let lease = active.lease.clone();
        (dir, flow, lease)
    }

    pub(in crate::flow_store) fn publish(
        flow: &mut FlowStore,
        lease: &TurnLease<AtmanUuid>,
        message: Message,
    ) {
        flow.publish_message(MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message,
        })
        .unwrap();
    }

    pub(in crate::flow_store) fn checkpoint(
        flow: &mut FlowStore,
        lease: &TurnLease<AtmanUuid>,
        text: &str,
    ) {
        let messages = flow.context_messages().unwrap();
        flow.checkpoint_context(ContextCheckpoint {
            expected_head: Some(flow.history_head()),
            purpose: Default::default(),
            id: Uuid::new_v4(),
            expected: context_digest(&messages).unwrap(),
            messages: vec![Message::user_text(lease.turn.clone(), text)],
            summary: text.into(),
            range: 0..messages.len(),
            count: messages.len(),
            before_tokens: 100,
            after_tokens: 10,
        })
        .unwrap();
    }

    pub(super) fn texts(snapshot: &ReferenceSnapshot) -> Vec<String> {
        snapshot.messages.iter().map(Message::text_concat).collect()
    }

    #[test]
    fn full_read_plan_keeps_original_checkpoint_cursor_and_shared_context() {
        let (_dir, mut flow, lease) = fixture();
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "before checkpoint"),
        );
        checkpoint(&mut flow, &lease, "retained summary");
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "shared suffix"),
        );
        let private = RunId::now();
        flow.publish_message(MessagePublication {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: private.clone(),
            context_run: Some(private),
            message: Message::assistant_text(lease.turn.clone(), "private execution"),
        })
        .unwrap();
        let expected = flow.context_messages().unwrap();
        let cursor = flow.cursor().require_durable().unwrap().clone();
        let branch = flow.history_head().branch;
        let plan = flow
            .capture_full_read_plan(FullReadKind::Context, 1024 * 1024)
            .unwrap();
        checkpoint(&mut flow, &lease, "later summary");
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "later output"),
        );
        let (source, messages, output) = plan.prepare().unwrap();
        assert_eq!(source.cursor, cursor);
        assert_eq!(source.branch, branch);
        assert_eq!(messages, expected);
        assert!(output.is_none());
        assert!(
            messages
                .iter()
                .all(|message| !message.text_concat().contains("private execution"))
        );
        assert!(
            flow.capture_full_read_plan(FullReadKind::Context, 1)
                .is_err()
        );
    }

    #[test]
    fn full_live_and_cold_reads_inline_retained_images_without_recovering_the_source() {
        let (dir, mut flow, lease) = fixture();
        let bytes = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        ];
        let original = dir.path().join("original.png");
        std::fs::write(&original, bytes).unwrap();
        let mut image = crate::attachment_store::AttachmentStore::at(flow.dir().unwrap())
            .import_bytes(&bytes, Some("original.png"))
            .unwrap();
        image.data = ImageData::Path {
            path: original.clone(),
        };
        let mut message = Message::assistant_text(lease.turn.clone(), "final image");
        message.origin = MessageOrigin::FinalAnswer;
        message.parts.push(MessagePart::Image { source: image });
        publish(&mut flow, &lease, message);
        let context = flow
            .capture_full_read_plan(FullReadKind::Context, 1024 * 1024)
            .unwrap();
        let output = flow
            .capture_full_read_plan(FullReadKind::Output, 1024 * 1024)
            .unwrap();
        std::fs::remove_file(&original).unwrap();
        let (_, context, _) = context.prepare().unwrap();
        let (_, _, output) = output.prepare().unwrap();
        let output = output.unwrap();
        let observed = flow.cursor().clone();
        let catalog =
            RuntimeStore::inspect_session(dir.path(), flow.state().address().session_id.0)
                .unwrap()
                .journal
                .cursor
                .unwrap();
        let cold_context = RuntimeStore::capture_recorded_full_read(
            dir.path(),
            flow.state().address(),
            FullReadKind::Context,
            1024 * 1024,
            &catalog,
        )
        .unwrap();
        let cold_output = RuntimeStore::capture_recorded_full_read(
            dir.path(),
            flow.state().address(),
            FullReadKind::Output,
            1024 * 1024,
            &catalog,
        )
        .unwrap();
        assert_eq!(cold_context.messages, context);
        assert_eq!(cold_output.output.as_ref().unwrap(), &output);
        assert_eq!(flow.cursor(), &observed);
        assert!(
            matches!(flow.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == lease)
        );
        assert!(
            RuntimeStore::capture_recorded_full_read(
                dir.path(),
                flow.state().address(),
                FullReadKind::Context,
                1,
                &catalog
            )
            .is_err()
        );
        drop(flow);
        dir.close().unwrap();
        for message in context.iter().chain(
            output
                .iter()
                .map(|publication| &publication.publication.message),
        ) {
            let source = message
                .parts
                .iter()
                .find_map(|part| match part {
                    MessagePart::Image { source } => Some(source),
                    _ => None,
                })
                .unwrap();
            assert!(matches!(source.data, ImageData::Base64 { .. }));
            assert_eq!(crate::attachment_store::image_bytes(source).unwrap(), bytes);
        }
    }

    #[test]
    fn reference_capture_uses_only_the_checkpoint_and_shared_suffix_at_its_exact_cursor() {
        let (_dir, mut flow, lease) = fixture();
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "original"),
        );
        checkpoint(&mut flow, &lease, "old summary");
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "old suffix"),
        );
        let mut request = ReferenceRequest::current(flow.state().address().clone());
        request.at = Some(flow.cursor().require_durable().unwrap().clone());
        let plan = flow.capture_reference_plan(&request).unwrap();
        let branch = plan.source.branch;
        checkpoint(&mut flow, &lease, "later summary");
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "later suffix"),
        );
        let isolated = RunId::now();
        flow.publish_message(MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: isolated.clone(),
            context_run: Some(isolated),
            message: Message::assistant_text(lease.turn.clone(), "private invocation"),
        })
        .unwrap();
        let (source, fixed) = plan.prepare().unwrap();
        assert_eq!(Some(&source.cursor), request.at.as_ref());
        assert_eq!(texts(&fixed), ["old summary", "old suffix"]);
        let (_, same_boundary) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(same_boundary.messages, fixed.messages);
        let current = ReferenceRequest::current(request.flow.clone());
        let (current_source, latest) = flow
            .capture_reference_plan(&current)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(current_source.branch, branch);
        assert_eq!(texts(&latest), ["later summary", "later suffix"]);
        assert!(!latest.omissions.earlier_context);
        request.branch = Some(ConversationBranchId::initial(&request.flow, Uuid::new_v4()));
        assert!(flow.capture_reference_plan(&request).is_err());
        request.branch = None;
        request.at.as_mut().unwrap().digest = "incorrect digest".into();
        assert!(flow.capture_reference_plan(&request).is_err());
    }

    #[test]
    fn reference_capture_bounds_batch_metadata_messages_and_context_independently() {
        let (_dir, mut flow, lease) = fixture();
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "before checkpoint"),
        );
        checkpoint(&mut flow, &lease, "summary");
        for text in ["one", "two", "three"] {
            publish(
                &mut flow,
                &lease,
                Message::assistant_text(lease.turn.clone(), text),
            );
        }
        let mut request = ReferenceRequest::current(flow.state().address().clone());
        request.budget.max_source_batches = 3;
        let plan = flow.capture_reference_plan(&request).unwrap();
        assert_eq!(plan.journal.len(), 3);
        let (_, snapshot) = plan.prepare().unwrap();
        assert_eq!(texts(&snapshot), ["one", "two", "three"]);
        assert!(snapshot.omissions.earlier_context);
        request.budget = ReferenceBudget {
            max_messages: 2,
            ..Default::default()
        };
        let (_, snapshot) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(texts(&snapshot), ["two", "three"]);
        assert!(snapshot.omissions.earlier_context);
        request.budget = ReferenceBudget {
            max_context_bytes: 1,
            ..Default::default()
        };
        let (_, snapshot) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        assert!(snapshot.messages.is_empty());
        assert!(snapshot.omissions.earlier_context);
        request.budget.max_source_bytes = 1;
        assert!(flow.capture_reference_plan(&request).is_err());
    }

    #[test]
    fn nested_reference_images_use_the_same_capture_budget_and_ownership_validation() {
        use crate::context_plan::{
            ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
        };
        let (_dir, mut flow, lease) = fixture();
        let source = crate::attachment_store::AttachmentStore::at(flow.dir().unwrap())
            .import_bytes(b"\x89PNG\r\n\x1a\n\0\0\0\r", None)
            .unwrap();
        publish(
            &mut flow,
            &lease,
            Message::context_record(
                lease.turn.clone(),
                ContextRecord::new(
                    "quickask.source",
                    1,
                    ContextRecordAuthority::Retrieved,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::Reference {
                        text: "captured image".into(),
                        images: vec![source],
                        origin: None,
                    },
                ),
            ),
        );
        let mut request = ReferenceRequest::current(flow.state().address().clone());
        let (_, captured) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        let MessagePart::ContextRecord(record) = &captured.messages[0].parts[0] else {
            panic!("expected retained record")
        };
        assert!(matches!(
            record.body().images()[0].data,
            ImageData::Base64 { .. }
        ));
        request.budget.max_artifact_bytes = 0;
        let (_, bounded) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(bounded.omissions.unavailable_images, 1);
        let MessagePart::ContextRecord(record) = &bounded.messages[0].parts[0] else {
            panic!("expected retained record")
        };
        assert!(record.body().images().is_empty());
        let roundtrip: ContextRecord =
            serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
        assert_eq!(roundtrip.digest(), record.digest());
    }

    #[test]
    fn reference_artifacts_outlive_source_deletion_without_inheriting_file_access() {
        let (dir, mut flow, lease) = fixture();
        let image_bytes = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
        ];
        let image = crate::attachment_store::AttachmentStore::at(flow.dir().unwrap())
            .import_bytes(&image_bytes, Some("capture.png"))
            .unwrap();
        let output = "retained tool output\n".repeat(100);
        let reference = truncate_tool_result_content_with_budget(
            &output,
            "result",
            Some(&OutputStore::at(flow.dir().unwrap())),
            ToolOutputBudget {
                max_lines: 1,
                max_bytes: 8,
                max_line_bytes: 8,
            },
        );
        let message = |role, parts| Message {
            turn_id: lease.turn.clone(),
            role,
            origin: MessageOrigin::Internal,
            parts,
        };
        publish(
            &mut flow,
            &lease,
            message(
                MessageRole::User,
                vec![MessagePart::Image { source: image }],
            ),
        );
        publish(
            &mut flow,
            &lease,
            message(
                MessageRole::Assistant,
                vec![MessagePart::ToolUse {
                    id: "read".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({}),
                    intent: None,
                }],
            ),
        );
        publish(
            &mut flow,
            &lease,
            message(
                MessageRole::Tool,
                vec![MessagePart::ToolResult {
                    tool_use_id: "read".into(),
                    content: reference.clone(),
                    is_error: false,
                }],
            ),
        );
        publish(
            &mut flow,
            &lease,
            message(
                MessageRole::Assistant,
                vec![MessagePart::ToolUse {
                    id: "pending".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({}),
                    intent: None,
                }],
            ),
        );
        let request = ReferenceRequest::current(flow.state().address().clone());
        let (_, snapshot) = flow
            .capture_reference_plan(&request)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(snapshot.messages.len(), 3);
        assert_eq!(snapshot.omissions.incomplete_tool_parts, 1);
        assert_eq!(snapshot.omissions.unavailable_images, 0);
        assert_eq!(snapshot.omissions.unavailable_outputs, 0);
        let mut bounded = request.clone();
        bounded.budget.max_artifact_bytes = 0;
        let (_, without_assets) = flow
            .capture_reference_plan(&bounded)
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(without_assets.omissions.unavailable_images, 1);
        assert_eq!(without_assets.omissions.unavailable_outputs, 1);
        assert!(
            without_assets
                .outputs
                .retained_content(&reference)
                .is_none()
        );
        bounded.budget = ReferenceBudget {
            max_messages: 2,
            ..Default::default()
        };
        let (_, incomplete) = flow
            .capture_reference_plan(&bounded)
            .unwrap()
            .prepare()
            .unwrap();
        assert!(incomplete.messages.is_empty());
        assert_eq!(incomplete.omissions.incomplete_tool_parts, 2);
        drop(flow);
        dir.close().unwrap();
        let MessagePart::Image { source } = &snapshot.messages[0].parts[0] else {
            panic!("captured image must remain readable");
        };
        assert_eq!(
            crate::attachment_store::image_bytes(source).unwrap(),
            image_bytes
        );
        assert!(matches!(source.data, ImageData::Base64 { .. }));
        assert_eq!(
            snapshot.outputs.retained_content(&reference).as_deref(),
            Some(output.as_str())
        );
        assert!(snapshot.outputs.register("write", "forbidden").is_none());
        let unknown = reference.replace(&output.len().to_string(), "1");
        assert!(snapshot.outputs.retained_content(&unknown).is_none());
    }
}
