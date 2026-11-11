use std::{num::NonZeroUsize, sync::Arc};

use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId};

use super::*;
use crate::{
    flow_store::{
        EntrySnapshot, FlowInstanceId, FlowTransition, RuntimeStore, SessionDescriptor,
        completion::{InvocationResult, StoredValue},
        inbox::{
            DeliveryMode, DeliveryRequest, InboxChange, InboxEditRequest, InboxPayload,
            SenderIdentity, UserContent,
        },
        publication::MessagePublication,
        transient::{TransientBudget, TransientLimits, TransientRetention},
    },
    message::{ImageData, ImageSource, Message, MessagePart},
};

fn fixture() -> (tempfile::TempDir, SessionStore, FlowStore, TransientBudget) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let id = Uuid::now_v7();
    let mut session = runtime
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Retention".into(),
        })
        .unwrap();
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: crate::source_program::RevisionBundle {
                entry: "entry:main.at".into(),
                edges: Default::default(),
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> int { return 7 }".into(),
                )]
                .into(),
            },
        }
        .into(),
    };
    let budget = TransientBudget::new(256 * 1024).unwrap();
    let retention = TransientRetention::new(
        budget.clone(),
        Uuid::now_v7(),
        descriptor.address.instance_id.0.0,
        TransientLimits {
            maximum_bytes: 128 * 1024,
            terminal_reserve_bytes: 16 * 1024,
        },
    )
    .unwrap();
    let flow = session
        .create_transient_flow(descriptor, None, retention)
        .unwrap();
    (directory, session, flow, budget)
}

fn content(text: &str) -> UserContent {
    UserContent {
        text: text.into(),
        images: Vec::new(),
        presentation: None,
    }
}

fn queue(flow: &mut FlowStore) -> InboxEditRequest {
    let sender = SenderIdentity::User(Uuid::now_v7());
    let accepted = flow
        .accept_input(
            DeliveryRequest {
                expected_head: None,
                request_id: Uuid::now_v7(),
                sender: sender.clone(),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::UserSubmission {
                    content: content("queued"),
                    invocation: Default::default(),
                },
            },
            |_| Ok(()),
        )
        .unwrap();
    let edit = InboxEditRequest {
        expected_head: None,
        id: Uuid::now_v7(),
        sender,
        message: accepted.message_id,
        expected: accepted.receipt,
        change: InboxChange::Replace {
            content: content("edited"),
            delivery: DeliveryMode::NextTurn,
        },
    };
    flow.edit_input(edit.clone(), |_| Ok(())).unwrap();
    edit
}

fn freeze(flow: &mut FlowStore) -> RetentionCapture {
    let cursor = flow.cursor().clone();
    flow.capture_retention(Uuid::now_v7(), &cursor).unwrap()
}

#[test]
fn transfer_preserves_full_history_ids_assets_and_pending_input_with_typed_positions() {
    let (_directory, mut session, mut flow, budget) = fixture();
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
        panic!("active turn")
    };
    let lease = active.lease.clone();
    let mut message = Message::assistant_text(lease.turn.clone(), "answer");
    message.parts.push(MessagePart::Image {
        source: ImageSource { media_type: "image/png".into(), detail: Default::default(), data: ImageData::Base64 { data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a3ioAAAAASUVORK5CYII=".into() } },
    });
    let publication = MessagePublication {
        id: Uuid::now_v7(),
        lease: lease.clone(),
        run: lease.run.clone(),
        context_run: None,
        message,
    };
    flow.publish_message(publication.clone()).unwrap();
    flow.finish_turn(lease, InvocationResult::Returned(StoredValue::Int(7)))
        .unwrap();
    let edit = queue(&mut flow);
    let output = flow
        .output_store()
        .register("tool", "captured output\n")
        .unwrap();
    let capture = freeze(&mut flow);
    let owner = session.owner.clone();
    let source_cursor = flow.cursor().clone();
    let cursor = session.cursor().clone();
    let mut kept = session.retain_flow(&capture, &cursor).unwrap();
    assert!(Arc::ptr_eq(&session.owner, &owner));
    assert_eq!(flow.cursor(), &source_cursor);
    assert_eq!(kept.store.descriptor(), flow.descriptor());
    assert_eq!(kept.store.pending_inputs().count(), 1);
    assert_eq!(
        kept.store
            .published_message(publication.id)
            .unwrap()
            .unwrap()
            .publication,
        publication
    );
    assert_eq!(
        kept.store
            .output_store()
            .read_lines(&output, 0, 10, Default::default())
            .unwrap()
            .content,
        "captured output\n"
    );
    let page = kept
        .store
        .journal
        .read_after(None, NonZeroUsize::new(100).unwrap())
        .unwrap();
    assert_eq!(page.batches.len(), capture.history.len());
    for (index, batch) in page.batches.iter().enumerate() {
        let mapped = capture
            .history
            .map_batch(index, |position| {
                *position = page.batches[position.sequence() as usize - 1]
                    .receipt
                    .clone();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(&batch.facts).unwrap(),
            serde_json::to_value(mapped.facts).unwrap()
        );
    }
    let mut mapped_edit = edit;
    mapped_edit.expected = page.batches[mapped_edit.expected.sequence() as usize - 1]
        .receipt
        .clone();
    assert!(
        kept.store
            .input_edit_receipt(&mapped_edit)
            .unwrap()
            .is_some()
    );
    assert_eq!(session.projection.flows.len(), 1);
    assert_eq!(
        session
            .retention_receipt(capture.request(), &source_cursor)
            .unwrap(),
        Some(kept.receipt.clone())
    );
    drop(flow);
    assert!(budget.retained_bytes() > 0);
    drop(capture);
    assert_eq!(budget.retained_bytes(), 0);
}

#[test]
fn transfer_retains_the_flow_project_after_its_session_moves() {
    let (directory, mut session, mut flow, _budget) = fixture();
    let original: ProjectContext = serde_json::from_value(serde_json::json!({
        "root": directory.path().join("original"), "scope": "local", "storage_root": directory.path().join("original/.atman")
    })).unwrap();
    let moved: ProjectContext = serde_json::from_value(serde_json::json!({
        "root": directory.path().join("moved"), "scope": "local", "storage_root": directory.path().join("moved/.atman")
    })).unwrap();
    flow.project = Some(original.clone());
    let before = session.cursor().clone();
    session.set_project(&before, Some(moved.clone())).unwrap();
    let capture = freeze(&mut flow);
    let cursor = session.cursor().clone();
    let kept = session.retain_flow(&capture, &cursor).unwrap();
    assert_eq!(kept.store.project(), Some(&original));
    assert_eq!(session.projection.project.as_ref(), Some(&moved));
    assert_eq!(
        session.projection.flows[0].project.as_ref(),
        Some(&original)
    );
    let id = kept.receipt.flow.instance_id;
    drop(kept.store);
    assert_eq!(session.open_flow(&id).unwrap().project(), Some(&original));
}

#[test]
fn failed_asset_publication_retains_memory_and_retries_only_the_fixed_capture() {
    let (_directory, mut session, mut flow, budget) = fixture();
    let output = flow.output_store().register("tool", "asset").unwrap();
    let capture = freeze(&mut flow);
    let before = session.cursor().clone();
    let retained = budget.retained_bytes();
    let plans = session.dir.join("retentions");
    std::fs::write(&plans, "blocked").unwrap();
    assert!(session.retain_flow(&capture, &before).is_err());
    assert_eq!(session.cursor(), &before);
    assert!(session.projection.flows.is_empty());
    assert_eq!(budget.retained_bytes(), retained);
    assert_eq!(
        flow.output_store()
            .read_lines(&output, 0, 1, Default::default())
            .unwrap()
            .content,
        "asset"
    );
    std::fs::remove_file(plans).unwrap();
    let kept = session.retain_flow(&capture, &before).unwrap();
    let receipt = kept.receipt.clone();
    drop(kept);
    assert_eq!(
        session.retain_flow(&capture, &before).unwrap().receipt,
        receipt
    );
    assert_eq!(session.projection.flows.len(), 1);
    let mut foreign = FlowPosition::Transient {
        transient: capture.source().clone(),
    };
    if let FlowPosition::Transient { transient } = &mut foreign {
        transient.host_generation = Uuid::now_v7();
    }
    assert!(
        session
            .retention_receipt(capture.request(), &foreign)
            .is_err()
    );
}

#[test]
fn completed_unregistered_destination_is_reconciled_without_rewriting_history() {
    let (_directory, mut session, mut flow, _budget) = fixture();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    queue(&mut flow);
    let capture = freeze(&mut flow);
    let destination = session
        .dir
        .join("flows")
        .join(capture.flow().instance_id.to_string());
    initialize_dir(&destination, |stage| write_history(stage, &capture)).unwrap();
    let original = std::fs::read(destination.join("events.jsonl")).unwrap();
    let cursor = session.cursor().clone();
    let kept = session.retain_flow(&capture, &cursor).unwrap();
    assert_eq!(
        std::fs::read(destination.join("events.jsonl")).unwrap(),
        original
    );
    assert_eq!(kept.store.pending_inputs().count(), 1);
    drop(kept);
    drop(session);
    drop(flow);
    let runtime = RuntimeStore::open(_directory.path()).unwrap();
    let mut session = runtime.open_session(capture.flow().session_id.0).unwrap();
    assert_eq!(
        session
            .retain_flow(&capture, &cursor)
            .unwrap()
            .receipt
            .source,
        *capture.source()
    );
}

#[test]
fn fixed_request_rejects_changed_capture_and_conflicting_assets() {
    let (_directory, mut session, mut flow, _budget) = fixture();
    let output = flow.output_store().register("tool", "original").unwrap();
    let mut capture = freeze(&mut flow);
    let cursor = session.cursor().clone();
    let kept = session.retain_flow(&capture, &cursor).unwrap();
    let path = kept.store.dir().unwrap().to_path_buf();
    drop(kept);
    let plan = capture.plan.clone();
    capture.plan.outputs_digest = format!("blake3:{}", "0".repeat(64));
    assert!(session.retain_flow(&capture, &cursor).is_err());
    capture.plan = plan;
    let output_path = path.join("tool_outputs").join(format!("{output}.txt"));
    std::fs::write(output_path, "conflict").unwrap();
    assert!(session.retain_flow(&capture, &cursor).is_err());
    assert_eq!(
        flow.output_store()
            .read_lines(&output, 0, 1, Default::default())
            .unwrap()
            .content,
        "original"
    );
}

#[test]
fn uncertain_catalog_replays_under_the_same_owner_and_recovers_the_recorded_transfer() {
    let (_directory, mut session, mut flow, _budget) = fixture();
    let capture = freeze(&mut flow);
    let destination = session
        .dir
        .join("flows")
        .join(capture.flow().instance_id.to_string());
    initialize_dir(&destination, |stage| write_history(stage, &capture)).unwrap();
    let manifest: RetentionManifest =
        serde_json::from_slice(&std::fs::read(destination.join(MANIFEST)).unwrap()).unwrap();
    let registration = FlowRegistration {
        id: capture.flow().instance_id.clone(),
        creation_request: capture.plan.creation_request,
        project: capture.plan.project.clone(),
        retention: Some(manifest.receipt.clone()),
        retention_resume: None,
    };
    let before = session.cursor().clone();
    let owner = session.owner.clone();
    // Persist the complete registration without installing its application acknowledgement.
    let written = session
        .journal
        .commit(&[
            SessionFact::FlowRetained(Box::new(registration)),
            SessionFact::DefaultFlowSelected(capture.flow().instance_id.clone()),
        ])
        .unwrap();
    let path = session.dir.join("events.jsonl");
    let bytes = std::fs::read(&path).unwrap();
    // A real read failure makes this same locked writer uncertain; restoring the file permits reconciliation.
    std::fs::write(&path, b"unavailable").unwrap();
    assert!(session.journal.read_batch::<SessionFact>(&written).is_err());
    assert!(session.journal.needs_reconciliation());
    std::fs::write(&path, &bytes).unwrap();
    let kept = session.retain_flow(&capture, &before).unwrap();
    assert_eq!(kept.receipt, manifest.receipt);
    assert_eq!(session.cursor(), &written);
    assert!(Arc::ptr_eq(&owner, &session.owner));
    assert!(!session.journal.needs_reconciliation());
    assert_eq!(session.projection.flows.len(), 1);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
