//! Bounded source observations captured at a durable Flow owner's boundary.

use std::{collections::BTreeMap, io, path::PathBuf, sync::Arc};

use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowRef, FlowStore, inbox::InputContent, invalid};
use crate::{
    event_log::journal::{CapturedJournal, CommitReceipt},
    message::{ImageData, Message, MessagePart},
    tools::tool_output::OutputStore,
};

mod reference_record;
pub use reference_record::PreparedReference;
mod boundary;
mod fork_boundary;
pub(super) use boundary::BoundarySource;
pub use boundary::{
    BoundaryReferenceRequest, BoundaryReferenceSnapshot, ConversationCutoff,
    ResolvedConversationBoundary,
};
pub use fork_boundary::{ForkBoundaryRequest, ForkCutoff, ResolvedForkBoundary};

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
#[derive(Clone, Debug)]
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
#[derive(Clone, Debug)]
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
    pub(crate) flow_project: Option<crate::storage::ProjectContext>,
    pub(crate) configuration: super::SessionConfiguration,
    pub(crate) execution: Option<super::launch::EntryBinding>,
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
            flow_project: self.project().cloned(),
            configuration: self.session_descriptor().configuration.clone(),
            execution: self
                .activation
                .as_ref()
                .and_then(|(activation, _)| activation.input.execution.clone()),
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

    pub(in crate::flow_store) fn fixture() -> (tempfile::TempDir, FlowStore, TurnLease<AtmanUuid>) {
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
        let mut flow = session
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
