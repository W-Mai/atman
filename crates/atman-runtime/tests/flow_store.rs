use std::collections::BTreeMap;

use atman_rt::{
    InstanceActivity, InstanceState, Interruption, RunId, TurnId, TurnLease, TurnOutcome, TurnPhase,
};
use atman_runtime::event::AtmanUuid;
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, InputContent, SenderIdentity,
};
use atman_runtime::flow_store::outbox::{DeliveryReceipt, SendStatus, SendSummary};
use atman_runtime::flow_store::waiter::{ReplyStatus, ReplyWaiter};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor,
};
use atman_runtime::message::Message;
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

mod common;

fn durable_position(
    position: &mut atman_runtime::flow_store::position::FlowPosition,
) -> &mut atman_runtime::event_log::journal::CommitReceipt {
    let atman_runtime::flow_store::position::FlowPosition::Durable(receipt) = position else {
        panic!("expected durable test history")
    };
    receipt
}

#[test]
fn execution_observations_keep_live_lease_time_and_replay_without_inventing_a_clock() {
    use atman_runtime::{
        event::Event,
        flow_store::{
            execution::TurnTiming,
            trace::{ExecutionEvent, ExecutionScope},
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_descriptor()).unwrap();
    let info = descriptor(session.projection().descriptor.id);
    let mut flow = session.create_flow(info.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let first = start(&mut flow);
    let admitted = match flow.execution_observation().active.unwrap().timing {
        TurnTiming::Local { admitted_at, .. } => admitted_at,
        TurnTiming::Unknown => panic!("live admission must have a clock"),
    };
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: first.clone(),
            phase: TurnPhase::Waiting(atman_rt::WaitReason::Effect),
        },
    );
    let revision = flow.state().revision();
    let cursor = flow.cursor().clone();
    let sampled = flow.execution_observation();
    assert_eq!(sampled.receipt, cursor);
    assert_eq!(flow.state().revision(), revision);
    assert!(
        matches!(sampled.active.unwrap().timing, TurnTiming::Local { admitted_at, .. } if admitted_at == admitted)
    );
    let event = |lease: &TurnLease<AtmanUuid>, sequence| ExecutionEvent {
        scope: ExecutionScope::Invocation(lease.clone()),
        sequence,
        timestamp: chrono::Utc::now(),
        event: Event::FlowStart {
            run_id: lease.run.clone(),
            flow_name: "worker".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        },
    };
    flow.append_execution_events(vec![event(&first, 1)])
        .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: first.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    assert!(flow.execution_observation().active.is_none());
    let second = start(&mut flow);
    assert!(flow.execution_observation().last_execution_event.is_none());
    flow.append_execution_events(vec![event(&second, 2), event(&first, 3)])
        .unwrap();
    assert_eq!(
        flow.execution_observation()
            .last_execution_event
            .unwrap()
            .lease,
        second
    );
    let inspected = session.inspect_flow(&info.address.instance_id).unwrap();
    assert!(!inspected.execution.loaded);
    assert!(matches!(
        inspected.execution.active.unwrap().timing,
        TurnTiming::Unknown
    ));
    assert_eq!(
        inspected.execution.last_execution_event.unwrap().lease,
        second
    );
    drop(flow);
    let reopened = session.open_flow(&info.address.instance_id).unwrap();
    assert!(reopened.execution_observation().active.is_none());
    assert!(matches!(
        reopened.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: Interruption::HostLost,
            ..
        })
    ));
}

#[test]
fn history_search_keeps_storage_scope_and_committed_transcript_identity() {
    use atman_runtime::{
        config_hub::ConfigHub,
        event::Event,
        flow_store::{
            context::{ContextCheckpoint, context_digest},
            publication::MessagePublication,
        },
        history_search::{HistoryLocation, SearchRequest, SearchScope, SearchSource, search},
        index::AnchorIndex,
        message::MessageOrigin,
        session_meta::SessionMeta,
        storage::ProjectContext,
    };

    let data = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_config_dir(data.path().join("config"));
    let alpha = ProjectContext::resolve(&hub, project.path(), data.path()).unwrap();
    let beta = ProjectContext::resolve(&hub, other.path(), data.path()).unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut info = session_descriptor();
    info.project = Some(alpha.clone());
    let mut session = root.create_session(info.clone()).unwrap();
    let mut flows = Vec::new();
    for (binding, text) in [
        (alpha.clone(), "needle first"),
        (alpha.clone(), "needle second"),
        (beta, "needle foreign"),
    ] {
        session
            .set_project(&session.cursor().clone(), Some(binding))
            .unwrap();
        let description = descriptor(info.id);
        let mut flow = session.create_flow(description.clone()).unwrap();
        change(&mut flow, FlowTransition::Activated);
        flow.accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::now_v7()),
                text,
            ),
            |_| Ok(()),
        )
        .unwrap();
        let batch = flow
            .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
            .unwrap()
            .unwrap();
        let lease = batch.lease().clone();
        flow.commit_input(
            &batch,
            InputContent::InvocationInput {
                command: None,
                arguments: BTreeMap::new(),
                invocation: BTreeMap::new(),
                messages: vec![Message::user_text(lease.turn.clone(), text)],
            },
        )
        .unwrap();
        for (text, internal, isolated) in [
            ("needle answer", false, false),
            ("needle internal", true, false),
            ("needle isolated", false, true),
        ] {
            let mut message = Message::assistant_text(lease.turn.clone(), text);
            if internal {
                message.origin = MessageOrigin::Internal;
            }
            let run = if isolated {
                RunId::now()
            } else {
                lease.run.clone()
            };
            flow.publish_message(MessagePublication {
                id: Uuid::now_v7(),
                lease: lease.clone(),
                context_run: isolated.then(|| run.clone()),
                run,
                message,
            })
            .unwrap();
        }
        flow.accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::now_v7()),
                "needle pending",
            ),
            |_| Ok(()),
        )
        .unwrap();
        let window = flow.context_messages().unwrap();
        flow.checkpoint_context(ContextCheckpoint {
            id: Uuid::now_v7(),
            expected: context_digest(&window).unwrap(),
            messages: vec![Message::user_text(lease.turn.clone(), "needle checkpoint")],
            summary: "needle checkpoint".into(),
            range: 0..window.len(),
            count: window.len(),
            before_tokens: 100,
            after_tokens: 10,
        })
        .unwrap();
        flows.push((flow, description.address));
    }

    // An original index with the same UUID is a separate source; its Session directory may be gone.
    let indexed = AnchorIndex::open_project(alpha.storage_root()).unwrap();
    let turn = TurnId::now();
    let event = Event::UserMsg {
        turn_id: turn.clone(),
        flow_run_id: None,
        message: Message::user_text(turn, "needle original"),
        presentation: None,
        injection_id: None,
    };
    indexed
        .insert_project_event(
            &info.id.to_string(),
            &event,
            &serde_json::to_string(&event).unwrap(),
        )
        .unwrap();
    let index_path = indexed.path().to_owned();
    drop(indexed);
    let index_before = std::fs::read(&index_path).unwrap();
    let journals: Vec<_> = flows
        .iter()
        .map(|(flow, _)| {
            let path = flow.dir().unwrap().join("events.jsonl");
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    let source = SearchSource::Instance(flows[0].1.clone());
    let mut request = SearchRequest {
        query: "needle".into(),
        scope: SearchScope::Session,
    };
    let result = search(
        data.path(),
        &source,
        &request,
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(result.hits.len(), 6);
    assert!(
        result
            .hits
            .iter()
            .all(|hit| matches!(&hit.location, HistoryLocation::Flow { .. }))
    );
    assert!(result.hits.iter().all(|hit| {
        !["internal", "isolated", "pending", "checkpoint"]
            .iter()
            .any(|text| hit.snippet.contains(text))
    }));
    let first = result
        .hits
        .iter()
        .find(|hit| hit.snippet == "needle first")
        .unwrap();
    let second = result
        .hits
        .iter()
        .find(|hit| hit.snippet == "needle second")
        .unwrap();
    assert_eq!(first.seq, second.seq);
    assert_ne!(first.location, second.location);
    assert!(first.preview.join("\n").contains("needle answer"));
    assert!(!first.preview.join("\n").contains("needle second"));

    request.scope = SearchScope::Project;
    let result = search(
        data.path(),
        &source,
        &request,
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    assert_eq!(result.hits.len(), 5);
    assert!(
        !result
            .hits
            .iter()
            .any(|hit| hit.snippet.contains("foreign"))
    );
    assert!(result.hits.iter().any(
        |hit| matches!(&hit.location, HistoryLocation::Indexed { .. })
            && hit.preview.join("\n").contains("needle original")
    ));
    assert!(!data.path().join("sessions").exists());

    let legacy = data.path().join("sessions").join(info.id.to_string());
    std::fs::create_dir_all(&legacy).unwrap();
    SessionMeta {
        project_root: Some(alpha.root().to_owned()),
        ..Default::default()
    }
    .save(&legacy)
    .unwrap();
    // An absent local index must stay absent after search.
    request.scope = SearchScope::Session;
    let result = search(
        data.path(),
        &SearchSource::Legacy(info.id),
        &request,
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.hits.len(), 1);
    assert!(matches!(
        &result.hits[0].location,
        HistoryLocation::Indexed { .. }
    ));
    assert!(!project.path().join(".atman/index.db").exists());
    request.query = "/NEEDLE (first|second)/".into();
    assert_eq!(
        search(
            data.path(),
            &source,
            &request,
            &tokio_util::sync::CancellationToken::new()
        )
        .unwrap()
        .hits
        .len(),
        2
    );
    request.query = "/[/".into();
    assert!(
        search(
            data.path(),
            &source,
            &request,
            &tokio_util::sync::CancellationToken::new()
        )
        .is_err()
    );
    for (path, bytes) in journals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(std::fs::read(index_path).unwrap(), index_before);
}

#[test]
fn opening_an_owned_session_reports_cross_process_ownership() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let descriptor = session_descriptor();
    let session = root.create_session(descriptor.clone()).unwrap();
    let error = match root.open_session(descriptor.id) {
        Ok(_) => panic!("an owned session must reject a second writer"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(
        error
            .to_string()
            .contains("session is already open in another Atman process")
    );
    drop(session);
}

#[test]
fn native_image_ownership_survives_source_loss_edits_checkpoints_and_replay() {
    use atman_runtime::{
        attachment_store::{AttachmentStore, image_bytes},
        flow_store::{
            context::{ContextCheckpoint, context_digest},
            inbox::{InboxChange, InboxEditRequest, UserContent},
            inbox_view::{InboxHistoryItem, InboxSide},
            publication::MessagePublication,
        },
        message::{ImageData, MessagePart},
    };
    let data = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let original = AttachmentStore::at(external.path()).import_base64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7p8AAAAASUVORK5CYII=", Some("pixel.png")).unwrap();
    let bytes = image_bytes(&original).unwrap();
    let mut source = original.clone();
    let ImageData::Artifact { path, .. } = &original.data else {
        unreachable!()
    };
    source.data = ImageData::Path { path: path.clone() };
    let root = RuntimeStore::open(data.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let mut request = delivery(
        &description.address,
        SenderIdentity::User(Uuid::now_v7()),
        "",
    );
    let content = UserContent {
        text: "picture".into(),
        images: vec![source.clone()],
        presentation: None,
    };
    request.payload = InboxPayload::UserSubmission {
        content: content.clone(),
        invocation: BTreeMap::new(),
    };
    let accepted = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut edit = InboxEditRequest {
        id: Uuid::now_v7(),
        sender: request.sender.clone(),
        message: accepted.message_id,
        expected: accepted.receipt.clone(),
        change: InboxChange::Replace {
            content,
            delivery: DeliveryMode::NextTurn,
        },
    };
    // Replacement has its own binding, independent of queue ordering receipts.
    if let InboxChange::Replace { content, .. } = &mut edit.change {
        content.images = vec![original.clone()];
    }
    let edited = flow.edit_input(edit.clone(), |_| Ok(())).unwrap();
    external.close().unwrap();
    assert_eq!(
        flow.accept_input(request.clone(), |_| panic!("retry must not reauthorize"))
            .unwrap(),
        accepted
    );
    assert_eq!(flow.edit_input(edit.clone(), |_| Ok(())).unwrap(), edited);
    let entry = flow.inbox_entry(accepted.message_id).unwrap().unwrap();
    let InboxPayload::UserSubmission { content, .. } = entry.envelope.request.payload else {
        unreachable!()
    };
    assert_ne!(content.images[0], original);
    assert_eq!(image_bytes(&content.images[0]).unwrap(), bytes);
    let pending = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = pending.lease().clone();
    let mut message = Message::user_text(lease.turn.clone(), "picture");
    message.parts.push(MessagePart::Image {
        source: content.images[0].clone(),
    });
    flow.commit_input(
        &pending,
        InputContent::InvocationInput {
            command: None,
            arguments: BTreeMap::new(),
            invocation: BTreeMap::new(),
            messages: vec![message.clone()],
        },
    )
    .unwrap();
    let publication = MessagePublication {
        id: Uuid::now_v7(),
        lease: lease.clone(),
        run: lease.run.clone(),
        context_run: None,
        message: message.clone(),
    };
    let published = flow.publish_message(publication.clone()).unwrap();
    assert_eq!(published.publication.message, message);
    let before = flow.context_messages().unwrap();
    let checkpoint = ContextCheckpoint {
        id: Uuid::now_v7(),
        expected: context_digest(&before).unwrap(),
        messages: vec![message],
        summary: "retained image".into(),
        range: 0..0,
        count: 0,
        before_tokens: 10,
        after_tokens: 10,
    };
    let saved = flow.checkpoint_context(checkpoint.clone()).unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let dir = flow.dir().unwrap().to_path_buf();
    let history = flow.history_messages().unwrap();
    let cursor = flow.cursor().clone();
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.history_messages().unwrap(), history);
    assert_eq!(flow.context_messages().unwrap(), saved.checkpoint.messages);
    assert_eq!(flow.checkpoint_context(checkpoint).unwrap(), saved);
    assert_eq!(flow.publish_message(publication).unwrap(), published);
    assert_eq!(
        flow.accept_input(request.clone(), |_| Ok(()))
            .unwrap()
            .receipt,
        accepted.receipt
    );
    let inspection = RuntimeStore::inspect_session(data.path(), info.id).unwrap();
    let page = inspection
        .inspect_inbox(
            &description.address.instance_id,
            InboxSide::Received,
            None,
            10,
        )
        .unwrap();
    let InboxHistoryItem::Received(item) = &page.items[0] else {
        unreachable!()
    };
    let InboxPayload::UserSubmission { content, .. } = &item.envelope.request.payload else {
        unreachable!()
    };
    assert_eq!(image_bytes(&content.images[0]).unwrap(), bytes);
    let mut live = Vec::new();
    let mut after = None;
    loop {
        let page = flow
            .read_updates(after.as_ref(), std::num::NonZeroUsize::new(32).unwrap())
            .unwrap();
        for batch in page.batches {
            live.push(serde_json::to_value(batch).unwrap());
        }
        if !page.has_more {
            break;
        }
        after = page.next;
    }
    let mut restored = Vec::new();
    inspection
        .inspect_flow_updates(&description.address.instance_id, |receipt, facts| {
            restored.push(
                serde_json::to_value(atman_runtime::event_log::journal::CommittedBatch {
                    receipt: receipt.require_durable().unwrap().clone(),
                    facts,
                })
                .unwrap(),
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(live, restored);
    assert!(
        std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap()
            .contains("images_retained")
    );
    let mut conflict = request;
    if let InboxPayload::UserSubmission { content, .. } = &mut conflict.payload {
        content.images[0].detail = atman_runtime::provider::ImageDetail::High;
    }
    assert!(flow.accept_input(conflict, |_| Ok(())).is_err());
}

#[test]
fn native_image_failure_never_accepts_or_changes_the_journal() {
    use atman_runtime::{attachment_store::AttachmentStore, flow_store::inbox::UserContent};
    let data = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let image = AttachmentStore::at(external.path()).import_base64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7p8AAAAASUVORK5CYII=", None).unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let mut request = delivery(
        &description.address,
        SenderIdentity::User(Uuid::now_v7()),
        "",
    );
    request.payload = InboxPayload::UserSubmission {
        content: UserContent {
            text: "picture".into(),
            images: vec![image],
            presentation: None,
        },
        invocation: BTreeMap::new(),
    };
    let cursor = flow.cursor().clone();
    let journal = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    std::fs::write(flow.dir().unwrap().join("attachments"), "not a directory").unwrap();
    assert!(flow.accept_input(request.clone(), |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.pending_inputs().next().is_none());
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        journal
    );
    std::fs::remove_file(flow.dir().unwrap().join("attachments")).unwrap();
    external.close().unwrap();
    assert!(flow.accept_input(request.clone(), |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.pending_inputs().next().is_none());
}

#[test]
fn committed_input_index_restores_payloads_without_retaining_historical_bodies() {
    use atman_runtime::flow_store::context::{ContextCheckpoint, context_digest};

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::now_v7()),
            "start",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = batch.lease().clone();
    let invocation = InputContent::InvocationInput {
        command: None,
        arguments: BTreeMap::from([(
            "input".into(),
            serde_json::json!("ARGUMENT_BODY".repeat(4096)),
        )]),
        invocation: BTreeMap::from([(
            "marker".into(),
            serde_json::json!("ENVIRONMENT_BODY".repeat(4096)),
        )]),
        messages: vec![Message::user_text(
            lease.turn.clone(),
            "INVOCATION_BODY".repeat(4096),
        )],
    };
    let first = flow.commit_input(&batch, invocation.clone()).unwrap();
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::now_v7()),
            "context",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let batch = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let context = InputContent::Context {
        messages: vec![Message::user_text(
            lease.turn.clone(),
            "CONTEXT_BODY".repeat(4096),
        )],
    };
    let second = flow.commit_input(&batch, context.clone()).unwrap();
    let waiter = ReplyWaiter {
        correlation: Uuid::now_v7(),
        lease: lease.clone(),
        sender: SenderIdentity::Flow(FlowRef {
            session_id: info.id.into(),
            instance_id: FlowInstanceId::now(),
        }),
    };
    flow.register_reply_waiter(waiter.clone()).unwrap();
    wait_phase(&mut flow, &waiter);
    flow.accept_input(reply_request(&flow, &waiter), |_| Ok(()))
        .unwrap();
    let batch = flow
        .reserve_reply_input(&lease, waiter.correlation, |_| true)
        .unwrap()
        .unwrap();
    let reply = InputContent::WaitingContinuation {
        correlation: waiter.correlation,
        value: serde_json::json!({"result": "REPLY_BODY".repeat(4096)}),
    };
    let third = flow.commit_input(&batch, reply.clone()).unwrap();
    let history = flow.history_messages().unwrap();
    let summary = vec![Message::assistant_text(
        lease.turn.clone(),
        "compact window",
    )];
    flow.checkpoint_context(ContextCheckpoint {
        id: Uuid::now_v7(),
        expected: context_digest(&history).unwrap(),
        messages: summary.clone(),
        summary: "compact window".into(),
        range: 0..history.len(),
        count: history.len(),
        before_tokens: 1000,
        after_tokens: 10,
    })
    .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    for reopen in [false, true] {
        if reopen {
            drop(flow);
            flow = session.open_flow(&description.address.instance_id).unwrap();
        }
        let index = serde_json::to_string(flow.committed_inputs()).unwrap();
        assert!(index.len() < 4096);
        assert!(!index.contains("_BODY"));
        assert_eq!(
            flow.committed_inputs()
                .iter()
                .map(|record| record.message_count)
                .sum::<usize>(),
            2
        );
        for (receipt, content) in [(&first, &invocation), (&second, &context), (&third, &reply)] {
            assert_eq!(
                &flow
                    .committed_input(receipt)
                    .unwrap()
                    .unwrap()
                    .input
                    .content,
                content
            );
        }
        assert_eq!(flow.last_invocation_ref().unwrap().receipt, first);
        assert_eq!(
            flow.last_invocation_input().unwrap().unwrap().input.content,
            invocation
        );
        assert_eq!(flow.context_messages().unwrap(), summary);
        assert_eq!(flow.history_messages().unwrap(), history);
    }
    let cursor = flow.cursor().clone();
    let mut foreign = first.clone();
    durable_position(&mut foreign).journal_id = Uuid::now_v7();
    assert!(flow.committed_input(&foreign).is_err());
    foreign = first.clone();
    durable_position(&mut foreign).digest.push('0');
    assert!(flow.committed_input(&foreign).is_err());
    assert!(flow.committed_input(&cursor).unwrap().is_none());
    let journal = flow.dir().unwrap().join("events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let mut damaged = bytes.clone();
    let marker = b"CONTEXT_BODY";
    let position = damaged
        .windows(marker.len())
        .position(|part| part == marker)
        .unwrap();
    damaged[position] = b'X';
    std::fs::write(&journal, damaged).unwrap();
    assert_eq!(
        flow.committed_input(&first).unwrap().unwrap().input.content,
        invocation
    );
    assert!(flow.committed_input(&second).is_err());
    assert!(flow.history_messages().is_err());
    assert_eq!(flow.context_messages().unwrap(), summary);
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.pending_inputs().next().is_none());
    std::fs::write(&journal, bytes).unwrap();
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.history_messages().unwrap(), history);
    assert_eq!(flow.cursor(), &cursor);
}

#[test]
fn publication_index_reads_only_requested_bodies_and_preserves_checkpoint_history() {
    use atman_runtime::flow_store::context::{ContextCheckpoint, context_digest};
    use atman_runtime::flow_store::publication::MessagePublication;
    use atman_runtime::message::MessageOrigin;
    use std::num::NonZeroUsize;

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let mut saved = Vec::new();
    for text in ["FIRST_PUBLICATION_BODY", "SECOND_PUBLICATION_BODY"] {
        let mut message = Message::assistant_text(lease.turn.clone(), text.repeat(4096));
        message.origin = MessageOrigin::FinalAnswer;
        saved.push(
            flow.publish_message(MessagePublication {
                id: Uuid::now_v7(),
                lease: lease.clone(),
                run: lease.run.clone(),
                context_run: None,
                message,
            })
            .unwrap(),
        );
    }
    assert!(serde_json::to_vec(flow.published_messages()).unwrap().len() < 2048);
    let history = flow.history_messages().unwrap();
    let summary = vec![Message::assistant_text(
        lease.turn.clone(),
        "compact window",
    )];
    flow.checkpoint_context(ContextCheckpoint {
        id: Uuid::now_v7(),
        expected: context_digest(&history).unwrap(),
        messages: summary.clone(),
        summary: "compact window".into(),
        range: 0..history.len(),
        count: history.len(),
        before_tokens: 1000,
        after_tokens: 10,
    })
    .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.context_messages().unwrap(), summary);
    assert_eq!(flow.history_messages().unwrap(), history);
    assert!(serde_json::to_vec(flow.published_messages()).unwrap().len() < 2048);
    assert!(flow.published_message(Uuid::now_v7()).unwrap().is_none());
    assert_eq!(
        flow.publish_message(saved[1].publication.clone()).unwrap(),
        saved[1]
    );
    let mut conflict = saved[1].publication.clone();
    conflict.message = Message::assistant_text(conflict.lease.turn.clone(), "changed");
    assert!(flow.publish_message(conflict).is_err());

    let journal = flow.dir().unwrap().join("events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    let mut damaged = bytes.clone();
    let marker = b"SECOND_PUBLICATION_BODY";
    let position = damaged
        .windows(marker.len())
        .position(|part| part == marker)
        .unwrap();
    damaged[position] = b'X';
    std::fs::write(&journal, damaged).unwrap();
    let cursor = flow.cursor().clone();
    let first = flow.events_after(0, NonZeroUsize::new(1).unwrap()).unwrap();
    assert_eq!(first.through(), saved[0].receipt.sequence());
    assert!(
        flow.events_after(first.through(), NonZeroUsize::new(1).unwrap())
            .is_err()
    );
    assert!(flow.published_message(saved[1].publication.id).is_err());
    assert!(flow.publish_message(saved[1].publication.clone()).is_err());
    assert!(flow.history_messages().is_err());
    assert_eq!(flow.context_messages().unwrap(), summary);
    assert_eq!(flow.cursor(), &cursor);
    std::fs::write(&journal, bytes).unwrap();
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(
        flow.events_after(first.through(), NonZeroUsize::new(1).unwrap())
            .unwrap()
            .through(),
        saved[1].receipt.sequence()
    );
    assert_eq!(
        flow.published_message(saved[1].publication.id).unwrap(),
        Some(saved[1].clone())
    );
    assert_eq!(
        flow.publish_message(saved[1].publication.clone()).unwrap(),
        saved[1]
    );
    assert_eq!(flow.cursor(), &cursor);
}

#[test]
fn authored_input_recovers_interruption_without_releasing_pauses_or_replaying_retries() {
    for during_cancellation in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let info = session_descriptor();
        let mut session = root.create_session(info.clone()).unwrap();
        let description = descriptor(info.id);
        let mut flow = session.create_flow(description.clone()).unwrap();
        change(&mut flow, FlowTransition::Activated);
        let user = SenderIdentity::User(Uuid::now_v7());
        let old = delivery(&description.address, user.clone(), "earlier queued input");
        flow.accept_input(old.clone(), |_| Ok(())).unwrap();
        let lease = start(&mut flow);
        change(
            &mut flow,
            FlowTransition::PhaseChanged {
                lease: lease.clone(),
                phase: TurnPhase::Cancelling,
            },
        );
        let pause = Uuid::now_v7().into();
        change(&mut flow, FlowTransition::ExecutionPaused { key: pause });
        let next = delivery(&description.address, user.clone(), "new user request");
        if during_cancellation {
            flow.accept_input(next.clone(), |_| Ok(())).unwrap();
        }
        change(
            &mut flow,
            FlowTransition::TurnFinished {
                lease: lease.clone(),
                outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
            },
        );
        if !during_cancellation {
            let peer = delivery(
                &description.address,
                SenderIdentity::Flow(descriptor(info.id).address),
                "peer notification",
            );
            flow.accept_input(peer, |_| Ok(())).unwrap();
            flow.accept_input(old, |_| panic!("retry must not authorize again"))
                .unwrap();
            assert!(matches!(
                flow.state().state(),
                InstanceState::Open(InstanceActivity::Interrupted { .. })
            ));
            assert!(
                flow.accept_input(next.clone(), |_| Err(std::io::Error::other("denied")))
                    .is_err()
            );
            assert!(matches!(
                flow.state().state(),
                InstanceState::Open(InstanceActivity::Interrupted { .. })
            ));
            flow.accept_input(next.clone(), |_| Ok(())).unwrap();
        }
        assert_eq!(
            flow.state().state(),
            &InstanceState::Open(InstanceActivity::Idle)
        );
        assert_eq!(flow.state().pauses(), &[pause]);
        assert!(!flow.state().can_start_turn());
        assert_eq!(
            flow.completion(None).unwrap().completion.outcome,
            TurnOutcome::Interrupted(Interruption::Cancelled)
        );
        let expected = flow.state().clone();
        drop(flow);
        let mut flow = session.open_flow(&description.address.instance_id).unwrap();
        assert_eq!(flow.state(), &expected);
        assert_eq!(
            RuntimeStore::inspect_session(dir.path(), info.id)
                .unwrap()
                .inspect_flow(&description.address.instance_id)
                .unwrap()
                .state,
            expected
        );
        change(&mut flow, FlowTransition::ExecutionResumed { key: pause });
        let lease = start(&mut flow);
        change(
            &mut flow,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Interrupted(Interruption::Failed),
            },
        );
        flow.accept_input(next, |_| panic!("retry must not authorize again"))
            .unwrap();
        assert!(matches!(
            flow.state().state(),
            InstanceState::Open(InstanceActivity::Interrupted { .. })
        ));
        flow.accept_input(
            delivery(&description.address, user, "continue after failure"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(flow.state().can_start_turn());
    }
}

#[test]
fn session_removal_retains_unsettled_sends_until_acceptance_or_definitive_failure() {
    use atman_runtime::flow_store::{SessionRemoval, outbox::DeliveryFailure};
    for accepted in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let info = session_descriptor();
        let target = session_descriptor();
        let mut session = root.create_session(info.clone()).unwrap();
        let mut other = root.create_session(target.clone()).unwrap();
        let mut sender = session.create_flow(descriptor(info.id)).unwrap();
        let mut receiver = other.create_flow(descriptor(target.id)).unwrap();
        change(&mut sender, FlowTransition::Activated);
        change(&mut receiver, FlowTransition::Activated);
        let lease = start(&mut sender);
        let request = delivery(
            receiver.state().address(),
            SenderIdentity::Flow(sender.state().address().clone()),
            "retained until delivery settles",
        );
        sender
            .queue_send(&lease, request.clone(), None, |_| Ok(()))
            .unwrap();
        change(
            &mut sender,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        );
        change(&mut sender, FlowTransition::CloseRequested);
        change(&mut sender, FlowTransition::Closed);
        let id = sender.state().address().instance_id.clone();
        let journal = sender.dir().unwrap().join("events.jsonl");
        let original = std::fs::read(&journal).unwrap();
        let removal = SessionRemoval {
            session_id: info.id,
            expected: session.cursor().clone(),
        };
        drop((session, sender));
        assert!(
            root.delete_session(&removal)
                .unwrap_err()
                .to_string()
                .contains("pending deliveries")
        );
        assert_eq!(std::fs::read(&journal).unwrap(), original);
        assert!(
            RuntimeStore::inspect_session_removal(data.path(), info.id)
                .unwrap()
                .is_none()
        );
        let session = root.open_session(info.id).unwrap();
        let mut sender = session.open_flow(&id).unwrap();
        if accepted {
            let ack = receiver.accept_input(request.clone(), |_| Ok(())).unwrap();
            sender
                .confirm_delivery(request.request_id, DeliveryReceipt::from(&ack))
                .unwrap();
        } else {
            change(&mut receiver, FlowTransition::CloseRequested);
            sender
                .fail_delivery(
                    request.request_id,
                    DeliveryFailure::RecipientClosed {
                        observed: receiver.cursor().clone(),
                    },
                )
                .unwrap();
        }
        drop((session, sender));
        root.delete_session(&removal).unwrap();
        assert!(!journal.exists());
        assert_eq!(receiver.pending_inputs().count(), usize::from(accepted));
    }
}

#[test]
fn definitive_delivery_failure_is_atomic_terminal_and_replayable() {
    use atman_runtime::flow_store::outbox::DeliveryFailure;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut sender = session.create_flow(descriptor(info.id)).unwrap();
    let mut recipient = session.create_flow(descriptor(info.id)).unwrap();
    change(&mut sender, FlowTransition::Activated);
    change(&mut recipient, FlowTransition::Activated);
    let lease = start(&mut sender);
    let correlation = Uuid::new_v4();
    let waiter = ReplyWaiter {
        correlation,
        lease: lease.clone(),
        sender: SenderIdentity::Flow(recipient.state().address().clone()),
    };
    let mut request = delivery(
        recipient.state().address(),
        SenderIdentity::Flow(sender.state().address().clone()),
        "request",
    );
    request.correlation = Some(correlation);
    sender
        .queue_send(&lease, request.clone(), Some(waiter.clone()), |_| Ok(()))
        .unwrap();
    change(&mut recipient, FlowTransition::CloseRequested);
    let failure = DeliveryFailure::RecipientClosed {
        observed: recipient.cursor().clone(),
    };
    let mut foreign = recipient.cursor().clone();
    durable_position(&mut foreign).journal_id = Uuid::new_v4();
    let before = sender.cursor().clone();
    assert!(
        sender
            .fail_delivery(
                request.request_id,
                DeliveryFailure::RecipientClosed { observed: foreign }
            )
            .is_err()
    );
    assert_eq!(sender.cursor(), &before);
    let failed = sender
        .fail_delivery(request.request_id, failure.clone())
        .unwrap();
    let SendStatus::Failed { by, .. } = &failed.status else {
        panic!("expected terminal failure")
    };
    assert_eq!(
        sender.reply_waiters()[0].status,
        ReplyStatus::Cancelled { by: by.clone() }
    );
    assert_eq!(by.sequence(), before.sequence() + 1);
    assert!(sender.pending_sends().next().is_none());
    assert_eq!(
        sender
            .fail_delivery(request.request_id, failure.clone())
            .unwrap(),
        failed
    );
    change(&mut recipient, FlowTransition::Closed);
    assert_eq!(
        sender
            .fail_delivery(
                request.request_id,
                DeliveryFailure::RecipientClosed {
                    observed: recipient.cursor().clone()
                }
            )
            .unwrap(),
        failed
    );
    assert!(
        sender
            .confirm_delivery(
                request.request_id,
                DeliveryReceipt {
                    message_id: Uuid::new_v4(),
                    receipt: recipient.cursor().clone()
                }
            )
            .is_err()
    );
    let id = sender.state().address().instance_id.clone();
    drop(sender);
    let mut restored = session.open_flow(&id).unwrap();
    assert_eq!(restored.sends(), &[SendSummary::from(&failed)]);
    assert_eq!(
        restored.send_record(request.request_id).unwrap(),
        Some(failed.clone())
    );
    assert_eq!(
        restored.reply_waiters()[0].status,
        ReplyStatus::Cancelled { by: by.clone() }
    );
    assert_eq!(
        restored
            .queue_send(&lease, request, Some(waiter), |_| panic!(
                "retry must not create another intent"
            ))
            .unwrap(),
        failed
    );
    assert_eq!(
        restored
            .fail_delivery(failed.intent.request.request_id, failure)
            .unwrap(),
        failed
    );
}

#[test]
fn input_scope_invalidation_preserves_consumed_history_unrelated_input_and_retry_receipts() {
    use atman_runtime::flow_store::{
        inbox::InputScopeInvalidation,
        task_scope::{TaskScopeDefinition, TaskScopeRef},
        view::FlowUpdate,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut owner = session.create_flow(descriptor(info.id)).unwrap();
    let mut other = session.create_flow(descriptor(info.id)).unwrap();
    let mut recipient = session.create_flow(descriptor(info.id)).unwrap();
    for flow in [&mut owner, &mut other, &mut recipient] {
        change(flow, FlowTransition::Activated);
    }
    let definition = TaskScopeDefinition {
        id: Uuid::new_v4(),
        name: "Work".into(),
        goal: None,
    };
    owner.create_task_scope(definition.clone()).unwrap();
    other.create_task_scope(definition.clone()).unwrap();
    let scope = TaskScopeRef {
        owner: owner.state().address().clone(),
        id: definition.id,
    };
    let other_scope = TaskScopeRef {
        owner: other.state().address().clone(),
        id: definition.id,
    };
    let mut request = delivery(
        recipient.state().address(),
        SenderIdentity::Flow(owner.state().address().clone()),
        "scoped",
    );
    request.scope = Some(scope.clone());
    let consumed = recipient.accept_input(request.clone(), |_| Ok(())).unwrap();
    let reserved = recipient
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    let history_receipt = recipient
        .commit_input(&reserved, invocation_content(&lease))
        .unwrap();
    request.request_id = Uuid::new_v4();
    let pending = recipient.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut independent = request.clone();
    independent.request_id = Uuid::new_v4();
    independent.scope = Some(other_scope);
    let same_id_other_owner = recipient.accept_input(independent, |_| Ok(())).unwrap();
    let unscoped = recipient
        .accept_input(
            delivery(
                recipient.state().address(),
                request.sender.clone(),
                "independent",
            ),
            |_| Ok(()),
        )
        .unwrap();
    let reservation = recipient
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    owner.cancel_task_scope(scope.id).unwrap();
    let observation = InputScopeInvalidation {
        scope: scope.clone(),
        cancellation: owner
            .task_scopes()
            .next()
            .unwrap()
            .cancellation
            .clone()
            .unwrap(),
    };
    let state = recipient.state().clone();
    let before = recipient.cursor().clone();
    let receipt = recipient
        .invalidate_input_scope(observation.clone())
        .unwrap();
    assert_eq!(recipient.state(), &state);
    assert_eq!(recipient.committed_inputs()[0].receipt, history_receipt);
    assert_eq!(
        recipient.committed_inputs()[0].message_ids,
        [consumed.message_id]
    );
    assert!(
        recipient
            .commit_input(&reservation, context_content(&lease))
            .is_err()
    );
    assert_eq!(
        recipient
            .pending_inputs()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        [same_id_other_owner.message_id, unscoped.message_id]
    );
    let retry = recipient
        .accept_input(request.clone(), |_| {
            panic!("retry must not reauthorize new delivery")
        })
        .unwrap();
    assert_eq!(retry.receipt, pending.receipt);
    assert_eq!(
        retry.disposition,
        InboxDisposition::Cancelled {
            by: receipt.clone()
        }
    );
    assert_eq!(
        recipient
            .invalidate_input_scope(observation.clone())
            .unwrap(),
        receipt
    );
    assert_eq!(recipient.cursor(), &receipt);
    let mut conflicting = observation.clone();
    durable_position(&mut conflicting.cancellation.receipt).sequence += 1;
    assert!(recipient.invalidate_input_scope(conflicting).is_err());
    request.request_id = Uuid::new_v4();
    assert!(recipient.accept_input(request, |_| Ok(())).is_err());
    let page = recipient
        .read_updates(Some(&before), 10.try_into().unwrap())
        .unwrap();
    assert!(
        matches!(&page.batches[0].facts[..], [FlowUpdate::InputScopeInvalidated(record)] if **record == observation)
    );
    let id = recipient.state().address().instance_id.clone();
    let log = recipient.dir().unwrap().join("events.jsonl");
    let bytes = std::fs::read(&log).unwrap();
    drop(recipient);
    let inspection = session.inspect_flow(&id).unwrap();
    assert_eq!(inspection.pending_inputs, 2);
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
    let mut recipient = session.open_flow(&id).unwrap();
    assert_eq!(
        recipient
            .input_scope_invalidation(&scope)
            .unwrap()
            .observation,
        observation
    );
    assert_eq!(
        recipient.invalidate_input_scope(observation).unwrap(),
        receipt
    );
    assert_eq!(recipient.committed_inputs()[0].receipt, history_receipt);
    assert_eq!(
        recipient
            .pending_inputs()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        [same_id_other_owner.message_id, unscoped.message_id]
    );
}

#[test]
fn input_scope_replay_rejects_foreign_unknown_and_duplicate_cancellation_facts() {
    use atman_runtime::event_log::journal::Journal;
    use atman_runtime::flow_store::{
        inbox::InputScopeInvalidation,
        position::{FlowPosition, TransientPosition},
        task_scope::{TaskScopeDefinition, TaskScopeRef},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut owner = session.create_flow(descriptor(info.id)).unwrap();
    let mut recipient = session.create_flow(descriptor(info.id)).unwrap();
    change(&mut owner, FlowTransition::Activated);
    change(&mut recipient, FlowTransition::Activated);
    let definition = TaskScopeDefinition {
        id: Uuid::new_v4(),
        name: "Work".into(),
        goal: None,
    };
    owner.create_task_scope(definition.clone()).unwrap();
    let scope = TaskScopeRef {
        owner: owner.state().address().clone(),
        id: definition.id,
    };
    let mut request = delivery(
        recipient.state().address(),
        SenderIdentity::Flow(owner.state().address().clone()),
        "scoped",
    );
    request.scope = Some(TaskScopeRef {
        id: Uuid::nil(),
        ..scope.clone()
    });
    assert!(recipient.accept_input(request.clone(), |_| Ok(())).is_err());
    request.scope = Some(scope.clone());
    recipient.accept_input(request, |_| Ok(())).unwrap();
    owner.cancel_task_scope(scope.id).unwrap();
    let observation = InputScopeInvalidation {
        scope,
        cancellation: owner
            .task_scopes()
            .next()
            .unwrap()
            .cancellation
            .clone()
            .unwrap(),
    };
    let id = recipient.state().address().instance_id.clone();
    let path = recipient.dir().unwrap().join("events.jsonl");
    let accepted = std::fs::read(&path).unwrap();
    let mut transient = observation.clone();
    transient.cancellation.receipt = FlowPosition::Transient {
        transient: TransientPosition {
            host_generation: Uuid::now_v7(),
            flow_id: observation.scope.owner.instance_id.0.0,
            sequence: observation.cancellation.receipt.sequence(),
            digest: observation.cancellation.receipt.digest().into(),
        },
    };
    let before = recipient.cursor().clone();
    assert!(recipient.invalidate_input_scope(transient.clone()).is_err());
    assert_eq!(recipient.cursor(), &before);
    assert_eq!(std::fs::read(&path).unwrap(), accepted);
    drop(recipient);
    for case in 0..6 {
        std::fs::write(&path, &accepted).unwrap();
        let mut invalid = observation.clone();
        match case {
            0 => durable_position(&mut invalid.cancellation.receipt).journal_id = Uuid::new_v4(),
            1 => durable_position(&mut invalid.cancellation.receipt).sequence = 0,
            2 => invalid.scope.id = Uuid::new_v4(),
            5 => invalid = transient.clone(),
            _ => {}
        }
        let fact = serde_json::json!({"type": "input_scope_invalidated", "data": invalid});
        let (mut journal, _) =
            Journal::open::<serde_json::Value>(&path, id.0.0, |_, _| Ok(())).unwrap();
        if case == 3 {
            journal.commit(&[fact.clone(), fact]).unwrap();
        } else {
            journal.commit(std::slice::from_ref(&fact)).unwrap();
            if case == 4 {
                journal.commit(&[fact]).unwrap();
            }
        }
        drop(journal);
        let bytes = std::fs::read(&path).unwrap();
        let inspected = session.inspect_flow(&id).unwrap_err();
        let reopened = session.open_flow(&id).err().unwrap();
        assert_eq!(inspected.to_string(), reopened.to_string(), "case {case}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "case {case}");
    }
}

#[test]
fn task_scope_facts_bind_only_prepared_members_and_recover_independent_lifetimes() {
    use atman_runtime::flow_store::task_scope::{
        ScopeCancelReason, TaskScopeDefinition, TaskScopeRef,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut owner = session.create_flow(descriptor(info.id)).unwrap();
    let mut member = session.create_flow(descriptor(info.id)).unwrap();
    let definition = TaskScopeDefinition {
        id: Uuid::new_v4(),
        name: "Investigation".into(),
        goal: None,
    };
    assert!(owner.create_task_scope(definition.clone()).is_err());
    change(&mut owner, FlowTransition::Activated);
    let created = owner.create_task_scope(definition.clone()).unwrap();
    let scope = TaskScopeRef {
        owner: owner.descriptor().address.clone(),
        id: definition.id,
    };
    assert!(
        member
            .bind_task_scope(TaskScopeRef {
                owner: member.descriptor().address.clone(),
                ..scope.clone()
            })
            .is_err()
    );
    let bound = member.bind_task_scope(scope.clone()).unwrap();
    change(&mut member, FlowTransition::Activated);
    assert_eq!(member.bind_task_scope(scope.clone()).unwrap(), bound);
    assert!(
        member
            .bind_task_scope(TaskScopeRef {
                id: Uuid::new_v4(),
                ..scope.clone()
            })
            .is_err()
    );
    assert!(
        owner
            .bind_task_scope(TaskScopeRef {
                owner: member.descriptor().address.clone(),
                ..scope.clone()
            })
            .is_err()
    );
    assert_eq!(
        owner.create_task_scope(definition.clone()).unwrap(),
        created
    );
    assert!(
        owner
            .create_task_scope(TaskScopeDefinition {
                name: "different".into(),
                ..definition.clone()
            })
            .is_err()
    );
    let cancelled = owner.cancel_task_scope(definition.id).unwrap();
    let retry = owner.create_task_scope(definition.clone()).unwrap();
    assert_eq!(retry.receipt, created.receipt);
    assert_eq!(
        retry.cancellation.as_ref().unwrap().reason,
        ScopeCancelReason::Requested
    );
    let independent = TaskScopeDefinition {
        id: Uuid::new_v4(),
        name: "Independent".into(),
        goal: None,
    };
    owner.create_task_scope(independent.clone()).unwrap();
    assert_eq!(owner.cancel_task_scope(definition.id).unwrap(), cancelled);
    assert!(
        member.state().can_start_turn(),
        "storage replay does not perform host reconciliation"
    );
    let owner_id = owner.descriptor().address.instance_id.clone();
    let member_id = member.descriptor().address.instance_id.clone();
    drop(owner);
    drop(member);
    let inspected = session.inspect_flow(&member_id).unwrap();
    assert_eq!(inspected.task_scope_binding, Some(scope.clone()));
    let mut owner = session.open_flow(&owner_id).unwrap();
    let member = session.open_flow(&member_id).unwrap();
    assert_eq!(member.task_scope_binding(), Some(&scope));
    assert_eq!(owner.cancel_task_scope(definition.id).unwrap(), cancelled);
    change(&mut owner, FlowTransition::CloseRequested);
    let closed = owner.cursor().clone();
    assert_eq!(
        owner
            .create_task_scope(independent)
            .unwrap()
            .cancellation
            .unwrap()
            .reason,
        ScopeCancelReason::OwnerClosed
    );
    assert_eq!(owner.cancel_task_scope(definition.id).unwrap(), cancelled);
    drop(owner);
    let owner = session.open_flow(&owner_id).unwrap();
    assert_eq!(owner.cursor(), &closed);
    assert!(
        owner
            .task_scopes()
            .all(|record| record.cancellation.is_some())
    );
}

#[test]
fn goal_bound_task_scopes_cancel_on_changed_work_but_not_pause_or_resume() {
    use atman_runtime::flow_store::{
        goal::{GoalRequest, GoalVersion},
        task_scope::{ScopeCancelReason, ScopeGoal, TaskScopeDefinition},
    };
    use atman_runtime::memory::goal::{GoalActor, GoalEdit, GoalPauseReason};

    for edit in [
        GoalEdit::Set {
            text: "Other work".into(),
            replace_completed: false,
        },
        GoalEdit::Create {
            text: "Original work".into(),
        },
        GoalEdit::Clear,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let info = session_descriptor();
        let mut session = root.create_session(info.clone()).unwrap();
        let mut flow = session.create_flow(descriptor(info.id)).unwrap();
        change(&mut flow, FlowTransition::Activated);
        let request = |flow: &FlowStore, edit| GoalRequest {
            id: Uuid::new_v4(),
            sender: SenderIdentity::User(info.id),
            expected: flow.goal().map(GoalVersion::from),
            edit,
            start_turn: false,
        };
        flow.edit_goal(
            request(
                &flow,
                GoalEdit::Create {
                    text: "Original work".into(),
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
        let definition = TaskScopeDefinition {
            id: Uuid::new_v4(),
            name: "Bound".into(),
            goal: flow.goal().map(ScopeGoal::from),
        };
        let original = flow.create_task_scope(definition.clone()).unwrap();
        let independent = TaskScopeDefinition {
            id: Uuid::new_v4(),
            name: "Unbound".into(),
            goal: None,
        };
        flow.create_task_scope(independent.clone()).unwrap();
        for edit in [
            GoalEdit::Pause {
                actor: GoalActor::User,
                reason: GoalPauseReason::UserRequested,
                summary: String::new(),
            },
            GoalEdit::Resume {
                actor: GoalActor::User,
            },
            GoalEdit::Set {
                text: "Original work".into(),
                replace_completed: false,
            },
        ] {
            flow.edit_goal(request(&flow, edit), |_| Ok(())).unwrap();
            assert_eq!(
                flow.create_task_scope(definition.clone()).unwrap(),
                original
            );
        }
        let changed = flow.edit_goal(request(&flow, edit), |_| Ok(())).unwrap();
        let cancelled = flow.create_task_scope(definition.clone()).unwrap();
        assert_eq!(
            cancelled.cancellation.as_ref().unwrap().reason,
            ScopeCancelReason::GoalChanged
        );
        assert_eq!(
            cancelled.cancellation.as_ref().unwrap().receipt,
            changed.receipt
        );
        assert!(
            flow.create_task_scope(independent.clone())
                .unwrap()
                .cancellation
                .is_none()
        );
        assert!(
            flow.create_task_scope(TaskScopeDefinition {
                id: Uuid::new_v4(),
                ..definition.clone()
            })
            .is_err()
        );
        let id = flow.descriptor().address.instance_id.clone();
        drop(flow);
        let inspected = session.inspect_flow(&id).unwrap();
        assert!(inspected.task_scopes.contains(&cancelled));
        let mut flow = session.open_flow(&id).unwrap();
        assert_eq!(flow.create_task_scope(definition).unwrap(), cancelled);
        assert!(
            flow.create_task_scope(independent)
                .unwrap()
                .cancellation
                .is_none()
        );
    }
}

#[test]
fn durable_timers_commit_once_across_pause_recovery_and_cancellation() {
    use atman_runtime::flow_store::timer::{Timer, TimerStatus};
    use atman_runtime::flow_store::view::FlowUpdate;
    use chrono::{Duration, Utc};

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let id = description.address.instance_id.clone();
    let mut flow = session.create_flow(description).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let pause = Uuid::new_v4().into();
    change(&mut flow, FlowTransition::ExecutionPaused { key: pause });
    let at = Utc::now();
    let timers: Vec<_> = (0..3)
        .map(|offset| Timer {
            id: Uuid::new_v4(),
            due_at: at + Duration::seconds(offset),
            content: serde_json::json!({"offset": offset}),
        })
        .collect();
    for timer in &timers {
        flow.schedule_timer(timer.clone()).unwrap();
    }
    let before = flow.cursor().clone();
    assert_eq!(flow.next_timer_deadline(), Some(at));
    assert_eq!(
        flow.fire_due_timers(at - Duration::seconds(1)).unwrap(),
        before
    );
    drop(flow);
    let inspection = session.inspect_flow(&id).unwrap();
    assert_eq!(inspection.pending_inputs, 0);
    assert_eq!(inspection.timers.len(), 3);
    assert!(
        inspection
            .timers
            .iter()
            .all(|record| record.status == TimerStatus::Scheduled)
    );
    let mut flow = session.open_flow(&id).unwrap();
    assert_eq!(flow.cursor(), &before);
    assert_eq!(flow.pending_inputs().count(), 0);

    flow.cancel_timer(timers[2].id).unwrap();
    let before_delivery = flow.cursor().clone();
    let delivery = flow.fire_due_timers(at + Duration::seconds(10)).unwrap();
    assert!(!flow.state().can_start_turn());
    let pending: Vec<_> = common::pending_bodies(&mut flow);
    assert_eq!(pending.len(), 2);
    for (envelope, timer) in pending.iter().zip(&timers) {
        assert_eq!(
            flow.inbox_entry(envelope.id).unwrap().unwrap().envelope,
            *envelope
        );
        assert_eq!(
            envelope.request.payload,
            InboxPayload::Timer {
                timer: timer.id,
                due_at: timer.due_at,
                purpose: atman_runtime::flow_store::inbox::TimerPurpose::Notification,
                content: timer.content.clone()
            }
        );
        assert_ne!(envelope.request.request_id, timer.id);
    }
    let updates = flow
        .read_updates(Some(&before_delivery), 10.try_into().unwrap())
        .unwrap();
    assert_eq!(updates.batches.len(), 1);
    assert!(
        matches!(&updates.batches[0].facts[..], [FlowUpdate::TimersFired(batch)] if batch.envelopes == pending)
    );
    assert_eq!(
        flow.fire_due_timers(at + Duration::days(1)).unwrap(),
        delivery
    );
    assert_eq!(flow.cancel_timer(timers[0].id).unwrap(), delivery);
    assert_eq!(
        flow.schedule_timer(timers[0].clone()).unwrap().status,
        TimerStatus::Fired
    );
    assert_eq!(
        flow.schedule_timer(timers[2].clone()).unwrap().status,
        TimerStatus::Cancelled
    );
    let mut conflict = timers[0].clone();
    conflict.content = serde_json::json!("different");
    assert!(flow.schedule_timer(conflict).is_err());
    assert_eq!(flow.cursor(), &delivery);
    drop(flow);
    let mut flow = session.open_flow(&id).unwrap();
    assert_eq!(common::pending_bodies(&mut flow), pending);
    assert_eq!(flow.next_timer_deadline(), None);
    assert_eq!(
        flow.fire_due_timers(at + Duration::days(2)).unwrap(),
        delivery
    );
    change(&mut flow, FlowTransition::ExecutionResumed { key: pause });
    let remaining = Timer {
        id: Uuid::new_v4(),
        ..timers[0].clone()
    };
    flow.schedule_timer(remaining.clone()).unwrap();
    change(&mut flow, FlowTransition::CloseRequested);
    assert_eq!(
        flow.schedule_timer(remaining.clone()).unwrap().status,
        TimerStatus::Cancelled
    );
    assert_eq!(flow.next_timer_deadline(), None);
    assert!(flow.fire_due_timers(at + Duration::days(2)).is_err());
    assert!(
        flow.schedule_timer(Timer {
            id: Uuid::new_v4(),
            ..remaining
        })
        .is_err()
    );
    drop(flow);
    let flow = session.open_flow(&id).unwrap();
    assert_eq!(flow.next_timer_deadline(), None);
}

#[test]
fn durable_timers_replay_rejects_incomplete_or_mismatched_atomic_delivery() {
    use atman_runtime::event_log::journal::Journal;
    use atman_runtime::flow_store::timer::{Timer, TimerDelivery};
    use chrono::{Duration, Utc};

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let id = description.address.instance_id.clone();
    let mut flow = session.create_flow(description).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let at = Utc::now();
    for _ in 0..2 {
        flow.schedule_timer(Timer {
            id: Uuid::new_v4(),
            due_at: at,
            content: serde_json::json!("wake"),
        })
        .unwrap();
    }
    let path = flow.dir().unwrap().join("events.jsonl");
    let scheduled = std::fs::read(&path).unwrap();
    flow.fire_due_timers(at).unwrap();
    let delivery = TimerDelivery {
        at,
        envelopes: common::pending_bodies(&mut flow),
    };
    drop(flow);
    for case in 0..6 {
        std::fs::write(&path, &scheduled).unwrap();
        let mut invalid = delivery.clone();
        match case {
            0 => {
                invalid.envelopes.pop();
            }
            1 => {
                invalid.at -= Duration::seconds(1);
            }
            2 => {
                invalid.envelopes[0].id = invalid.envelopes[1].id;
            }
            3 => {
                invalid.envelopes[0].request.request_id = Uuid::new_v4();
            }
            4 => {
                invalid.envelopes[0].request.recipient.instance_id = FlowInstanceId::now();
            }
            _ => {}
        }
        let mut facts = vec![serde_json::json!({"type": "timers_fired", "data": invalid})];
        if case == 5 {
            facts.push(facts[0].clone());
        }
        let (mut journal, _) =
            Journal::open::<serde_json::Value>(&path, id.0.0, |_, _| Ok(())).unwrap();
        journal.commit(&facts).unwrap();
        drop(journal);
        let bytes = std::fs::read(&path).unwrap();
        let inspect = session.inspect_flow(&id).unwrap_err();
        let reopen = session.open_flow(&id).err().unwrap();
        assert_eq!(inspect.to_string(), reopen.to_string(), "case {case}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "case {case}");
    }
}

#[test]
fn flow_archive_rejects_incomplete_history_without_repair_or_catalog_writes() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let id = description.address.instance_id.clone();
    let mut flow = session.create_flow(description).unwrap();
    let expected = session.cursor().clone();
    assert!(
        session
            .set_flow_archived(&expected, id.clone(), true)
            .is_err()
    );
    change(&mut flow, FlowTransition::CloseRequested);
    change(&mut flow, FlowTransition::Closed);
    let path = flow.dir().unwrap().join("events.jsonl");
    assert!(
        session
            .set_flow_archived(&expected, id.clone(), true)
            .is_err(),
        "an unobserved live writer cannot supply authoritative closure through an unlocked scan"
    );
    assert_eq!(session.cursor(), &expected);
    drop(flow);
    let complete = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"unfinished")
        .unwrap();
    let damaged = std::fs::read(&path).unwrap();
    assert!(
        session
            .set_flow_archived(&expected, id.clone(), true)
            .is_err()
    );
    assert_eq!(session.cursor(), &expected);
    assert!(session.projection().archived_flows.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), damaged);
    std::fs::write(&path, complete).unwrap();
    session
        .set_flow_archived(&expected, id.clone(), true)
        .unwrap();
    let session_path = session.dir().to_path_buf();
    drop(session);
    std::fs::write(session_path.join("session.json"), "invalid cache").unwrap();
    let session = root.open_session(info.id).unwrap();
    assert!(session.projection().archived_flows.contains(&id));
    assert!(matches!(
        session.inspect_flow(&id).unwrap().state.state(),
        InstanceState::Closed
    ));
}

#[test]
fn operator_pause_release_is_selective_and_retains_retry_evidence_after_reacquisition() {
    use atman_runtime::flow_store::control::{ControlAction, ControlRequest};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let peer = SenderIdentity::Flow(description.address.clone());
    let user = SenderIdentity::User(Uuid::new_v4());
    let peer_key = Uuid::new_v4().into();
    let user_key = Uuid::new_v4().into();
    let request = |flow: &FlowStore, sender: SenderIdentity, pause_key, action| ControlRequest {
        id: Uuid::new_v4(),
        sender,
        target: description.address.clone(),
        expected_revision: flow.state().revision(),
        pause_key,
        action,
    };
    flow.control(request(&flow, peer.clone(), peer_key, ControlAction::Pause))
        .unwrap();
    let stale = request(&flow, user.clone(), peer_key, ControlAction::ReleasePause);
    flow.control(request(&flow, user.clone(), user_key, ControlAction::Pause))
        .unwrap();
    let cursor = flow.cursor().clone();
    assert!(flow.control(stale).is_err());
    assert_eq!(
        flow.control(request(
            &flow,
            peer.clone(),
            peer_key,
            ControlAction::ReleasePause
        ))
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(
        flow.control(request(
            &flow,
            user.clone(),
            Uuid::new_v4().into(),
            ControlAction::ReleasePause
        ))
        .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    let released = flow
        .control(request(
            &flow,
            user.clone(),
            peer_key,
            ControlAction::ReleasePause,
        ))
        .unwrap();
    assert_eq!(flow.state().pauses(), &[user_key]);
    flow.control(request(&flow, peer, peer_key, ControlAction::Pause))
        .unwrap();
    let cursor = flow.cursor().clone();
    assert_eq!(
        flow.control(released.update.request.clone()).unwrap(),
        released
    );
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.state().pauses().len(), 2);
    drop(flow);
    let mut restored = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(
        restored.control(released.update.request.clone()).unwrap(),
        released
    );
    assert_eq!(restored.cursor(), &cursor);
    assert_eq!(restored.state().pauses().len(), 2);
    restored
        .control(request(
            &restored,
            user,
            peer_key,
            ControlAction::ReleasePause,
        ))
        .unwrap();
    assert_eq!(restored.state().pauses(), &[user_key]);
    let inspected = RuntimeStore::inspect_session(dir.path(), info.id)
        .unwrap()
        .inspect_flow(&description.address.instance_id)
        .unwrap();
    assert_eq!(&inspected.state, restored.state());
}

#[test]
fn control_receipts_survive_later_turns_closure_and_recovery_without_reapplying() {
    use atman_runtime::flow_store::control::{ControlAction, ControlRequest};
    let dir = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = runtime.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let actor = SenderIdentity::User(Uuid::new_v4());
    let key = Uuid::new_v4().into();
    let request = |flow: &FlowStore, action| ControlRequest {
        id: Uuid::new_v4(),
        sender: actor.clone(),
        target: description.address.clone(),
        expected_revision: flow.state().revision(),
        pause_key: key,
        action,
    };
    let pause = flow.control(request(&flow, ControlAction::Pause)).unwrap();
    assert_eq!(pause.execution.pauses(), &[key]);
    let resume = flow.control(request(&flow, ControlAction::Resume)).unwrap();
    assert!(resume.execution.can_start_turn());
    let cursor = flow.cursor().clone();
    assert_eq!(flow.control(pause.update.request.clone()).unwrap(), pause);
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.state().can_start_turn());
    let mut conflicting = pause.update.request.clone();
    conflicting.action = ControlAction::Close;
    assert!(flow.control(conflicting).is_err());
    let mut stale = pause.update.request.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.control(stale).is_err());
    assert_eq!(flow.cursor(), &cursor);

    let noop = flow
        .control(request(&flow, ControlAction::CancelTurn))
        .unwrap();
    assert_eq!(noop.execution.revision(), resume.execution.revision());
    assert!(noop.receipt.sequence() > resume.receipt.sequence());
    let first = start(&mut flow);
    assert_eq!(flow.control(noop.update.request.clone()).unwrap(), noop);
    assert!(
        matches!(flow.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == first && active.phase == TurnPhase::Running)
    );
    let cancel = flow
        .control(request(&flow, ControlAction::CancelTurn))
        .unwrap();
    assert!(
        matches!(cancel.execution.state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == TurnPhase::Cancelling)
    );
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: first,
            outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
        },
    );
    flow.control(request(&flow, ControlAction::Recover))
        .unwrap();
    let second = start(&mut flow);
    assert_eq!(flow.control(cancel.update.request.clone()).unwrap(), cancel);
    assert!(
        matches!(flow.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == second && active.phase == TurnPhase::Running)
    );
    let close = flow.control(request(&flow, ControlAction::Close)).unwrap();
    assert!(matches!(
        close.execution.state(),
        InstanceState::Closing { active: Some(_) }
    ));
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: second,
            outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
        },
    );
    change(&mut flow, FlowTransition::Closed);
    let cursor = flow.cursor().clone();
    drop(flow);
    let mut restored = session.open_flow(&description.address.instance_id).unwrap();
    for record in [pause, resume, noop, cancel, close] {
        assert_eq!(
            restored.control(record.update.request.clone()).unwrap(),
            record
        );
    }
    assert_eq!(restored.cursor(), &cursor);
    assert_eq!(restored.state().state(), &InstanceState::Closed);
    let inspected = RuntimeStore::inspect_session(dir.path(), info.id)
        .unwrap()
        .inspect_flow(&description.address.instance_id)
        .unwrap();
    assert_eq!(&inspected.state, restored.state());
}

#[test]
fn control_noops_retain_input_reservations_and_invalid_requests_are_atomic() {
    use atman_runtime::flow_store::control::{ControlAction, ControlRequest};
    let dir = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = runtime.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let actor = SenderIdentity::User(Uuid::new_v4());
    flow.accept_input(
        delivery(&description.address, actor.clone(), "retained"),
        |_| Ok(()),
    )
    .unwrap();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let base = ControlRequest {
        id: Uuid::new_v4(),
        sender: actor,
        target: description.address.clone(),
        expected_revision: flow.state().revision(),
        pause_key: Uuid::new_v4().into(),
        action: ControlAction::CancelTurn,
    };
    let log = flow.dir().unwrap().join("events.jsonl");
    let before = std::fs::read(&log).unwrap();
    for variant in 0..3 {
        let mut invalid = base.clone();
        match variant {
            0 => invalid.id = Uuid::nil(),
            1 => invalid.pause_key = Uuid::nil().into(),
            _ => invalid.target = descriptor(info.id).address,
        }
        assert!(flow.control(invalid).is_err());
        assert_eq!(std::fs::read(&log).unwrap(), before);
    }
    flow.control(base.clone()).unwrap();
    let mut other_actor = base;
    other_actor.sender = SenderIdentity::User(Uuid::new_v4());
    let second = flow.control(other_actor).unwrap();
    assert_eq!(second.execution.revision(), flow.state().revision());
    flow.commit_input(
        &batch,
        InputContent::InvocationInput {
            command: None,
            arguments: BTreeMap::new(),
            invocation: BTreeMap::new(),
            messages: vec![],
        },
    )
    .unwrap();
    assert!(flow.pending_inputs().next().is_none());
}

#[tokio::test]
async fn entry_exit_fences_hook_input_and_resumes_one_pending_batch() {
    use atman_runtime::{Value, flow_runtime::FlowWorker, message::MessageOrigin};
    use std::{sync::Arc, time::Duration};
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::mock_model("mock")])).await;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"flow main(input: int) -> string {
            answer = deliver(input).await
            values = fanout [helper(), helper()]
            when values[0] != 1 || values[1] != 1 { return "invalid fanout result" }
            execution_probe()
            return answer
        }
        flow deliver(input: int) -> string {
            return final.answer(message: to_json_string(input))
        }
        flow helper() -> int { return 1 }"#
            .into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let mut executor = atman_runtime::Executor::new();
    let mut hooks = atman_runtime::lifecycle::LifecycleRunner::new();
    hooks.absorb(&atman_rt::parse_file(r#"on turn.end {
        execution_probe()
        llm.call(model: "mock", prompt: "summarize the completed turn", context: "session", record: false)
    }"#).unwrap());
    executor.tool_ctx.lifecycle_hooks = Some(Arc::new(hooks));
    let provider = Arc::new(InboxRecordingProvider {
        requests: Default::default(),
        inner: atman_runtime::providers::mock::MockProvider::new("mock")
            .with_fallback(Value::Str("summary".into())),
    });
    executor.providers.register(provider.clone());
    let probe = Arc::new(ExecutionProbe {
        entered: Arc::new(tokio::sync::Semaphore::new(0)),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let scheduler = worker
        .schedule(executor, |snapshot| {
            let mut request = turn_request(snapshot.pending[0].id, snapshot.pending.len() as i64);
            request.messages = snapshot.pending.iter().map(|input| input.id).collect();
            request.context = snapshot
                .pending
                .iter()
                .map(|input| {
                    atman_runtime::flow_runtime::input::context_message(input, &request.turn)
                })
                .collect::<Result<_, _>>()?;
            Ok(Some(request))
        })
        .unwrap();
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "initial",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let running = worker.state().await.unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = running.state() else {
        panic!("nested returns must retain the active entry")
    };
    assert_eq!(active.phase, TurnPhase::Running);
    let lease = active.lease.clone();
    assert!(
        worker
            .completion(Some(lease.turn.clone()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        worker
            .context()
            .messages()
            .iter()
            .filter(|message| message.origin == MessageOrigin::FinalAnswer)
            .count(),
        1
    );
    // Publication and nested flow returns do not finish the entry or release its lease.
    probe.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        matches!(worker.state().await.unwrap().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == TurnPhase::Finishing && active.lease == lease)
    );
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        worker.accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(descriptor(info.id).address),
                "peer-left"
            ),
            |_| Ok(())
        ),
        worker.accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(descriptor(info.id).address),
                "peer-right"
            ),
            |_| Ok(())
        ),
    );
    let expected = [left.unwrap().message_id, right.unwrap().message_id];
    probe.release.add_permits(1);
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if matches!(
                worker.state().await.unwrap().state(),
                InstanceState::Open(InstanceActivity::Idle)
            ) {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let snapshot = worker.snapshot().await.unwrap();
    assert_eq!(snapshot.pending.len(), 2);
    assert!(
        expected
            .iter()
            .all(|id| snapshot.pending.iter().any(|input| &input.id == id))
    );
    assert_eq!(snapshot.state.pauses(), &[key]);
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| message.text_concat().contains("peer-"))
        );
    }
    worker
        .transition(
            snapshot.state.revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    probe.release.add_permits(2);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.pending.is_empty() && snapshot.state.can_start_turn() {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    scheduler.stop().await.unwrap();
    let mut store = worker.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 2);
    assert_eq!(store.committed_inputs()[1].message_ids.len(), 2);
    assert_eq!(store.completions().len(), 2);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let answers: Vec<_> = store
        .context_messages()
        .unwrap()
        .iter()
        .filter(|message| message.origin == MessageOrigin::FinalAnswer)
        .map(Message::text_concat)
        .collect();
    assert_eq!(answers, ["1", "2"]);
    let history = store.context_messages().unwrap();
    drop(store);
    let mut restored = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(restored.context_messages().unwrap(), history);
    assert_eq!(restored.committed_inputs().len(), 2);
    assert!(restored.pending_inputs().next().is_none());
}

#[tokio::test]
async fn activity_is_derived_from_the_same_commits_in_live_inspection_and_recovery() {
    use atman_runtime::{
        event::Event,
        flow_store::trace::{ExecutionEvent, ExecutionScope},
    };
    let dir = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let mut session = runtime.create_session(session_descriptor()).unwrap();
    let description = descriptor(session.projection().descriptor.id);
    let address = description.address.clone();
    let mut flow = session.create_flow(description).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    assert!(
        flow.activity().is_none(),
        "Untimed state changes cannot invent a timestamp"
    );
    let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let event = |sequence, timestamp| ExecutionEvent {
        sequence,
        timestamp,
        scope: ExecutionScope::Invocation(lease.clone()),
        event: Event::FlowStart {
            run_id: lease.run.clone(),
            flow_name: "worker".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        },
    };
    let receipt = flow.append_execution_events(vec![event(1, at)]).unwrap();
    let expected = flow.activity().unwrap().clone();
    assert_eq!(expected.at, at);
    assert_eq!(expected.receipt, receipt);
    flow.append_execution_events(vec![event(2, at - chrono::Duration::seconds(10))])
        .unwrap();
    assert_eq!(
        flow.activity(),
        Some(&expected),
        "Late older diagnostics must not move activity backwards"
    );
    assert!(
        flow.append_execution_events(vec![event(2, at + chrono::Duration::hours(1))])
            .is_err()
    );
    assert_eq!(
        flow.activity(),
        Some(&expected),
        "Rejected writes cannot advance activity"
    );
    let before = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    let inspected = session.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspected.activity, Some(expected.clone()));
    assert_eq!(
        before,
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap()
    );
    drop(flow);
    let recovered = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(recovered.activity(), Some(&expected));
    assert_eq!(
        session.inspect_flow(&address.instance_id).unwrap().activity,
        Some(expected.clone())
    );
    drop(recovered);
    let host = atman_runtime::flow_runtime::host::RuntimeHost::default();
    let owner = host.attach_session(session).unwrap();
    let detached = owner
        .describe_flow(address.instance_id.clone())
        .await
        .unwrap();
    assert!(!detached.loaded);
    assert_eq!(detached.activity, Some(expected.clone()));
    let _worker = host.flow(&address).await.unwrap();
    let live = owner
        .describe_flow(address.instance_id.clone())
        .await
        .unwrap();
    assert!(live.loaded);
    assert_eq!(live.activity, Some(expected));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn inbox_history_pages_preserve_delivery_identity_without_consumption_or_execution() {
    use atman_runtime::{
        flow_runtime::{catalog::SessionCatalog, host::RuntimeHost},
        flow_store::{
            inbox::{InboxChange, InboxEditRequest},
            inbox_view::{InboxHistoryItem, InboxSide},
        },
    };
    use std::{cell::Cell, io::Write};
    let dir = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let session_info = session_descriptor();
    let mut session = runtime.create_session(session_info.clone()).unwrap();
    let description = descriptor(session_info.id);
    let address = description.address.clone();
    let mut flow = session.create_flow(description).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let user = SenderIdentity::User(Uuid::new_v4());
    let mut accepted = vec![];
    for text in ["first", "second", "third"] {
        accepted.push(
            flow.accept_input(delivery(&address, user.clone(), text), |_| Ok(()))
                .unwrap(),
        );
    }
    let read = RuntimeStore::inspect_session(dir.path(), session_info.id).unwrap();
    let first = read
        .inspect_inbox(&address.instance_id, InboxSide::Received, None, 2)
        .unwrap();
    assert_eq!(first.total, 3);
    let ids = |page: &atman_runtime::flow_store::inbox_view::InboxHistoryPage| {
        page.items
            .iter()
            .map(|item| match item {
                InboxHistoryItem::Received(entry) => entry.envelope.id,
                InboxHistoryItem::Sent(record) => record.intent.request.request_id,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&first),
        [accepted[2].message_id, accepted[1].message_id]
    );
    let anchor = first.next.clone().unwrap();
    flow.edit_input(
        InboxEditRequest {
            id: Uuid::new_v4(),
            sender: user.clone(),
            message: accepted[1].message_id,
            expected: accepted[1].receipt.clone(),
            change: InboxChange::Cancel,
        },
        |_| Ok(()),
    )
    .unwrap();
    flow.accept_input(delivery(&address, user, "later"), |_| Ok(()))
        .unwrap();
    let older = read
        .inspect_inbox(&address.instance_id, InboxSide::Received, Some(&anchor), 2)
        .unwrap();
    assert_eq!(older.total, 4);
    assert_eq!(ids(&older), [accepted[0].message_id]);
    assert!(older.next.is_none());
    assert!(older.cursor.sequence() > first.cursor.sequence());
    let fresh = read
        .inspect_inbox(&address.instance_id, InboxSide::Received, None, 50)
        .unwrap();
    assert!(
        matches!(&fresh.items[2], InboxHistoryItem::Received(entry) if matches!(entry.acceptance.disposition, InboxDisposition::Cancelled { .. }))
    );
    for limit in [0, 51] {
        assert!(
            read.inspect_inbox(&address.instance_id, InboxSide::Received, None, limit)
                .is_err()
        );
    }
    for field in 0..4 {
        let mut invalid = anchor.clone();
        match field {
            0 => invalid.owner.session_id = Uuid::new_v4().into(),
            1 => invalid.side = InboxSide::Sent,
            2 => durable_position(&mut invalid.receipt).digest.push('x'),
            _ => invalid.id = Uuid::new_v4(),
        }
        assert!(
            read.inspect_inbox(&address.instance_id, InboxSide::Received, Some(&invalid), 2)
                .is_err()
        );
    }
    assert!(
        read.inspect_inbox(&address.instance_id, InboxSide::Sent, Some(&anchor), 2)
            .is_err()
    );
    let lease = flow
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    let outgoing = delivery(
        &address,
        SenderIdentity::Flow(address.clone()),
        "self delivery",
    );
    let queued = flow
        .queue_send(&lease, outgoing.clone(), None, |_| Ok(()))
        .unwrap();
    let pending = read
        .inspect_inbox(&address.instance_id, InboxSide::Sent, None, 1)
        .unwrap();
    assert!(
        matches!(&pending.items[0], InboxHistoryItem::Sent(record) if record.status == SendStatus::PendingDelivery)
    );
    let recipient = flow.accept_input(outgoing.clone(), |_| Ok(())).unwrap();
    flow.confirm_delivery(outgoing.request_id, DeliveryReceipt::from(&recipient))
        .unwrap();
    let outgoing2 = delivery(
        &address,
        SenderIdentity::Flow(address.clone()),
        "not delivered",
    );
    flow.queue_send(&lease, outgoing2.clone(), None, |_| Ok(()))
        .unwrap();
    let sent = read
        .inspect_inbox(&address.instance_id, InboxSide::Sent, None, 1)
        .unwrap();
    assert_eq!(ids(&sent), [outgoing2.request_id]);
    let prior = read
        .inspect_inbox(&address.instance_id, InboxSide::Sent, sent.next.as_ref(), 1)
        .unwrap();
    assert!(
        matches!(&prior.items[0], InboxHistoryItem::Sent(record) if matches!(&record.status, SendStatus::Accepted { delivery, .. } if delivery.message_id == recipient.message_id) && record.queued == queued.queued)
    );
    let reserved = flow
        .reserve_context_input(&lease, |input| input.id == accepted[0].message_id)
        .unwrap()
        .unwrap();
    let during = read
        .inspect_inbox(&address.instance_id, InboxSide::Received, None, 50)
        .unwrap();
    assert!(during.items.iter().any(|item| matches!(item, InboxHistoryItem::Received(entry) if entry.envelope.id == accepted[0].message_id && entry.acceptance.disposition == InboxDisposition::Pending)));
    let committed = flow
        .commit_input(&reserved, context_content(&lease))
        .unwrap();
    let consumed = read
        .inspect_inbox(&address.instance_id, InboxSide::Received, Some(&anchor), 2)
        .unwrap();
    assert!(
        matches!(&consumed.items[0], InboxHistoryItem::Received(entry) if matches!(&entry.acceptance.disposition, InboxDisposition::InputCommitted { by } if by == &committed))
    );
    let host = RuntimeHost::default();
    let catalog = SessionCatalog::new(Some(host.clone()), Some(dir.path().to_owned()));
    let state = flow.state().clone();
    let original_pending = flow
        .pending_inputs()
        .map(|item| item.id)
        .collect::<Vec<_>>();
    let bytes = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    let checks = Cell::new(0);
    let page = catalog
        .inbox_page(&address, InboxSide::Received, None, 50, |owner| {
            assert_eq!(owner, &address);
            checks.set(checks.get() + 1);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(checks.get(), 3);
    assert_eq!(page.total, 5);
    assert!(host.sessions().is_empty());
    assert_eq!(flow.state(), &state);
    assert_eq!(
        flow.pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        original_pending
    );
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        bytes
    );
    checks.set(0);
    let denied = catalog
        .inbox_page(&address, InboxSide::Received, None, 1, |_| {
            checks.set(checks.get() + 1);
            if checks.get() == 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        })
        .await;
    assert!(matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    let path = flow.dir().unwrap().join("events.jsonl");
    drop(flow);
    drop(session);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{partial")
        .unwrap();
    let partial = std::fs::read(&path).unwrap();
    let detached = catalog
        .inbox_page(&address, InboxSide::Received, None, 50, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(detached.trailing_bytes, 8);
    assert_eq!(detached.cursor, page.cursor);
    assert_eq!(std::fs::read(&path).unwrap(), partial);
    assert!(host.sessions().is_empty());
    std::fs::write(&path, b"corrupt\n").unwrap();
    assert!(
        catalog
            .inbox_page(&address, InboxSide::Received, None, 1, |_| Ok(()))
            .await
            .is_err()
    );
    assert!(read.inspect_flow(&address.instance_id).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"corrupt\n");
}

#[test]
fn session_titles_replay_independently_of_creation_and_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let descriptor = session_descriptor();
    let mut session = root.create_session(descriptor.clone()).unwrap();
    let original = session.cursor().clone();
    let renamed = session
        .set_title(&original, Some("Renamed".into()))
        .unwrap();
    assert_eq!(session.projection().descriptor, descriptor);
    assert_eq!(session.projection().title.as_deref(), Some("Renamed"));
    assert_eq!(
        session
            .set_title(&original, Some("Renamed".into()))
            .unwrap(),
        renamed
    );
    assert!(session.set_title(&original, Some("Stale".into())).is_err());
    let cleared = session.set_title(&renamed, None).unwrap();
    assert!(session.projection().title.is_none());
    assert!(
        session
            .set_title(&original, Some("Renamed".into()))
            .is_err()
    );
    let path = session.dir().to_owned();
    let facts = std::fs::read(path.join("events.jsonl")).unwrap();
    // A disposable cache is never the source of the current name.
    std::fs::write(path.join("session.json"), b"{\"title\":\"stale\"}").unwrap();
    let inspected = RuntimeStore::inspect_session(dir.path(), descriptor.id).unwrap();
    assert!(inspected.projection.title.is_none());
    assert_eq!(inspected.projection.descriptor, descriptor);
    assert_eq!(inspected.journal.cursor, Some(cleared.clone()));
    assert_eq!(std::fs::read(path.join("events.jsonl")).unwrap(), facts);
    assert_eq!(
        std::fs::read(path.join("session.json")).unwrap(),
        b"{\"title\":\"stale\"}"
    );
    drop(session);
    let reopened = root.open_session(descriptor.id).unwrap();
    assert!(reopened.projection().title.is_none());
    assert_eq!(reopened.projection().descriptor, descriptor);
    assert_eq!(reopened.cursor(), &cleared);
}

#[test]
fn project_binding_retains_scope_without_recreating_missing_paths_or_accepting_invalid_config() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::flow_store::SessionConfiguration;
    use atman_runtime::storage::{ProjectContext, StorageScope};
    let data = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_config_dir(config.path());
    let config_path = config.path().join("config.toml");
    std::fs::write(&config_path, "[storage]\nscope = 'local'\n").unwrap();
    let local = ProjectContext::resolve(&hub, project.path(), data.path()).unwrap();
    assert_eq!(local.scope(), StorageScope::Local);
    assert_eq!(local.storage_root(), local.root().join(".atman"));
    assert!(!local.storage_root().exists());
    let mut info = session_descriptor();
    info.project = Some(local.clone());
    info.configuration = SessionConfiguration::Atman(hub.source());
    let root = RuntimeStore::open(data.path()).unwrap();
    drop(root.create_session(info.clone()).unwrap());
    std::fs::write(&config_path, "[storage]\nscope = 'global'\n").unwrap();
    let global = ProjectContext::resolve(&hub, project.path(), data.path()).unwrap();
    assert_eq!(global.scope(), StorageScope::Global);
    assert_ne!(local.storage_root(), global.storage_root());
    assert!(!global.storage_root().exists());
    project.close().unwrap();
    let store = root.open_session(info.id).unwrap();
    assert_eq!(store.projection().descriptor.project.as_ref(), Some(&local));
    assert!(!local.root().exists());
    assert!(!local.storage_root().exists());

    let mut invalid = serde_json::to_value(&info).unwrap();
    let mut missing = invalid.clone();
    missing.as_object_mut().unwrap().remove("project");
    assert!(serde_json::from_value::<SessionDescriptor>(missing).is_err());
    invalid["id"] = serde_json::json!(Uuid::new_v4());
    invalid["project"]["storage_root"] = "relative".into();
    let invalid: SessionDescriptor = serde_json::from_value(invalid).unwrap();
    assert!(root.create_session(invalid.clone()).is_err());
    assert!(
        !root
            .root()
            .join("sessions")
            .join(invalid.id.to_string())
            .exists()
    );
    std::fs::write(&config_path, "[storage]\nscope = 'invalid'\n").unwrap();
    assert!(ProjectContext::resolve(&hub, local.root(), data.path()).is_err());
    // An unreadable configuration path is not an absent configuration file.
    std::fs::remove_file(&config_path).unwrap();
    std::fs::create_dir(&config_path).unwrap();
    assert!(ProjectContext::resolve(&hub, local.root(), data.path()).is_err());
    let profile = atman_runtime::sandbox::SandboxExec::without_project()
        .with_template("{PROJECT_ROOT}|{CWD}")
        .render_profile(std::path::Path::new("/explicit/operation"));
    assert_eq!(profile, "/explicit/operation|/explicit/operation");
}

#[cfg(unix)]
#[test]
fn recorded_project_locations_reject_replaced_directory_aliases() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::storage::ProjectContext;
    let data = tempfile::tempdir().unwrap();
    let project = data.path().join("project");
    let replacement = data.path().join("replacement");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    let binding = ProjectContext::resolve(
        &ConfigHub::from_config_dir(data.path()),
        &project,
        data.path(),
    )
    .unwrap();
    binding.validate_locations().unwrap();
    std::fs::remove_dir(&project).unwrap();
    std::os::unix::fs::symlink(&replacement, &project).unwrap();
    assert!(binding.validate_locations().is_err());
    assert!(!binding.storage_root().exists());
}

#[cfg(unix)]
#[test]
fn configuration_reopen_rejects_a_directory_replaced_by_an_alias() {
    use atman_runtime::config_hub::ConfigHub;
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("config");
    let replacement = dir.path().join("other");
    std::fs::create_dir_all(&original).unwrap();
    std::fs::create_dir_all(&replacement).unwrap();
    let source = ConfigHub::from_auth_path(original.join("selected.json")).source();
    let encoded = serde_json::to_string(&source).unwrap();
    let restored = serde_json::from_str(&encoded).unwrap();
    assert_eq!(ConfigHub::from_source(&restored).unwrap().source(), source);
    std::fs::remove_dir(&original).unwrap();
    std::os::unix::fs::symlink(&replacement, &original).unwrap();
    assert!(
        ConfigHub::from_source(&restored)
            .unwrap_err()
            .to_string()
            .contains("location has changed")
    );
    assert!(!replacement.join(".config.toml.lock").exists());
}

#[test]
fn session_project_changes_capture_registration_scope_without_rewriting_existing_instances() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::storage::ProjectContext;

    let data = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_config_dir(data.path().join("config"));
    let alpha = ProjectContext::resolve(&hub, first.path(), data.path()).unwrap();
    let beta = ProjectContext::resolve(&hub, second.path(), data.path()).unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut info = session_descriptor();
    info.project = Some(alpha.clone());
    let mut session = root.create_session(info.clone()).unwrap();
    let old_descriptor = descriptor(info.id);
    let old = session.create_flow(old_descriptor.clone()).unwrap();
    let old_cursor = old.cursor().clone();
    let version = session.cursor().clone();
    let moved = session.set_project(&version, Some(beta.clone())).unwrap();
    assert_eq!(
        session.set_project(&version, Some(beta.clone())).unwrap(),
        moved
    );
    assert!(session.set_project(&version, None).is_err());
    assert_eq!(session.projection().descriptor, info);
    assert_eq!(old.project(), Some(&alpha));
    assert_eq!(old.cursor(), &old_cursor);

    let new_descriptor = descriptor(info.id);
    let new = session.create_flow(new_descriptor.clone()).unwrap();
    assert_eq!(new.project(), Some(&beta));
    let current = session.cursor().clone();
    session.set_project(&current, None).unwrap();
    let unbound_descriptor = descriptor(info.id);
    let unbound = session.create_flow(unbound_descriptor.clone()).unwrap();
    assert!(unbound.project().is_none());
    let expected = session.cursor().clone();
    let dir = session.dir().to_owned();
    drop((old, new, unbound, session));
    first.close().unwrap();
    second.close().unwrap();
    std::fs::write(dir.join("session.json"), b"discarded cache").unwrap();
    let inspected = RuntimeStore::inspect_session(data.path(), info.id).unwrap();
    assert!(inspected.projection.project.is_none());
    assert_eq!(
        std::fs::read(dir.join("session.json")).unwrap(),
        b"discarded cache"
    );
    let mut session = root.open_session(info.id).unwrap();
    assert_eq!(session.cursor(), &expected);
    assert_eq!(session.projection().descriptor, info);
    for (descriptor, project) in [
        (old_descriptor, Some(alpha)),
        (new_descriptor, Some(beta)),
        (unbound_descriptor, None),
    ] {
        let id = descriptor.address.instance_id.clone();
        let opened = session.open_flow(&id).unwrap();
        assert_eq!(opened.project(), project.as_ref());
        assert_eq!(inspected.inspect_flow(&id).unwrap().project, project);
        drop(opened);
        assert_eq!(
            session.create_flow(descriptor).unwrap().project(),
            project.as_ref()
        );
    }
    assert_eq!(session.cursor(), &expected);
}

#[test]
fn configuration_source_replays_without_accessing_project_or_credential_files() {
    use atman_runtime::config_hub::{ConfigHub, ConfigSource};
    use atman_runtime::flow_store::SessionConfiguration;
    let data = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = ConfigHub::from_auth_path(config.path().join("selected-account.json")).source();
    std::fs::write(source.auth_path(), "private-credential-test-value").unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut info = session_descriptor();
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(
            &ConfigHub::from_source(&source).unwrap(),
            project.path(),
            data.path(),
        )
        .unwrap(),
    );
    info.configuration = SessionConfiguration::Atman(source.clone());
    let mut store = root.create_session(info.clone()).unwrap();
    let flow = descriptor(info.id);
    let instance = store.create_flow(flow.clone()).unwrap();
    assert_eq!(instance.project(), info.project.as_ref());
    assert_eq!(
        instance.source_roots().project_root,
        info.source_roots().project_root
    );
    assert_eq!(store.projection().flows[0].project, info.project);
    assert_eq!(
        store
            .inspect_flow(&flow.address.instance_id)
            .unwrap()
            .project,
        info.project
    );
    drop(instance);
    let journal = std::fs::read_to_string(store.dir().join("events.jsonl")).unwrap();
    assert!(journal.contains("selected-account.json"));
    assert!(!journal.contains("private-credential-test-value"));
    drop(store);
    config.close().unwrap();
    project.close().unwrap();
    let restored = root.open_session(info.id).unwrap();
    assert_eq!(&restored.projection().descriptor, &info);
    assert_eq!(restored.projection().flows[0].project, info.project);
    let inspection = RuntimeStore::inspect_session(data.path(), info.id).unwrap();
    assert_eq!(
        inspection
            .inspect_flow(&flow.address.instance_id)
            .unwrap()
            .project,
        info.project
    );
    assert_eq!(
        restored
            .open_flow(&flow.address.instance_id)
            .unwrap()
            .project(),
        info.project.as_ref()
    );
    assert_eq!(
        restored
            .open_flow(&flow.address.instance_id)
            .unwrap()
            .session_descriptor(),
        &info
    );
    assert!(!source.config_dir().exists());

    let invalid_source: ConfigSource = serde_json::from_value(serde_json::json!({
        "config_dir": "relative", "auth_path": "relative/auth.json"
    }))
    .unwrap();
    let mut invalid = info.clone();
    invalid.id = Uuid::new_v4();
    invalid.configuration = SessionConfiguration::Atman(invalid_source);
    assert!(root.create_session(invalid.clone()).is_err());
    assert!(
        !root
            .root()
            .join("sessions")
            .join(invalid.id.to_string())
            .exists()
    );
    let mut missing = serde_json::to_value(&info).unwrap();
    missing.as_object_mut().unwrap().remove("configuration");
    assert!(serde_json::from_value::<SessionDescriptor>(missing).is_err());
}

#[test]
fn planning_commits_replay_exactly_and_reconcile_retries_without_rolling_back_state() {
    use atman_runtime::flow_store::planning::{PlanningEdit, PlanningOutcome, PlanningRequest};
    use atman_runtime::memory::todo::TodoStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut flow = session.create_flow(descriptor(info.id)).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let state = flow.state().clone();
    let sender = SenderIdentity::User(Uuid::new_v4());
    let request = |edit| PlanningRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        expected_revision: None,
        edit,
    };
    let add = request(PlanningEdit::AddTodo {
        where_: "src".into(),
        why: "verify isolation".into(),
        how: "shared tool registry".into(),
        expected_result: "independent records".into(),
    });
    let accepted = flow.edit_planning(add.clone(), |_| Ok(())).unwrap();
    let PlanningOutcome::Todo(id) = &accepted.outcome else {
        panic!("todo id");
    };
    let done = request(PlanningEdit::SetTodoStatus {
        id: id.clone(),
        status: TodoStatus::Done,
    });
    flow.edit_planning(done, |_| Ok(())).unwrap();
    let write = |title: &str| {
        request(PlanningEdit::WritePlan {
            id: "route".into(),
            title: title.into(),
            steps: vec!["verify".into(), "deliver".into()],
        })
    };
    let first_plan = flow.edit_planning(write("original"), |_| Ok(())).unwrap();
    let PlanningOutcome::Plan(first_plan) = first_plan.outcome else {
        panic!("plan");
    };
    flow.edit_planning(write("updated"), |_| Ok(())).unwrap();
    let tick = request(PlanningEdit::TickPlan {
        id: None,
        step_index: 0,
    });
    flow.edit_planning(tick.clone(), |_| Ok(())).unwrap();
    let expected = flow.planning().clone();
    assert_eq!(expected.todos[0].status, TodoStatus::Done);
    assert_eq!(expected.plans[0].created_at, first_plan.created_at);
    assert_eq!(expected.plans[0].progress(), (1, 2));
    assert_eq!(expected.revision, 5);
    assert_eq!(flow.state(), &state, "planning is not execution control");
    let cursor = flow.cursor().clone();
    assert_eq!(
        flow.edit_planning(add.clone(), |_| panic!("retry authorization"))
            .unwrap(),
        accepted
    );
    assert_eq!(flow.planning(), &expected);
    let mut conflict = add.clone();
    conflict.edit = PlanningEdit::DeleteTodo { id: id.clone() };
    assert!(flow.edit_planning(conflict, |_| Ok(())).is_err());
    let mut stale = write("stale");
    stale.expected_revision = Some(0);
    assert!(flow.edit_planning(stale, |_| Ok(())).is_err());
    assert!(
        flow.edit_planning(
            request(PlanningEdit::TickPlan {
                id: None,
                step_index: 8
            }),
            |_| Ok(())
        )
        .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    let address = flow.descriptor().address.clone();
    let log = flow.dir().unwrap().join("events.jsonl");
    flow.edit_planning(request(PlanningEdit::DeleteTodo { id: id.clone() }), |_| {
        Ok(())
    })
    .unwrap();
    drop(flow);
    // A partially written final batch must not delete the acknowledged todo.
    let length = std::fs::metadata(&log).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&log)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(flow.planning(), &expected);
    assert_eq!(
        flow.edit_planning(add.clone(), |_| panic!("reopened retry"))
            .unwrap(),
        accepted
    );
    let mut clear = request(PlanningEdit::ClearTodos);
    clear.expected_revision = Some(expected.revision);
    let cleared = flow.edit_planning(clear.clone(), |_| Ok(())).unwrap();
    assert_eq!(cleared.outcome, PlanningOutcome::TodosCleared);
    assert!(flow.planning().todos.is_empty());
    assert_eq!(flow.planning().plans, expected.plans);
    assert_eq!(flow.state(), &state);
    let mut next = add.clone();
    next.id = Uuid::new_v4();
    flow.edit_planning(next, |_| Ok(())).unwrap();
    let after_clear = flow.planning().clone();
    let mut stale = clear.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.edit_planning(stale, |_| Ok(())).is_err());
    assert_eq!(
        flow.edit_planning(clear.clone(), |_| panic!("clear retry"))
            .unwrap(),
        cleared
    );
    assert_eq!(
        flow.planning(),
        &after_clear,
        "retry must not erase subsequently added todos"
    );
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(flow.planning(), &after_clear);
    assert_eq!(
        flow.edit_planning(clear, |_| panic!("reopened clear retry"))
            .unwrap(),
        cleared
    );
    change(&mut flow, FlowTransition::CloseRequested);
    change(&mut flow, FlowTransition::Closed);
    assert_eq!(
        flow.edit_planning(add, |_| panic!("closed retry")).unwrap(),
        accepted
    );
    assert!(flow.edit_planning(write("closed"), |_| Ok(())).is_err());
    assert_eq!(flow.planning(), &after_clear);
}

fn turn_request(
    message: Uuid,
    argument: i64,
) -> atman_runtime::flow_runtime::execution::TurnRequest {
    let turn = TurnId::now();
    atman_runtime::flow_runtime::execution::TurnRequest {
        turn: turn.clone(),
        run: RunId::now(),
        messages: vec![message],
        arguments: BTreeMap::from([("input".into(), serde_json::json!(argument))]),
        invocation: BTreeMap::from([("marker".into(), serde_json::json!("recorded"))]),
        context: vec![Message::user_text(turn, "run the entry")],
        authority: atman_runtime::flow_authority::EffectiveAuthority::root(
            &Default::default(),
            false,
            None,
        ),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
    }
}

struct ExecutionProbe {
    entered: std::sync::Arc<tokio::sync::Semaphore>,
    release: std::sync::Arc<tokio::sync::Semaphore>,
}

struct InboxRecordingProvider {
    requests: std::sync::Mutex<Vec<atman_runtime::provider::LlmRequest>>,
    inner: atman_runtime::providers::mock::MockProvider,
}

impl atman_runtime::provider::Provider for InboxRecordingProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn call<'a>(
        &'a self,
        request: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::tool::BoxFut<
        'a,
        Result<atman_runtime::provider::AssistantMessage, atman_runtime::RuntimeError>,
    > {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.call(request)
    }

    fn call_streaming(
        &self,
        request: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::event::Observable<atman_runtime::provider::AssistantMessage> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.call_streaming(request)
    }
}

#[tokio::test]
async fn instance_inbox_enters_only_shared_general_calls_and_survives_recovery_without_reconsumption()
 {
    use atman_runtime::flow_runtime::FlowWorker;
    use atman_runtime::message::MessageOrigin;
    let mut text_model = common::model_for_provider("mock-text", "mock", 8_192, None);
    text_model.1.input_modalities = vec![atman_runtime::provider::InputModality::Text];
    let _registry = common::ModelRegistryGuard::acquire(common::config([
        common::mock_model("mock"),
        text_model,
    ]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert("entry:main.at".into(), r#"
        flow shared(model: string) {
            llm.call(model: model, prompt: "shared", context: "session", record: false)
        }
        flow branch() {
            llm.call(model: "mock", prompt: "branch", context: "session", record: false)
        }
        flow main(input: int) -> string {
            execution_probe()
            llm.extract(model: "mock", prompt: "extract", context: "session", fields: { value: string -- "value" })
            fanout [branch()]
            llm.call(model: "mock", prompt: "isolated", context: "none", record: false)
            shared("mock-text").await
            shared("mock").await
            return final.answer(message: "finished")
        }
    "#.into());
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let first = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "entry",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let executor = atman_runtime::Executor::new();
    let provider = std::sync::Arc::new(InboxRecordingProvider {
        requests: Default::default(),
        inner: atman_runtime::providers::mock::MockProvider::new("mock")
            .with_fallback(atman_runtime::Value::Str(r#"{"value":"ok"}"#.into())),
    });
    executor.providers.register(provider.clone());
    let probe = std::sync::Arc::new(ExecutionProbe {
        entered: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        release: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let ticket = worker
        .start_turn(executor, turn_request(first.message_id, 0))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    let mut accepted = Vec::new();
    let mut pending = Vec::new();
    let mut successful_edit = None;
    for (text, mode, marker) in [
        ("boundary-one", DeliveryMode::AtBoundary, None),
        ("boundary-two", DeliveryMode::NextTurn, Some("recorded")),
        ("command-boundary", DeliveryMode::NextTurn, Some("recorded")),
        (
            "command-other-config",
            DeliveryMode::NextTurn,
            Some("different"),
        ),
        ("next-turn-only", DeliveryMode::NextTurn, None),
        (
            "different-config",
            DeliveryMode::NextTurn,
            Some("different"),
        ),
    ] {
        let mut request = delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            text,
        );
        request.delivery = mode;
        if let Some(marker) = marker {
            let content = atman_runtime::flow_store::inbox::UserContent {
                text: text.into(),
                images: vec![],
                presentation: None,
            };
            let invocation = [("marker".into(), serde_json::json!(marker))].into();
            request.payload = if text.starts_with("command-") {
                InboxPayload::Command {
                    entry: Box::new(description.entry().unwrap().clone()),
                    arguments: [("input".into(), 99.into())].into(),
                    invocation,
                    user: Some(content),
                }
            } else {
                InboxPayload::UserSubmission {
                    content,
                    invocation,
                }
            };
        }
        let receipt = worker
            .accept_input(request.clone(), |_| Ok(()))
            .await
            .unwrap();
        if let InboxPayload::UserSubmission { content, .. }
        | InboxPayload::Command {
            user: Some(content),
            ..
        } = &request.payload
        {
            let edit = atman_runtime::flow_store::inbox::InboxEditRequest {
                id: Uuid::new_v4(),
                sender: request.sender.clone(),
                message: receipt.message_id,
                expected: receipt.receipt.clone(),
                change: atman_runtime::flow_store::inbox::InboxChange::Replace {
                    content: content.clone(),
                    delivery: DeliveryMode::AtBoundary,
                },
            };
            let before = worker.view_snapshot().await.unwrap();
            let outcome = worker.edit_input(edit.clone(), |_| Ok(())).await;
            let after = worker.view_snapshot().await.unwrap();
            assert_eq!(after.status.execution, before.status.execution);
            let entry = after
                .pending
                .iter()
                .find(|entry| entry.envelope.id == receipt.message_id)
                .unwrap();
            if text == "boundary-two" || text == "command-boundary" {
                let committed = outcome.unwrap();
                assert_eq!(entry.revision, committed);
                assert_eq!(entry.acceptance.receipt, receipt.receipt);
                assert_eq!(entry.envelope.request.delivery, DeliveryMode::AtBoundary);
                assert_eq!(
                    entry.envelope.request.payload, request.payload,
                    "promotion preserves captured source, arguments and user content"
                );
                successful_edit = Some((edit, committed));
            } else {
                assert!(
                    outcome
                        .unwrap_err()
                        .to_string()
                        .contains("invocation settings")
                );
                assert_eq!(after.status.cursor, before.status.cursor);
                assert_eq!(entry.envelope.request.delivery, DeliveryMode::NextTurn);
            }
        }
        if text.starts_with("boundary") || text == "command-boundary" {
            accepted.push(receipt.message_id);
        } else {
            pending.push(receipt.message_id);
        }
    }
    let image_content = atman_runtime::flow_store::inbox::UserContent {
        text: "image-boundary".into(),
        images: vec![atman_runtime::message::ImageSource {
            media_type: "image/png".into(),
            data: atman_runtime::message::ImageData::Base64 { data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7p8AAAAASUVORK5CYII=".into() },
            detail: Default::default(),
        }],
        presentation: None,
    };
    let mut image_request = delivery(
        &description.address,
        SenderIdentity::User(Uuid::new_v4()),
        "",
    );
    image_request.delivery = DeliveryMode::NextTurn;
    image_request.payload = InboxPayload::Command {
        entry: Box::new(description.entry().unwrap().clone()),
        arguments: [("input".into(), 99.into())].into(),
        user: Some(image_content.clone()),
        invocation: BTreeMap::new(),
    };
    let image_input = worker
        .accept_input(image_request.clone(), |_| Ok(()))
        .await
        .unwrap();
    let image_edit = atman_runtime::flow_store::inbox::InboxEditRequest {
        id: Uuid::new_v4(),
        sender: image_request.sender,
        message: image_input.message_id,
        expected: image_input.receipt,
        change: atman_runtime::flow_store::inbox::InboxChange::Replace {
            content: image_content,
            delivery: DeliveryMode::AtBoundary,
        },
    };
    let (left, right) = tokio::join!(
        worker.edit_input(image_edit.clone(), |_| Ok(())),
        worker.edit_input(image_edit, |_| Ok(()))
    );
    assert_eq!(left.unwrap(), right.unwrap());
    let peer = descriptor(info.id).address;
    let peer_receipt = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(peer),
                "peer-not-user",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    accepted.push(peer_receipt.message_id);
    probe.release.add_permits(1);
    assert!(provider.requests.lock().unwrap().is_empty());
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(15), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(done.result.is_ok(), "{:?}", done.result);
    let (edit, edit_receipt) = successful_edit.unwrap();
    let mut changes = worker.subscribe_commits();
    changes.borrow_and_update();
    assert_eq!(
        worker.edit_input(edit.clone(), |_| Ok(())).await.unwrap(),
        edit_receipt
    );
    assert!(
        !changes.has_changed().unwrap(),
        "edit receipt retry must not wake scheduling"
    );
    assert!(
        worker
            .edit_input(edit, |_| Err(std::io::Error::other("revoked")))
            .await
            .is_err()
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    for (index, request) in requests.iter().enumerate() {
        let images: Vec<_> = request
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| matches!(part, atman_runtime::message::MessagePart::Image { .. }))
            .collect();
        assert_eq!(
            images.len(),
            usize::from(index == 4),
            "images must wait for a capable shared call"
        );
        for text in [
            "boundary-one",
            "boundary-two",
            "command-boundary",
            "peer-not-user",
        ] {
            let messages: Vec<_> = request
                .messages
                .iter()
                .filter(|m| m.text_concat().contains(text))
                .collect();
            assert_eq!(
                messages.len(),
                usize::from(index >= 3),
                "call {index}: {text}"
            );
            if text == "peer-not-user" && index >= 3 {
                assert_ne!(messages[0].origin, MessageOrigin::User);
                assert!(messages[0].text_concat().contains("not user authorization"));
            }
        }
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.text_concat().contains("next-turn-only")
                    || m.text_concat().contains("different-config")
                    || m.text_concat().contains("command-other-config"))
        );
    }
    let messages = worker.context().messages().to_vec();
    let mut store = worker.shutdown().await.unwrap();
    assert_eq!(
        store.pending_inputs().map(|e| e.id).collect::<Vec<_>>(),
        pending
    );
    let inputs = store.committed_inputs();
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[1].message_ids, accepted);
    assert_eq!(inputs[2].message_ids, [image_input.message_id]);
    assert_eq!(store.context_messages().unwrap(), messages);
    drop(store);
    let mut restored = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(restored.context_messages().unwrap(), messages);
    assert_eq!(
        restored.pending_inputs().map(|e| e.id).collect::<Vec<_>>(),
        pending
    );
}

impl atman_runtime::tool::Tool for ExecutionProbe {
    fn name(&self) -> &str {
        "execution_probe"
    }
    fn tier(&self) -> atman_runtime::tool::Tier {
        atman_runtime::tool::Tier::Zero
    }
    fn call<'a>(
        &'a self,
        _args: atman_runtime::tool::ToolArgs,
        _ctx: &'a atman_runtime::tool::ToolCtx,
    ) -> atman_runtime::tool::BoxFut<'a, atman_runtime::tool::ToolResult> {
        Box::pin(async move {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(atman_runtime::Value::Unit)
        })
    }
}

#[tokio::test]
async fn owned_execution_commits_admission_and_completion_even_when_the_observer_leaves() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"flow main(input: int) -> string {
        when env("marker") != "recorded" { return "invalid environment" }
        execution_probe()
        return final.answer(message: to_json_string(input * 2))
    }"#
        .into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let first = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "first",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let executor = atman_runtime::Executor::new();
    let probe = std::sync::Arc::new(ExecutionProbe {
        entered: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        release: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let ticket = worker
        .start_turn(executor.clone(), turn_request(first.message_id, 21))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let duplicate = atman_runtime::InstanceInvocation::new(
        ticket.lease.clone(),
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None),
        Default::default(),
    )
    .with_storage(worker.clone())
    .unwrap();
    let duplicate_entry = atman_rt::parse_file("flow main() { execution_probe() }").unwrap();
    assert!(
        executor
            .run_with_invocation(
                &duplicate_entry,
                "main",
                vec![],
                atman_runtime::RootInvocation {
                    instance: Some(duplicate),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(
        worker
            .start_turn(executor.clone(), turn_request(first.message_id, 9))
            .await
            .is_err()
    );
    let pause_a = Uuid::new_v4().into();
    let pause_b = Uuid::new_v4().into();
    let mut state = worker.subscribe_state();
    for key in [pause_a, pause_b] {
        worker
            .transition(
                worker.state().await.unwrap().revision(),
                FlowTransition::ExecutionPaused { key },
            )
            .await
            .unwrap();
    }
    assert_eq!(state.borrow().pauses().len(), 2);
    probe.release.add_permits(1);
    let second = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "second",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key: pause_a },
        )
        .await
        .unwrap();
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert!(
        !worker
            .context()
            .messages()
            .iter()
            .any(|message| message.origin == atman_runtime::message::MessageOrigin::FinalAnswer)
    );
    let abandoned_turn = ticket.lease.turn.clone();
    drop(ticket);
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key: pause_b },
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if state.borrow_and_update().can_start_turn() {
                break;
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        worker.context().messages().last().unwrap().text_concat(),
        "42"
    );
    let ticket = worker
        .start_turn(executor, turn_request(second.message_id, 8))
        .await
        .unwrap();
    probe.release.add_permits(1);
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end.outcome, TurnOutcome::Completed);
    assert!(matches!(end.result, Ok(atman_runtime::Value::Str(text)) if text == "16"));
    assert!(worker.state().await.unwrap().can_start_turn());
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 2);
    assert!(store.pending_inputs().next().is_none());
    assert_eq!(store.cursor(), &end.receipt);
    drop(store);
    let mut store = session.open_flow(&description.address.instance_id).unwrap();
    assert!(store.state().can_start_turn());
    assert_eq!(
        store
            .context_messages()
            .unwrap()
            .last()
            .unwrap()
            .text_concat(),
        "16"
    );
    assert_eq!(store.completions().len(), 2);
    assert_eq!(
        store
            .completion(Some(&abandoned_turn))
            .unwrap()
            .completion
            .result,
        atman_runtime::flow_store::completion::InvocationResult::Returned(
            atman_runtime::flow_store::completion::StoredValue::String("42".into())
        )
    );
}

#[tokio::test]
async fn cooperative_instance_execution_can_be_cancelled_while_paused_without_replaying_input() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        "flow main(input: int) { loop { value = input + 1 } }".into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let accepted = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "loop",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let ticket = worker
        .start_turn(
            atman_runtime::Executor::new(),
            turn_request(accepted.message_id, 1),
        )
        .await
        .unwrap();
    let lease = ticket.lease.clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while worker.context().sink().next_seq_peek() < 1024 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("continuous diagnostics must yield to the writer and control tasks");
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::PhaseChanged {
                lease: lease.clone(),
                phase: TurnPhase::Cancelling,
            },
        )
        .await
        .unwrap();
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        end.outcome,
        TurnOutcome::Interrupted(Interruption::Cancelled)
    );
    assert!(end.result.is_err());
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.state().pauses(), &[key]);
    assert_eq!(store.committed_inputs().len(), 1);
    assert!(store.pending_inputs().next().is_none());
    drop(store);
    let store = session.open_flow(&description.address.instance_id).unwrap();
    assert!(matches!(
        store.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: Interruption::Cancelled,
            ..
        })
    ));
    assert_eq!(store.state().pauses(), &[key]);
}

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
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
    }
}

fn session_descriptor() -> SessionDescriptor {
    SessionDescriptor {
        configuration: Default::default(),
        id: Uuid::new_v4(),
        project: None,
        title: "Example".into(),
    }
}

fn change(flow: &mut FlowStore, transition: FlowTransition) {
    flow.transition(flow.state().revision(), transition)
        .unwrap();
}

fn start(flow: &mut FlowStore) -> TurnLease<AtmanUuid> {
    change(
        flow,
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    );
    let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
        panic!("turn was not started");
    };
    active.lease.clone()
}

#[test]
fn transcript_turn_graphs_preserve_instance_ownership_times_and_completion() {
    use atman_runtime::{
        event::{Event, FlowStatus},
        flow_store::{
            completion::{InvocationResult, StoredValue},
            publication::MessagePublication,
            trace::{ExecutionEvent, ExecutionScope},
        },
        message::{MessageOrigin, MessagePart, MessageRole},
        projection::flow::FlowTranscript,
        workflow::NodeStatus,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_descriptor()).unwrap();
    let info = descriptor(session.projection().descriptor.id);
    let mut flow = session.create_flow(info.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let end = at + chrono::Duration::seconds(7);
    let mut source = FlowTranscript::new(info.address.clone());
    let mut sequence = 0;
    for cancelled in [false, true] {
        let lease = start(&mut flow);
        let mut record = |event, timestamp| {
            sequence += 1;
            ExecutionEvent {
                scope: ExecutionScope::Invocation(lease.clone()),
                sequence,
                timestamp,
                event,
            }
        };
        flow.append_execution_events(vec![
            record(
                Event::FlowStart {
                    run_id: lease.run.clone(),
                    flow_name: "worker".into(),
                    parent_run_id: Some(RunId::now()),
                    parent_node_id: Some("spawn".into()),
                    spawned: true,
                },
                at,
            ),
            record(
                Event::FlowNodeStart {
                    run_id: lease.run.clone(),
                    node_id: "dispatch".into(),
                    kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                        path: "dispatch_all".into(),
                    },
                    label: "dispatch_all".into(),
                    parent_node_id: None,
                },
                at,
            ),
            record(
                Event::ToolNode {
                    run_id: lease.run.clone(),
                    parent_node_id: "dispatch".into(),
                    tool_use_id: "read".into(),
                    tool_name: "fs.read".into(),
                    args_preview: "file.txt".into(),
                    call_intent: None,
                },
                at,
            ),
        ])
        .unwrap();
        flow.publish_message(MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message: Message {
                turn_id: lease.turn.clone(),
                role: MessageRole::Tool,
                origin: MessageOrigin::Internal,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "read".into(),
                    content: "missing file".into(),
                    is_error: true,
                }],
            },
        })
        .unwrap();
        if cancelled {
            change(
                &mut flow,
                FlowTransition::PhaseChanged {
                    lease: lease.clone(),
                    phase: TurnPhase::Cancelling,
                },
            );
        } else {
            flow.append_execution_events(vec![record(
                Event::FlowEnd {
                    run_id: lease.run.clone(),
                    flow_name: "worker".into(),
                    status: FlowStatus::Ok,
                },
                end,
            )])
            .unwrap();
        }
        let completed = flow
            .finish_turn(lease.clone(), InvocationResult::Returned(StoredValue::Unit))
            .unwrap();
        // Buffered execution diagnostics can be persisted after the completion fact.
        flow.append_execution_events(vec![record(
            Event::FlowNodeStart {
                run_id: lease.run.clone(),
                node_id: "late".into(),
                kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                    path: "flush".into(),
                },
                label: "flush".into(),
                parent_node_id: None,
            },
            end,
        )])
        .unwrap();
        loop {
            let page = flow
                .read_updates(source.cursor(), std::num::NonZeroUsize::new(2).unwrap())
                .unwrap();
            for batch in page.batches {
                source.apply(batch.clone()).unwrap();
                let revisions = source
                    .turns()
                    .map(|turn| turn.workflow().revision())
                    .collect::<Vec<_>>();
                source.apply(batch).unwrap();
                assert_eq!(
                    source
                        .turns()
                        .map(|turn| turn.workflow().revision())
                        .collect::<Vec<_>>(),
                    revisions
                );
            }
            if !page.has_more {
                break;
            }
        }
        let turn = source.turns().next_back().unwrap();
        assert_eq!(turn.lease(), &lease);
        assert_eq!(turn.workflow().turn_id, lease.turn);
        assert_eq!(
            turn.completion().unwrap().outcome,
            completed.completion.outcome
        );
        let graph = turn.workflow();
        assert_eq!(
            graph.root.len(),
            1,
            "external creators do not hide this instance's entry"
        );
        assert_eq!(graph.root[0].started_at, Some(at));
        assert_eq!(
            graph.root[0].ended_at,
            Some(if cancelled {
                completed.completion.finished_at
            } else {
                end
            })
        );
        let tool = graph
            .find_node(&format!("tool:{}:read", lease.run))
            .unwrap();
        assert_eq!(tool.status, NodeStatus::Err);
        assert_eq!(
            tool.ended_at, None,
            "a publication with no timestamp must not invent a duration"
        );
        let late = graph.find_node(&format!("{}::late", lease.run)).unwrap();
        assert_eq!(
            late.status,
            if cancelled {
                NodeStatus::Cancelled
            } else {
                NodeStatus::Ok
            }
        );
        assert_eq!(late.ended_at, Some(completed.completion.finished_at));
    }
    assert_eq!(source.turns().count(), 2);
    let mut replay = FlowTranscript::new(info.address);
    loop {
        let page = flow
            .read_updates(replay.cursor(), std::num::NonZeroUsize::new(128).unwrap())
            .unwrap();
        for batch in page.batches {
            replay.apply(batch).unwrap();
        }
        if !page.has_more {
            break;
        }
    }
    for (live, restored) in source.turns().zip(replay.turns()) {
        assert_eq!(live.workflow().graph(), restored.workflow().graph());
        assert_eq!(
            live.completion().unwrap().record,
            restored.completion().unwrap().record
        );
    }
}

#[test]
fn transcript_projection_preserves_committed_provenance_and_rejects_partial_or_foreign_batches() {
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_store::position::FlowBatch,
        flow_store::{
            completion::{InvocationResult, StoredValue},
            context::{ContextCheckpoint, context_digest},
            publication::MessagePublication,
            view::FlowUpdate,
        },
        message::MessageOrigin,
        projection::flow::{FlowRecord, FlowTranscript},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_descriptor()).unwrap();
    let info = descriptor(session.projection().descriptor.id);
    let mut flow = session.create_flow(info.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let sender = SenderIdentity::User(Uuid::new_v4());
    let accepted = flow
        .accept_input(delivery(&info.address, sender.clone(), "request"), |_| {
            Ok(())
        })
        .unwrap();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = batch.lease().clone();
    flow.commit_input(
        &batch,
        InputContent::InvocationInput {
            command: None,
            arguments: BTreeMap::new(),
            invocation: BTreeMap::new(),
            messages: vec![Message::user_text(lease.turn.clone(), "request")],
        },
    )
    .unwrap();
    let pending = flow
        .accept_input(delivery(&info.address, sender, "pending"), |_| Ok(()))
        .unwrap();
    let mut message = Message::assistant_text(lease.turn.clone(), "answer");
    message.origin = MessageOrigin::FinalAnswer;
    let publication = MessagePublication {
        id: Uuid::new_v4(),
        lease: lease.clone(),
        run: lease.run.clone(),
        context_run: None,
        message,
    };
    flow.publish_message(publication.clone()).unwrap();
    let mut isolated = publication.clone();
    isolated.id = Uuid::new_v4();
    isolated.run = RunId::now();
    isolated.context_run = Some(isolated.run.clone());
    isolated.message = Message::assistant_text(lease.turn.clone(), "isolated");
    flow.publish_message(isolated).unwrap();
    let window = flow.context_messages().unwrap();
    flow.checkpoint_context(ContextCheckpoint {
        id: Uuid::new_v4(),
        expected: context_digest(&window).unwrap(),
        messages: vec![Message::user_text(lease.turn.clone(), "summary")],
        summary: "summary".into(),
        range: 0..window.len(),
        count: window.len(),
        before_tokens: 20,
        after_tokens: 5,
    })
    .unwrap();
    flow.finish_turn(
        lease,
        InvocationResult::Returned(StoredValue::String("result".into())),
    )
    .unwrap();
    let cursor = flow.cursor().clone();
    let state = flow.state().clone();
    let mut projected = FlowTranscript::new(info.address.clone());
    let mut batches = Vec::new();
    loop {
        let page = flow
            .read_updates(projected.cursor(), std::num::NonZeroUsize::new(2).unwrap())
            .unwrap();
        for batch in page.batches {
            batches.push(batch.clone());
            projected.apply(batch.clone()).unwrap();
            let count = projected.entries().len();
            assert_eq!(projected.apply(batch).unwrap(), count..count);
        }
        if !page.has_more {
            break;
        }
    }
    assert_eq!(projected.cursor(), Some(&cursor));
    assert_eq!(
        projected
            .entries()
            .iter()
            .flat_map(|entry| entry.record.shared_messages())
            .map(Message::text_concat)
            .collect::<Vec<_>>(),
        ["request", "answer"]
    );
    assert!(
        projected
            .entries()
            .iter()
            .any(|entry| matches!(entry.record, FlowRecord::Checkpoint(_)))
    );
    assert!(projected.entries().iter().any(|entry| matches!(&entry.record, FlowRecord::Publication(value) if value.context_run.is_some())));
    assert_eq!(flow.state(), &state);
    assert_eq!(
        flow.pending_inputs()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        [pending.message_id]
    );
    assert_ne!(accepted.message_id, pending.message_id);

    let count = projected.entries().len();
    let mut conflicting = batches[0].clone();
    durable_position(&mut conflicting.receipt).digest.push('0');
    assert!(projected.apply(conflicting).is_err());
    let mut gap = batches.last().unwrap().clone();
    durable_position(&mut gap.receipt).sequence += 2;
    assert!(projected.apply(gap).is_err());
    let mut foreign = publication.clone();
    foreign.lease.flow.session_id = Uuid::new_v4().into();
    assert!(
        projected
            .apply(FlowBatch {
                receipt: CommitReceipt {
                    sequence: cursor.sequence() + 1,
                    ..cursor.require_durable().unwrap().clone()
                }
                .into(),
                facts: vec![
                    FlowUpdate::MessagePublished(Box::new(publication)),
                    FlowUpdate::MessagePublished(Box::new(foreign))
                ],
            })
            .is_err()
    );
    assert_eq!(projected.cursor(), Some(&cursor));
    assert_eq!(projected.entries().len(), count);
    let mut rebuilt = FlowTranscript::new(info.address.clone());
    for batch in batches {
        rebuilt.apply(batch).unwrap();
    }
    assert_eq!(
        rebuilt
            .entries()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        projected
            .entries()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>()
    );
    let mut other_flow = info.address;
    other_flow.instance_id = FlowInstanceId::now();
    assert!(
        FlowTranscript::new(other_flow)
            .apply(FlowBatch {
                receipt: cursor,
                facts: vec![]
            })
            .is_err()
    );
}

#[test]
fn completion_and_terminal_state_commit_together_and_recover_without_fabricating_results() {
    use atman_runtime::flow_store::completion::{InvocationResult, StoredValue};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let result = InvocationResult::Returned(StoredValue::Struct(vec![(
        "answer".into(),
        StoredValue::Int(42),
    )]));
    let completed = flow.finish_turn(lease.clone(), result.clone()).unwrap();
    assert_eq!(completed.completion.outcome, TurnOutcome::Completed);
    assert_eq!(flow.completion(Some(&lease.turn)), Some(&completed));
    assert_eq!(flow.cursor(), &completed.receipt);
    assert!(flow.state().can_start_turn());
    assert!(flow.published_messages().is_empty());
    assert!(flow.context_messages().unwrap().is_empty());
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(
        flow.finish_turn(lease.clone(), result.clone()).unwrap(),
        completed
    );
    assert!(
        flow.finish_turn(lease, InvocationResult::Returned(StoredValue::Int(7)))
            .is_err()
    );

    let cancelled = start(&mut flow);
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: cancelled.clone(),
            phase: TurnPhase::Cancelling,
        },
    );
    let record = flow.finish_turn(cancelled.clone(), result.clone()).unwrap();
    assert_eq!(
        record.completion.outcome,
        TurnOutcome::Interrupted(Interruption::Cancelled)
    );
    assert_eq!(record.completion.result, result);
    assert_eq!(flow.finish_turn(cancelled, result.clone()).unwrap(), record);
    change(&mut flow, FlowTransition::InterruptionRecovered);

    let torn = start(&mut flow);
    let file = flow.dir().unwrap().join("events.jsonl");
    let prefix = std::fs::metadata(&file).unwrap().len() as usize;
    flow.finish_turn(torn.clone(), result).unwrap();
    drop(flow);
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(&file, &bytes[..prefix + (bytes.len() - prefix) / 2]).unwrap();
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    let recovered = flow.completion(Some(&torn.turn)).unwrap();
    assert_eq!(
        recovered.completion.outcome,
        TurnOutcome::Interrupted(Interruption::HostLost)
    );
    assert_eq!(
        recovered.completion.result,
        InvocationResult::Unobserved(TurnOutcome::Interrupted(Interruption::HostLost))
    );
    assert_eq!(flow.completions().len(), 3);
    assert!(flow.interrupt_lost_turn().unwrap().is_none());
}

#[test]
fn publications_preserve_input_order_and_fence_stale_delivery_across_recovery() {
    use atman_runtime::flow_store::publication::MessagePublication;
    use atman_runtime::message::MessageOrigin;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let first = MessagePublication {
        id: Uuid::new_v4(),
        lease: lease.clone(),
        run: RunId::now(),
        context_run: None,
        message: Message::assistant_text(lease.turn.clone(), "working"),
    };
    let first_record = flow.publish_message(first.clone()).unwrap();
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "detail",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let reserved = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let input = Message::user_text(lease.turn.clone(), "detail");
    flow.commit_input(
        &reserved,
        InputContent::Context {
            messages: vec![input.clone()],
        },
    )
    .unwrap();
    change(
        &mut flow,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: TurnPhase::Finishing,
        },
    );
    let mut answer = first.clone();
    answer.id = Uuid::new_v4();
    answer.message = Message::assistant_text(lease.turn.clone(), "done");
    answer.message.origin = MessageOrigin::FinalAnswer;
    flow.publish_message(answer.clone()).unwrap();
    let expected = vec![first.message.clone(), input, answer.message.clone()];
    assert_eq!(flow.context_messages().unwrap(), expected);
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: lease.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    let cursor = flow.cursor().clone();
    assert_eq!(flow.publish_message(first.clone()).unwrap(), first_record);
    assert_eq!(flow.cursor(), &cursor);
    let mut stale = first.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.publish_message(stale.clone()).is_err());
    let mut conflict = first.clone();
    conflict.message.parts.clear();
    assert!(flow.publish_message(conflict).is_err());
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.context_messages().unwrap(), expected);
    assert_eq!(flow.publish_message(first).unwrap(), first_record);
    let key = *flow.state().pauses().first().unwrap();
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = start(&mut flow);
    assert!(flow.publish_message(stale.clone()).is_err());
    stale.lease = current.clone();
    assert!(flow.publish_message(stale.clone()).is_err());
    stale.message.turn_id = current.turn.clone();
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: current,
            phase: TurnPhase::Cancelling,
        },
    );
    assert!(flow.publish_message(stale).is_err());
    assert_eq!(flow.context_messages().unwrap(), expected);
}

#[test]
fn goal_facts_recover_exact_revisions_without_changing_execution_or_inbox() {
    use atman_runtime::flow_store::goal::{GoalRequest, GoalVersion};
    use atman_runtime::memory::goal::{GoalActor, GoalEdit, GoalPauseReason, GoalStatus};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let user = SenderIdentity::User(Uuid::new_v4());
    let request = |flow: &FlowStore, edit| GoalRequest {
        start_turn: false,
        id: Uuid::new_v4(),
        sender: user.clone(),
        expected: flow.goal().map(GoalVersion::from),
        edit,
    };
    let create = request(
        &flow,
        GoalEdit::Create {
            text: "retained objective".into(),
        },
    );
    let created = flow.edit_goal(create.clone(), |_| Ok(())).unwrap();
    let initial = created.goal.as_ref().unwrap();
    let key = Uuid::new_v4().into();
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    flow.accept_input(
        delivery(&description.address, user.clone(), "new work"),
        |_| Ok(()),
    )
    .unwrap();
    let state = flow.state().clone();
    let paused = flow
        .edit_goal(
            request(
                &flow,
                GoalEdit::Pause {
                    actor: GoalActor::User,
                    reason: GoalPauseReason::UserRequested,
                    summary: "hold objective".into(),
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(paused.goal.as_ref().unwrap().revision, initial.revision + 1);
    let agent_resume = request(
        &flow,
        GoalEdit::Resume {
            actor: GoalActor::Agent,
        },
    );
    assert!(flow.edit_goal(agent_resume, |_| Ok(())).is_err());
    let edit = request(
        &flow,
        GoalEdit::Set {
            text: "edited while paused".into(),
            replace_completed: false,
        },
    );
    let updated = flow.edit_goal(edit.clone(), |_| Ok(())).unwrap();
    assert_eq!(updated.goal.as_ref().unwrap().status, GoalStatus::Paused);
    assert_eq!(flow.state(), &state);
    assert_eq!(flow.pending_inputs().count(), 1);
    let resumed = flow
        .edit_goal(
            request(
                &flow,
                GoalEdit::Resume {
                    actor: GoalActor::User,
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        flow.state(),
        &state,
        "goal resume must not remove execution pauses"
    );
    assert_eq!(
        flow.edit_goal(create.clone(), |_| panic!("retry authorization"))
            .unwrap(),
        created
    );
    assert_eq!(flow.goal(), resumed.goal.as_ref());
    let mut stale = edit.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.edit_goal(stale, |_| Ok(())).is_err());
    let mut conflict = edit;
    conflict.edit = GoalEdit::Clear;
    assert!(flow.edit_goal(conflict, |_| Ok(())).is_err());
    let journal = flow.dir().unwrap().join("events.jsonl");
    let prefix = std::fs::metadata(&journal).unwrap().len();
    flow.edit_goal(request(&flow, GoalEdit::Clear), |_| Ok(()))
        .unwrap();
    drop(flow);
    let length = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), prefix);
    assert_eq!(flow.goal(), resumed.goal.as_ref());
    assert_eq!(flow.state(), &state);
    assert_eq!(flow.pending_inputs().count(), 1);
    let cleared = flow
        .edit_goal(request(&flow, GoalEdit::Clear), |_| Ok(()))
        .unwrap();
    assert!(cleared.goal.is_none());
    assert_eq!(flow.state(), &state);
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.goal().is_none());
    assert_eq!(flow.goal_updates().len(), 5);
    change(&mut flow, FlowTransition::CloseRequested);
    assert_eq!(
        flow.edit_goal(create, |_| panic!("closed retry authorization"))
            .unwrap(),
        created
    );
    assert!(
        flow.edit_goal(
            request(
                &flow,
                GoalEdit::Create {
                    text: "closed".into()
                }
            ),
            |_| Ok(())
        )
        .is_err()
    );
}

#[test]
fn explicit_goal_resume_atomically_records_one_internal_next_turn_input() {
    use atman_runtime::flow_store::goal::{GoalRequest, GoalVersion};
    use atman_runtime::memory::goal::{GoalActor, GoalEdit, GoalPauseReason, GoalStatus};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let user = SenderIdentity::User(Uuid::new_v4());
    let request = |flow: &FlowStore, edit, start_turn| GoalRequest {
        id: Uuid::new_v4(),
        sender: user.clone(),
        expected: flow.goal().map(GoalVersion::from),
        edit,
        start_turn,
    };
    flow.edit_goal(
        request(
            &flow,
            GoalEdit::Create {
                text: "retained objective".into(),
            },
            false,
        ),
        |_| Ok(()),
    )
    .unwrap();
    let pause = GoalEdit::Pause {
        actor: GoalActor::User,
        reason: GoalPauseReason::UserRequested,
        summary: "hold".into(),
    };
    flow.edit_goal(request(&flow, pause.clone(), false), |_| Ok(()))
        .unwrap();
    let lease = start(&mut flow);
    let resume = request(
        &flow,
        GoalEdit::Resume {
            actor: GoalActor::User,
        },
        true,
    );
    let mut forged = resume.clone();
    forged.sender = SenderIdentity::Flow(description.address.clone());
    assert!(flow.edit_goal(forged, |_| Ok(())).is_err());
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Paused);
    let resumed = flow.edit_goal(resume.clone(), |_| Ok(())).unwrap();
    let envelope = resumed.update.input.as_ref().unwrap();
    assert_eq!(
        flow.inbox_entry(envelope.id).unwrap().unwrap().envelope,
        *envelope
    );
    assert_eq!(flow.inbox_entries()[0].acceptance.receipt, resumed.receipt);
    assert_eq!(flow.pending_inputs().count(), 1);
    assert!(
        flow.reserve_context_input(&lease, |_| true)
            .unwrap()
            .is_none()
    );
    let mut forged = envelope.request.clone();
    forged.request_id = Uuid::new_v4();
    assert!(flow.accept_input(forged, |_| Ok(())).is_err());
    let projected =
        atman_runtime::flow_runtime::input::context_message(envelope, &lease.turn).unwrap();
    assert_eq!(
        projected.origin,
        atman_runtime::message::MessageOrigin::Internal
    );
    assert_eq!(projected.role, atman_runtime::message::MessageRole::User);
    assert!(
        flow.edit_goal(
            request(
                &flow,
                GoalEdit::Resume {
                    actor: GoalActor::User
                },
                true
            ),
            |_| Ok(())
        )
        .is_err()
    );
    // An interrupted invocation does not discard the user's accepted next-turn control.
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
        },
    );
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(common::pending_bodies(&mut flow).first(), Some(envelope));
    assert!(!flow.state().can_start_turn());
    assert_eq!(
        flow.edit_goal(resume.clone(), |_| panic!("retry authorization"))
            .unwrap(),
        resumed
    );
    // A later pause supersedes the unread resume without changing its original receipt.
    flow.edit_goal(request(&flow, pause, false), |_| Ok(()))
        .unwrap();
    assert_eq!(flow.pending_inputs().count(), 0);
    assert!(matches!(
        flow.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert_eq!(
        flow.edit_goal(resume, |_| panic!("retry authorization"))
            .unwrap(),
        resumed
    );
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Paused);
    let cursor = flow.cursor().clone();
    let journal = flow.dir().unwrap().join("events.jsonl");
    let prefix = std::fs::metadata(&journal).unwrap().len();
    flow.edit_goal(
        request(
            &flow,
            GoalEdit::Resume {
                actor: GoalActor::User,
            },
            true,
        ),
        |_| Ok(()),
    )
    .unwrap();
    drop(flow);
    let length = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), prefix);
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Paused);
    assert_eq!(flow.pending_inputs().count(), 0);
}

#[test]
fn goal_continuations_share_inbox_receipts_and_recover_without_restarting_interrupted_work() {
    use atman_runtime::flow_store::{
        completion::{InvocationResult, StoredValue},
        goal::{GoalRequest, GoalVersion},
    };
    use atman_runtime::memory::goal::{GoalActor, GoalEdit, GoalPauseReason};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    for scenario in [
        "deliver",
        "new_input",
        "edit",
        "pause",
        "replace",
        "clear",
        "cancel",
        "failure",
        "host_lost",
    ] {
        let description = descriptor(info.id);
        let mut flow = session.create_flow(description.clone()).unwrap();
        change(&mut flow, FlowTransition::Activated);
        let turn = TurnId::now();
        let run = RunId::now();
        let lease = flow
            .state()
            .clone()
            .begin_turn(turn.clone(), run.clone())
            .unwrap();
        change(&mut flow, FlowTransition::TurnStarted { turn, run });
        let goal = flow
            .edit_goal(
                GoalRequest {
                    start_turn: false,
                    id: Uuid::new_v4(),
                    sender: SenderIdentity::User(Uuid::new_v4()),
                    expected: None,
                    edit: GoalEdit::Create {
                        text: "objective".into(),
                    },
                },
                |_| Ok(()),
            )
            .unwrap()
            .goal
            .unwrap();
        let version = GoalVersion::from(&goal);
        let mut stale = version.clone();
        stale.revision += 1;
        assert!(
            flow.schedule_goal(lease.clone(), stale, "stale proposal".into(), |_| Ok(()))
                .unwrap()
                .is_none()
        );
        assert_eq!(flow.pending_inputs().count(), 0);
        let first = flow
            .schedule_goal(
                lease.clone(),
                version.clone(),
                "continue objective".into(),
                |_| Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            flow.schedule_goal(
                lease.clone(),
                version.clone(),
                "continue objective".into(),
                |_| unreachable!()
            )
            .unwrap(),
            Some(first.clone())
        );
        assert!(
            flow.schedule_goal(
                lease.clone(),
                version.clone(),
                "different request".into(),
                |_| Ok(())
            )
            .is_err()
        );
        let request = common::pending_bodies(&mut flow)[0].request.clone();
        let mut forged = request.clone();
        forged.request_id = Uuid::new_v4();
        assert!(flow.accept_input(forged.clone(), |_| Ok(())).is_err());
        forged.sender = SenderIdentity::User(Uuid::new_v4());
        assert!(flow.accept_input(forged, |_| Ok(())).is_err());
        assert!(
            flow.reserve_context_input(&lease, |_| true)
                .unwrap()
                .is_none()
        );
        assert!(
            flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
                .is_err()
        );
        let user = delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "new request",
        );
        let user_receipt = flow.accept_input(user.clone(), |_| Ok(())).unwrap();

        match scenario {
            "edit" | "pause" | "replace" | "clear" => {
                let edit = match scenario {
                    "edit" => GoalEdit::Set {
                        text: "updated objective".into(),
                        replace_completed: false,
                    },
                    "pause" => GoalEdit::Pause {
                        actor: GoalActor::User,
                        reason: GoalPauseReason::UserRequested,
                        summary: "hold objective".into(),
                    },
                    "replace" => GoalEdit::Create {
                        text: "new objective".into(),
                    },
                    _ => GoalEdit::Clear,
                };
                flow.edit_goal(
                    GoalRequest {
                        start_turn: false,
                        id: Uuid::new_v4(),
                        sender: SenderIdentity::User(Uuid::new_v4()),
                        expected: Some(version.clone()),
                        edit,
                    },
                    |_| Ok(()),
                )
                .unwrap();
            }
            "cancel" => change(
                &mut flow,
                FlowTransition::PhaseChanged {
                    lease: lease.clone(),
                    phase: TurnPhase::Cancelling,
                },
            ),
            _ => {}
        }
        if scenario != "host_lost" {
            let result = if scenario == "failure" {
                InvocationResult::Failed(atman_runtime::RuntimeError::ToolFailed(
                    "entry failed".into(),
                ))
            } else {
                InvocationResult::Returned(StoredValue::Unit)
            };
            flow.finish_turn(lease.clone(), result).unwrap();
        }
        let key = Uuid::new_v4().into();
        change(&mut flow, FlowTransition::ExecutionPaused { key });
        drop(flow);
        let mut flow = session.open_flow(&description.address.instance_id).unwrap();
        match scenario {
            "clear" => assert!(flow.goal().is_none()),
            "replace" => assert_ne!(flow.goal().unwrap().id, goal.id),
            _ => assert_eq!(flow.goal().unwrap().id, goal.id),
        }
        let acceptance = flow
            .accept_input(request.clone(), |_| unreachable!())
            .unwrap();
        let retained = matches!(scenario, "deliver" | "new_input");
        assert_eq!(
            matches!(acceptance.disposition, InboxDisposition::Pending),
            retained,
            "{scenario}"
        );
        assert_eq!(acceptance.receipt, first.receipt);
        assert_eq!(
            flow.accept_input(user, |_| unreachable!())
                .unwrap()
                .disposition,
            InboxDisposition::Pending
        );
        if retained {
            assert!(
                flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
                    .is_err()
            );
            change(&mut flow, FlowTransition::ExecutionResumed { key });
            let chosen = if scenario == "deliver" {
                first.message_id
            } else {
                user_receipt.message_id
            };
            let reservation = flow
                .reserve_invocation_input(TurnId::now(), RunId::now(), |envelope| {
                    envelope.id == chosen
                })
                .unwrap()
                .unwrap();
            let receipt = flow
                .commit_input(
                    &reservation,
                    InputContent::InvocationInput {
                        command: None,
                        arguments: Default::default(),
                        invocation: Default::default(),
                        messages: vec![],
                    },
                )
                .unwrap();
            assert_eq!(
                flow.accept_input(request.clone(), |_| unreachable!())
                    .unwrap()
                    .disposition,
                if scenario == "deliver" {
                    InboxDisposition::InputCommitted { by: receipt }
                } else {
                    InboxDisposition::Cancelled { by: receipt }
                }
            );
            assert!(!flow.pending_inputs().any(|envelope| matches!(
                envelope.request.payload,
                atman_runtime::flow_store::inbox::InputKind::GoalContinuation { .. }
            )));
        } else {
            assert!(matches!(
                acceptance.disposition,
                InboxDisposition::Cancelled { .. }
            ));
            assert_eq!(flow.pending_inputs().count(), 1);
        }
        drop(flow);
        let mut flow = session.open_flow(&description.address.instance_id).unwrap();
        assert!(!matches!(
            flow.accept_input(request, |_| unreachable!())
                .unwrap()
                .disposition,
            InboxDisposition::Pending
        ));
    }
}

#[test]
fn context_checkpoints_fence_stale_windows_and_recover_without_rewriting_history() {
    use atman_runtime::flow_store::context::{
        ContextCheckpoint, ContextCheckpointRef, context_digest,
    };
    use atman_runtime::flow_store::publication::MessagePublication;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let publish = |flow: &mut FlowStore, text: &str| {
        flow.publish_message(MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message: Message::assistant_text(lease.turn.clone(), text),
        })
        .unwrap();
    };
    let checkpoint = |messages: &[Message], summary: &str| ContextCheckpoint {
        id: Uuid::new_v4(),
        expected: context_digest(messages).unwrap(),
        messages: vec![Message::system_compact_summary(
            lease.turn.clone(),
            summary,
            0,
            (messages.len() - 1) as u64,
            messages.len(),
        )],
        summary: summary.into(),
        range: 0..messages.len(),
        count: messages.len(),
        before_tokens: 100,
        after_tokens: 10,
    };
    publish(&mut flow, "first original");
    let stale = checkpoint(&flow.context_messages().unwrap(), "stale");
    publish(&mut flow, "arrived during summary");
    let cursor = flow.cursor().clone();
    assert!(flow.checkpoint_context(stale).is_err());
    assert_eq!(flow.cursor(), &cursor);
    let original = flow.context_messages().unwrap();
    let first = checkpoint(&original, &"first summary ".repeat(8192));
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "pending",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let saved = flow.checkpoint_context(first.clone()).unwrap();
    assert_eq!(flow.context_messages().unwrap(), first.messages);
    assert_eq!(flow.history_messages().unwrap(), original);
    assert_eq!(flow.pending_inputs().count(), 1);
    let input = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    flow.commit_input(
        &input,
        InputContent::Context {
            messages: vec![Message::user_text(lease.turn.clone(), "pending")],
        },
    )
    .unwrap();
    let window = flow.context_messages().unwrap();
    let full = flow.history_messages().unwrap();
    assert_eq!(window.len(), 2);
    assert_eq!(full.len(), 3);
    assert_eq!(window[1], full[2]);
    assert_eq!(flow.checkpoint_context(first.clone()).unwrap(), saved);
    assert_eq!(
        flow.context_messages().unwrap(),
        window,
        "retry must not remove the tail"
    );
    let mut conflicting = first.clone();
    conflicting.summary = "conflict".into();
    assert!(flow.checkpoint_context(conflicting).is_err());
    let second = checkpoint(&window, "second summary");
    flow.checkpoint_context(second.clone()).unwrap();
    publish(&mut flow, "new tail");
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: lease.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    let window = flow.context_messages().unwrap();
    let full = flow.history_messages().unwrap();
    let journal = flow.dir().unwrap().join("events.jsonl");
    let prefix = std::fs::metadata(&journal).unwrap().len();
    let torn = checkpoint(&window, "uncommitted summary");
    flow.checkpoint_context(torn).unwrap();
    drop(flow);
    let length = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), prefix);
    assert_eq!(flow.context_checkpoints().len(), 2);
    assert_eq!(
        flow.context_checkpoints()[0],
        ContextCheckpointRef {
            id: first.id,
            receipt: saved.receipt.clone(),
        }
    );
    assert!(
        serde_json::to_vec(flow.context_checkpoints())
            .unwrap()
            .len()
            < 1024
    );
    assert_eq!(flow.latest_context_checkpoint().unwrap().checkpoint, second);
    assert_eq!(
        flow.context_checkpoint(first.id).unwrap(),
        Some(saved.clone())
    );
    assert!(flow.context_checkpoint(Uuid::new_v4()).unwrap().is_none());
    assert_eq!(flow.context_messages().unwrap(), window);
    assert_eq!(flow.history_messages().unwrap(), full);
    assert_eq!(flow.published_messages().len(), 3);
    assert_eq!(
        RuntimeStore::inspect_session(dir.path(), info.id)
            .unwrap()
            .inspect_flow(&description.address.instance_id)
            .unwrap()
            .message_count,
        full.len()
    );
    change(&mut flow, FlowTransition::CloseRequested);
    assert!(
        flow.checkpoint_context(checkpoint(&window, "too late"))
            .is_err()
    );
    assert_eq!(flow.checkpoint_context(first.clone()).unwrap(), saved);
    assert_eq!(flow.context_messages().unwrap(), window);

    // Historical retries must read the authoritative body and cannot reset the latest window.
    let cursor = flow.cursor().clone();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(flow.checkpoint_context(first).is_err());
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.context_messages().is_err());
    assert_eq!(flow.latest_context_checkpoint().unwrap().checkpoint, second);
}

#[tokio::test]
async fn instance_compaction_restores_window_and_raw_history_through_the_host() {
    use atman_runtime::flow_runtime::FlowWorker;
    use atman_runtime::model_registry::ModelEntry;
    use atman_runtime::stream::StreamFrame;
    let _registry = common::ModelRegistryGuard::acquire(common::config([(
        "mock-summary".into(),
        ModelEntry {
            model: "mock-summary".into(),
            context_budget: Some(40_000),
            compact_threshold_ratio: Some(0.8),
            ..Default::default()
        },
    )]))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let executor = atman_runtime::Executor::new();
    executor.providers.register(std::sync::Arc::new(
        atman_runtime::providers::mock::MockProvider::new("mock-summary").with_fallback(
            atman_runtime::Value::Str("Summary of previous work.".into()),
        ),
    ));
    let body = (0..60)
        .map(|index| {
            format!(
                "session.push(message.user(\"{} original {index}\"))\n",
                "x".repeat(4000),
            )
        })
        .collect::<String>();
    let file = atman_rt::parse_file(&format!(
        "flow main() -> string {{ {body} return final.answer(message: \"delivered\") }}"
    ))
    .unwrap();
    let authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(
                    atman_runtime::InstanceInvocation::new(
                        lease.clone(),
                        authority.clone(),
                        Default::default(),
                    )
                    .with_storage(worker.clone())
                    .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let raw = context.messages_full();
    let before = context.messages().to_vec();
    assert_eq!(raw.len(), 61);
    let mut events = context.stream_subscribe();
    context.record_llm_call("mock-summary", 0, 0, 0, 0, None, None);
    atman_runtime::compaction::maybe_auto_compact(&context, "mock-summary", &executor.providers)
        .await;
    let compacted = context.messages().to_vec();
    assert!(compacted.len() < before.len());
    assert_eq!(context.messages_full().as_ref(), raw.as_ref());
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, StreamFrame::AssistantMsg { .. }),
            "checkpoint must not republish retained answers"
        );
    }
    assert!(
        context
            .commit_rewritten_window(
                &before,
                vec![Message::assistant_text(lease.turn.clone(), "stale")],
                60_000,
                before.len(),
            )
            .await
            .is_err()
    );
    assert_eq!(context.messages().to_vec(), compacted);
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    let mut flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.context_messages().unwrap(), compacted);
    assert_eq!(flow.history_messages().unwrap().as_slice(), raw.as_slice());
    assert_eq!(flow.context_checkpoints().len(), 1);
    assert_eq!(flow.published_messages().len(), raw.len());
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    let next = start(&mut flow);
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    assert_eq!(context.messages().to_vec(), compacted);
    assert_eq!(context.messages_full().as_ref(), raw.as_ref());
    assert!(context.stream_subscribe().try_recv().is_err());
    let file = atman_rt::parse_file(
        "flow main() -> string { return final.answer(message: \"after restart\") }",
    )
    .unwrap();
    executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(
                    atman_runtime::InstanceInvocation::new(
                        next.clone(),
                        authority,
                        Default::default(),
                    )
                    .with_storage(worker.clone())
                    .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::TurnFinished {
                lease: next.clone(),
                outcome: TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    let window_with_tail = context.messages().to_vec();
    let full_with_tail = context.messages_full();
    assert_eq!(window_with_tail.len(), compacted.len() + 1);
    assert_eq!(full_with_tail.len(), raw.len() + 1);
    let flow = worker.shutdown().await.unwrap();
    assert!(
        context
            .commit_rewritten_window(
                &window_with_tail,
                vec![Message::assistant_text(next.turn, "closed writer")],
                60_000,
                window_with_tail.len(),
            )
            .await
            .is_err()
    );
    assert_eq!(context.messages().to_vec(), window_with_tail);
    drop(flow);
    let worker = FlowWorker::start(session.open_flow(&description.address.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(worker.context().messages().to_vec(), window_with_tail);
    assert_eq!(
        worker.context().messages_full().as_ref(),
        full_with_tail.as_ref()
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_execution_separates_pre_admission_errors_from_interrupted_turns() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    for (source, admitted) in [
        ("flow main(input: int) { missing_tool() }", true),
        ("flow main(", false),
    ] {
        let mut description = descriptor(info.id);
        description
            .entry_mut()
            .unwrap()
            .bundle
            .sources
            .insert("entry:main.at".into(), source.into());
        let mut store = session.create_flow(description.clone()).unwrap();
        change(&mut store, FlowTransition::Activated);
        let worker = FlowWorker::start(store).await.unwrap();
        let input = worker
            .accept_input(
                delivery(
                    &description.address,
                    SenderIdentity::User(Uuid::new_v4()),
                    "run",
                ),
                |_| Ok(()),
            )
            .await
            .unwrap();
        let launched = worker
            .start_turn(
                atman_runtime::Executor::new(),
                turn_request(input.message_id, 1),
            )
            .await;
        if admitted {
            let end = launched.unwrap().wait().await.unwrap();
            assert!(end.result.is_err());
            assert_eq!(end.outcome, TurnOutcome::Interrupted(Interruption::Failed));
        } else {
            assert!(launched.is_err());
        }
        let store = worker.shutdown().await.unwrap();
        assert_eq!(store.committed_inputs().len(), usize::from(admitted));
        assert_eq!(store.pending_inputs().count(), usize::from(!admitted));
        assert_eq!(store.state().can_start_turn(), !admitted);
    }
}

#[test]
fn separated_journals_restore_peers_and_ignore_corrupt_metadata_without_touching_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let session_info = session_descriptor();
    let legacy_dir = dir
        .path()
        .join("sessions")
        .join(session_info.id.to_string());
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(legacy_dir.join("events.jsonl"), "legacy history\n").unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_info.clone()).unwrap();
    let a_info = descriptor(session_info.id);
    let mut b_info = descriptor(session_info.id);
    b_info.created_by = Some(a_info.address.clone());
    let mut a = session.create_flow(a_info.clone()).unwrap();
    let mut b = session.create_flow(b_info.clone()).unwrap();
    let a_path = a.dir().unwrap().to_path_buf();
    let session_path = session.dir().to_path_buf();
    assert_eq!(
        a_path,
        session_path
            .join("flows")
            .join(a_info.address.instance_id.to_string())
    );
    change(&mut a, FlowTransition::Activated);
    change(&mut b, FlowTransition::Activated);
    let lease = start(&mut a);
    change(
        &mut a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let pause = AtmanUuid(Uuid::new_v4());
    change(&mut a, FlowTransition::ExecutionPaused { key: pause });
    session
        .select_default(
            &session.cursor().clone(),
            b_info.address.instance_id.clone(),
        )
        .unwrap();
    let a_state = a.state().clone();
    let b_state = b.state().clone();
    drop((session, a, b));
    std::fs::write(a_path.join("flow.json"), "corrupt cache").unwrap();
    std::fs::write(session_path.join("session.json"), "corrupt cache").unwrap();
    let session = root.open_session(session_info.id).unwrap();
    let a = session.open_flow(&a_info.address.instance_id).unwrap();
    let b = session.open_flow(&b_info.address.instance_id).unwrap();
    assert_eq!(session.projection().flows.len(), 2);
    assert_eq!(
        session.projection().default_flow,
        Some(b_info.address.instance_id)
    );
    assert_eq!(a.descriptor(), &a_info);
    assert_eq!(a.state(), &a_state);
    assert_eq!(b.state(), &b_state);
    assert!(!a.state().can_start_turn());
    assert!(b.state().can_start_turn());
    assert!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(a_path.join("flow.json")).unwrap()
        )
        .is_ok()
    );
    assert_eq!(
        std::fs::read_to_string(legacy_dir.join("events.jsonl")).unwrap(),
        "legacy history\n"
    );
}

#[test]
fn restored_active_turn_is_interrupted_once_and_keeps_independent_pause_controls() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let old = start(&mut flow);
    let pause = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key: pause });
    let cursor = flow.cursor().sequence();
    drop((flow, session));
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.cursor().sequence(), cursor + 1);
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: Interruption::HostLost,
            ..
        })
    ));
    assert_eq!(flow.state().pauses(), &[pause]);
    assert!(flow.interrupt_lost_turn().unwrap().is_none());
    change(&mut flow, FlowTransition::InterruptionRecovered);
    assert!(!flow.state().can_start_turn());
    change(&mut flow, FlowTransition::ExecutionResumed { key: pause });
    let new = start(&mut flow);
    assert!(new.epoch > old.epoch);
    let before = flow.state().clone();
    assert!(
        flow.transition(
            flow.state().revision(),
            FlowTransition::TurnFinished {
                lease: old,
                outcome: TurnOutcome::Completed
            }
        )
        .is_err()
    );
    assert_eq!(flow.state(), &before);
}

#[test]
fn lost_closing_turn_still_requires_explicit_resource_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    start(&mut flow);
    change(&mut flow, FlowTransition::CloseRequested);
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.state().state(),
        &InstanceState::Closing { active: None }
    );
    assert!(!flow.state().accepts_input());
    change(&mut flow, FlowTransition::Closed);
    drop(flow);
    let flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.state().state(), &InstanceState::Closed);
}

#[test]
fn owner_lease_survives_session_handle_and_prevents_duplicate_writers() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let flow = session.create_flow(descriptor.clone()).unwrap();
    assert!(root.open_session(info.id).is_err());
    assert!(session.open_flow(&descriptor.address.instance_id).is_err());
    drop(session);
    assert!(root.open_session(info.id).is_err());
    drop(flow);
    assert!(root.open_session(info.id).is_ok());
}

#[test]
fn prepared_orphan_is_unaddressable_until_registration_and_retry_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let path = session.dir().join("events.jsonl");
    let before_registration = std::fs::read(&path).unwrap();
    let descriptor = descriptor(info.id);
    let flow = session.create_flow(descriptor.clone()).unwrap();
    drop((flow, session));
    // Simulate a crash after the flow's creation commit, before the session registration commit.
    std::fs::write(&path, before_registration).unwrap();
    let mut session = root.open_session(info.id).unwrap();
    assert!(session.projection().flows.is_empty());
    assert!(session.open_flow(&descriptor.address.instance_id).is_err());
    let flow = session.create_flow(descriptor.clone()).unwrap();
    assert_eq!(flow.state().state(), &InstanceState::Prepared);
    let receipt = session.cursor().clone();
    drop(flow);
    drop(session.create_flow(descriptor.clone()).unwrap());
    assert_eq!(session.cursor(), &receipt);
    let mut conflict = descriptor.clone();
    conflict.entry_mut().unwrap().name = "different".into();
    assert!(session.create_flow(conflict).is_err());
    let mut conflict = descriptor;
    conflict.address.instance_id = FlowInstanceId::now();
    assert!(session.create_flow(conflict).is_err());
    assert_eq!(session.projection().flows.len(), 1);
}

#[test]
fn stale_or_invalid_controls_do_not_commit_and_projection_failures_do_not_erase_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    let before = flow.cursor().clone();
    assert!(flow.transition(999, FlowTransition::Activated).is_err());
    assert!(flow.transition(0, FlowTransition::Closed).is_err());
    assert_eq!(flow.cursor(), &before);
    let cache = flow.dir().unwrap().join("flow.json");
    std::fs::remove_file(&cache).unwrap();
    std::fs::create_dir(&cache).unwrap();
    change(&mut flow, FlowTransition::Activated);
    assert!(flow.take_projection_error().is_some());
    let committed = flow.cursor().clone();
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.cursor(), &committed);
    assert!(flow.state().can_start_turn());
    assert!(flow.take_projection_error().is_some());
}

#[test]
fn peer_input_projection_preserves_model_context_and_replays_display_provenance() {
    use atman_runtime::{
        flow_store::inbox::context_message,
        message::{MessageOrigin, MessagePart, MessageRole},
        projection::flow::FlowTranscript,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_descriptor()).unwrap();
    let info = descriptor(session.projection().descriptor.id);
    let mut flow = session.create_flow(info.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let sender = FlowRef {
        session_id: info.address.session_id,
        instance_id: FlowInstanceId::now(),
    };
    let mut envelopes = Vec::new();
    for origin in [
        SenderIdentity::User(Uuid::new_v4()),
        SenderIdentity::Flow(sender.clone()),
        SenderIdentity::Flow(sender.clone()),
    ] {
        let accepted = flow
            .accept_input(delivery(&info.address, origin, "Explain the code"), |_| {
                Ok(())
            })
            .unwrap();
        envelopes.push(
            flow.inbox_entry(accepted.message_id)
                .unwrap()
                .unwrap()
                .envelope
                .clone(),
        );
    }
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let mut messages = envelopes
        .iter()
        .map(|envelope| context_message(envelope, &batch.lease().turn).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(messages[0].origin, MessageOrigin::Interjection);
    assert_eq!(messages[0].text_concat(), "Explain the code");
    for message in &messages[1..] {
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.origin, MessageOrigin::Peer);
        assert!(message.text_concat().contains("not user authorization"));
        assert!(message.parts.iter().any(|part| matches!(part, MessagePart::PeerInput { sender: source, text } if source == &sender && text == "Explain the code")));
    }
    // Earlier commits retained the full envelope but classified peer input as a human interjection.
    messages[1].origin = MessageOrigin::Interjection;
    messages[1]
        .parts
        .retain(|part| !matches!(part, MessagePart::PeerInput { .. }));
    flow.commit_input(
        &batch,
        InputContent::InvocationInput {
            command: None,
            arguments: BTreeMap::new(),
            invocation: BTreeMap::new(),
            messages: messages.clone(),
        },
    )
    .unwrap();
    let mut live = FlowTranscript::new(info.address.clone());
    let mut batches = Vec::new();
    loop {
        let page = flow
            .read_updates(live.cursor(), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        for batch in page.batches {
            batches.push(batch.clone());
            live.apply(batch.clone()).unwrap();
            let count = live.entries().len();
            assert_eq!(live.apply(batch).unwrap(), count..count);
        }
        if !page.has_more {
            break;
        }
    }
    let mut restored = FlowTranscript::new(info.address);
    for batch in batches {
        restored.apply(batch).unwrap();
    }
    for view in [live, restored] {
        let projected = view
            .entries()
            .iter()
            .flat_map(|entry| entry.record.shared_messages())
            .collect::<Vec<_>>();
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[0], &messages[0]);
        for (message, stored) in projected[1..].iter().zip(&messages[1..]) {
            assert_eq!(message.origin, MessageOrigin::Peer);
            assert_eq!(message.text_concat(), stored.text_concat());
            assert_eq!(
                message
                    .parts
                    .iter()
                    .filter(|part| matches!(part, MessagePart::PeerInput { .. }))
                    .count(),
                1
            );
        }
    }
    assert_eq!(flow.context_messages().unwrap(), messages);
}

fn delivery(recipient: &FlowRef, sender: SenderIdentity, text: &str) -> DeliveryRequest {
    DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender,
        recipient: recipient.clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection { text: text.into() },
    }
}

#[test]
fn inbox_retry_identity_is_shared_by_edits_replay_and_deleted_delivery() {
    use atman_runtime::flow_store::{
        SessionRemoval,
        inbox::{InboxChange, InboxEditRequest, UserContent},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let content = UserContent {
        text: "original body".into(),
        images: vec![],
        presentation: None,
    };
    let mut original = delivery(
        &description.address,
        SenderIdentity::User(Uuid::now_v7()),
        "",
    );
    original.delivery = DeliveryMode::NextTurn;
    original.payload = InboxPayload::UserSubmission {
        content: content.clone(),
        invocation: BTreeMap::from([(
            "options".into(),
            serde_json::from_str(r#"{"first": [1, -0.0], "second": {"a": 2, "b": 3}}"#).unwrap(),
        )]),
    };
    let accepted = flow.accept_input(original.clone(), |_| Ok(())).unwrap();
    let mut reordered = original.clone();
    if let InboxPayload::UserSubmission { invocation, .. } = &mut reordered.payload {
        invocation.insert(
            "options".into(),
            serde_json::from_str(r#"{"second": {"b": 3, "a": 2}, "first": [1, 0.0]}"#).unwrap(),
        );
    }
    assert_eq!(original, reordered);
    let edit = InboxEditRequest {
        id: Uuid::now_v7(),
        sender: original.sender.clone(),
        message: accepted.message_id,
        expected: accepted.receipt.clone(),
        change: InboxChange::Replace {
            content: UserContent {
                text: "replacement body".into(),
                ..content
            },
            delivery: DeliveryMode::AtBoundary,
        },
    };
    let edited = flow.edit_input(edit.clone(), |_| Ok(())).unwrap();
    let latest = flow
        .inbox_entry(accepted.message_id)
        .unwrap()
        .unwrap()
        .envelope
        .request;
    assert_eq!(
        flow.accept_input(reordered.clone(), |_| Ok(())).unwrap(),
        accepted
    );
    assert!(flow.accept_input(latest.clone(), |_| Ok(())).is_err());
    let mut conflict = edit.clone();
    conflict.change = InboxChange::Cancel;
    assert!(flow.edit_input(conflict.clone(), |_| Ok(())).is_err());
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.edit_input(edit, |_| Ok(())).unwrap(), edited);
    assert!(flow.edit_input(conflict, |_| Ok(())).is_err());
    assert_eq!(
        flow.accept_input(reordered.clone(), |_| Ok(())).unwrap(),
        accepted
    );
    assert!(flow.accept_input(latest.clone(), |_| Ok(())).is_err());
    change(&mut flow, FlowTransition::CloseRequested);
    change(&mut flow, FlowTransition::Closed);
    let final_acceptance = flow.accept_input(original, |_| Ok(())).unwrap();
    let removal = SessionRemoval {
        session_id: info.id,
        expected: session.cursor().clone(),
    };
    drop((flow, session));
    root.delete_session(&removal).unwrap();
    assert_eq!(
        RuntimeStore::inspect_deleted_delivery(data.path(), &reordered).unwrap(),
        Some(final_acceptance)
    );
    assert!(RuntimeStore::inspect_deleted_delivery(data.path(), &latest).is_err());
}

#[test]
fn inbox_body_reads_follow_replacements_independently_of_queue_revisions() {
    use atman_runtime::flow_store::inbox::{InboxChange, InboxEditRequest, InboxMove, UserContent};
    use atman_runtime::flow_store::inbox_view::{InboxHistoryItem, InboxSide};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let user = SenderIdentity::User(Uuid::now_v7());
    let mut request = delivery(&description.address, user.clone(), "original");
    request.delivery = DeliveryMode::NextTurn;
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    request.request_id = Uuid::now_v7();
    let second = flow.accept_input(request, |_| Ok(())).unwrap();
    let replaced = flow
        .edit_input(
            InboxEditRequest {
                id: Uuid::now_v7(),
                sender: user.clone(),
                message: first.message_id,
                expected: first.receipt,
                change: InboxChange::Replace {
                    content: UserContent {
                        text: "edited body".repeat(4096),
                        images: vec![],
                        presentation: None,
                    },
                    delivery: DeliveryMode::NextTurn,
                },
            },
            |_| Ok(()),
        )
        .unwrap();
    let moved = flow
        .edit_input(
            InboxEditRequest {
                id: Uuid::now_v7(),
                sender: user,
                message: first.message_id,
                expected: replaced,
                change: InboxChange::Move {
                    direction: InboxMove::Later,
                    expected_order: vec![first.message_id, second.message_id],
                },
            },
            |_| Ok(()),
        )
        .unwrap();
    let metadata = serde_json::to_string(flow.inbox_entries()).unwrap();
    assert!(!metadata.contains("edited body"));
    assert!(!metadata.contains("original"));
    assert!(metadata.len() < 4096);
    let expected: Vec<_> = flow
        .inbox_entries()
        .iter()
        .map(|entry| entry.envelope.id)
        .collect();
    let expected: Vec<_> = expected
        .into_iter()
        .map(|id| flow.inbox_entry(id).unwrap().unwrap())
        .collect();
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    let cursor = flow.cursor().clone();
    for entry in &expected {
        let read = flow.inbox_entry(entry.envelope.id).unwrap().unwrap();
        assert_eq!(read.envelope, entry.envelope);
        assert_eq!(read.acceptance, entry.acceptance);
        assert_eq!(read.revision, moved);
    }
    assert!(flow.inbox_entry(Uuid::now_v7()).unwrap().is_none());
    let read = RuntimeStore::inspect_session(dir.path(), info.id).unwrap();
    let page = read
        .inspect_inbox(
            &description.address.instance_id,
            InboxSide::Received,
            None,
            2,
        )
        .unwrap();
    for (item, entry) in page.items.iter().zip(expected.iter().rev()) {
        let InboxHistoryItem::Received(body) = item else {
            panic!("expected received input")
        };
        assert_eq!(body.envelope, entry.envelope);
        assert_eq!(body.revision, entry.revision);
    }
    assert_eq!(flow.cursor(), &cursor);
    std::fs::OpenOptions::new()
        .write(true)
        .open(flow.dir().unwrap().join("events.jsonl"))
        .unwrap()
        .set_len(0)
        .unwrap();
    assert!(flow.inbox_entry(first.message_id).is_err());
    assert!(
        flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
            .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.pending_inputs().count(), 2);
}

#[test]
fn inbox_edits_preserve_delivery_identity_order_and_receipts_across_recovery() {
    use atman_runtime::flow_store::inbox::{InboxChange, InboxEditRequest, InboxMove, UserContent};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let sender = SenderIdentity::User(Uuid::new_v4());
    let mut original = delivery(&description.address, sender.clone(), "first");
    original.delivery = DeliveryMode::NextTurn;
    let first = flow.accept_input(original.clone(), |_| Ok(())).unwrap();
    let background = flow
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(description.address.clone()),
                "notice",
            ),
            |_| Ok(()),
        )
        .unwrap();
    let mut second_request = original.clone();
    second_request.request_id = Uuid::new_v4();
    let second = flow
        .accept_input(second_request.clone(), |_| Ok(()))
        .unwrap();
    let state = flow.state().clone();
    let reorder = InboxEditRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        message: second.message_id,
        expected: second.receipt.clone(),
        change: InboxChange::Move {
            direction: InboxMove::Earlier,
            expected_order: vec![first.message_id, second.message_id],
        },
    };
    let moved = flow.edit_input(reorder.clone(), |_| Ok(())).unwrap();
    assert_eq!(
        flow.pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [second.message_id, background.message_id, first.message_id]
    );
    assert_eq!(flow.inbox_entries()[0].revision, moved);
    let replace = InboxEditRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        message: second.message_id,
        expected: moved.clone(),
        change: InboxChange::Replace {
            content: UserContent {
                text: "edited".into(),
                images: vec![],
                presentation: None,
            },
            delivery: DeliveryMode::AtBoundary,
        },
    };
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert!(flow.edit_input(replace.clone(), |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &moved);
    drop(reserved);
    let replaced = flow.edit_input(replace.clone(), |_| Ok(())).unwrap();
    assert_eq!(
        flow.accept_input(second_request.clone(), |_| panic!(
            "accepted delivery must not be readmitted"
        ))
        .unwrap(),
        second
    );
    let mut tampered = second_request.clone();
    tampered.payload = InboxPayload::Interjection {
        text: "edited".into(),
    };
    assert!(flow.accept_input(tampered, |_| Ok(())).is_err());
    let mut stale = replace.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.edit_input(stale, |_| Ok(())).is_err());
    assert_eq!(flow.state(), &state);
    let lease = start(&mut flow);
    let batch = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(
        batch.message_ids(),
        [second.message_id, background.message_id]
    );
    let messages = common::pending_bodies(&mut flow)
        .iter()
        .filter(|item| batch.message_ids().contains(&item.id))
        .map(|item| atman_runtime::flow_runtime::input::context_message(item, &lease.turn).unwrap())
        .collect();
    flow.commit_input(&batch, InputContent::Context { messages })
        .unwrap();
    assert!(matches!(
        flow.accept_input(second_request.clone(), |_| Ok(()))
            .unwrap()
            .disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    assert_eq!(
        flow.edit_input(replace.clone(), |_| Ok(())).unwrap(),
        replaced
    );
    let cancel = InboxEditRequest {
        id: Uuid::new_v4(),
        sender,
        message: first.message_id,
        expected: moved,
        change: InboxChange::Cancel,
    };
    let cancelled = flow.edit_input(cancel.clone(), |_| Ok(())).unwrap();
    assert!(flow.pending_inputs().next().is_none());
    assert!(
        flow.edit_input(cancel.clone(), |_| Err(std::io::Error::other("revoked")))
            .is_err()
    );
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let cursor = flow.cursor().clone();
    drop(flow);
    drop(session);
    let inspection = RuntimeStore::inspect_session(dir.path(), info.id).unwrap();
    assert_eq!(
        inspection
            .inspect_flow(&description.address.instance_id)
            .unwrap()
            .journal
            .cursor,
        Some(cursor.require_durable().unwrap().clone())
    );
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(
        flow.edit_input(reorder, |_| Ok(())).unwrap(),
        replace.expected
    );
    assert_eq!(flow.edit_input(replace, |_| Ok(())).unwrap(), replaced);
    assert_eq!(flow.edit_input(cancel, |_| Ok(())).unwrap(), cancelled);
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.pending_inputs().next().is_none());
    for entry in flow.inbox_entries().to_vec() {
        let body = flow.inbox_entry(entry.envelope.id).unwrap().unwrap();
        assert_eq!(body.envelope.id, entry.envelope.id);
        assert_eq!(body.acceptance, entry.acceptance);
        assert_eq!(body.revision, entry.revision);
    }
    assert!(matches!(
        flow.accept_input(original, |_| Ok(())).unwrap().disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert_eq!(
        flow.accept_input(second_request, |_| Ok(()))
            .unwrap()
            .receipt,
        second.receipt
    );
}

#[test]
fn inbox_edit_validation_preserves_configuration_and_rejects_replayed_mutations() {
    use atman_runtime::flow_store::inbox::{InboxChange, InboxEditRequest, InboxMove, UserContent};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    change(
        &mut flow,
        FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        },
    );
    let sender = SenderIdentity::User(Uuid::new_v4());
    let original_content = UserContent {
        text: "original".into(),
        images: vec![],
        presentation: None,
    };
    let invocation = BTreeMap::from([("effort".into(), serde_json::json!("high"))]);
    let mut request = delivery(&description.address, sender.clone(), "");
    request.delivery = DeliveryMode::NextTurn;
    request.payload = InboxPayload::UserSubmission {
        content: original_content.clone(),
        invocation: invocation.clone(),
    };
    let accepted = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let state = flow.state().clone();
    let edit = InboxEditRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        message: accepted.message_id,
        expected: accepted.receipt.clone(),
        change: InboxChange::Replace {
            content: UserContent {
                text: "changed".into(),
                ..original_content.clone()
            },
            delivery: DeliveryMode::AtBoundary,
        },
    };
    let mut invalid = edit.clone();
    invalid.sender = SenderIdentity::User(Uuid::new_v4());
    assert!(flow.edit_input(invalid, |_| Ok(())).is_err());
    let mut invalid = edit.clone();
    invalid.change = InboxChange::Move {
        direction: InboxMove::Later,
        expected_order: vec![accepted.message_id],
    };
    assert!(flow.edit_input(invalid, |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &accepted.receipt);
    let changed = flow.edit_input(edit.clone(), |_| Ok(())).unwrap();
    assert!(
        matches!(&common::pending_bodies(&mut flow)[0].request.payload, InboxPayload::UserSubmission { invocation: value, .. } if *value == invocation)
    );
    let restore = InboxEditRequest {
        id: Uuid::new_v4(),
        expected: changed,
        change: InboxChange::Replace {
            content: original_content,
            delivery: DeliveryMode::NextTurn,
        },
        ..edit.clone()
    };
    flow.edit_input(restore.clone(), |_| Ok(())).unwrap();
    let mut stale = edit.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.edit_input(stale, |_| Ok(())).is_err());
    let mut conflict = edit.clone();
    conflict.change = InboxChange::Cancel;
    assert!(flow.edit_input(conflict, |_| Ok(())).is_err());
    assert_eq!(flow.state(), &state);
    assert_eq!(flow.accept_input(request, |_| Ok(())).unwrap(), accepted);

    // Control input and peers cannot be rewritten as human-authored text.
    let notice = flow
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(description.address.clone()),
                "notice",
            ),
            |_| Ok(()),
        )
        .unwrap();
    assert!(
        flow.edit_input(
            InboxEditRequest {
                id: Uuid::new_v4(),
                message: notice.message_id,
                expected: notice.receipt,
                ..edit.clone()
            },
            |_| Ok(())
        )
        .is_err()
    );
    let path = flow.dir().unwrap().join("events.jsonl");
    drop(flow);
    let (mut journal, _) = atman_runtime::event_log::journal::Journal::open::<serde_json::Value>(
        &path,
        description.address.instance_id.0.0,
        |_, _| Ok(()),
    )
    .unwrap();
    journal
        .commit(&[serde_json::json!({"type": "inbox_edited", "data": restore})])
        .unwrap();
    drop(journal);
    let bytes = std::fs::read(&path).unwrap();
    let inspection_error = session
        .inspect_flow(&description.address.instance_id)
        .unwrap_err();
    let recovery_error = session
        .open_flow(&description.address.instance_id)
        .err()
        .unwrap();
    assert_eq!(inspection_error.to_string(), recovery_error.to_string());
    assert!(
        inspection_error
            .to_string()
            .contains("duplicate inbox edit")
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn committed_readiness_includes_inbox_arrival_without_spinning_on_reads_or_retries() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let mut observer = worker.subscribe_commits();
    let mut second_observer = observer.clone();
    observer.borrow_and_update();
    second_observer.borrow_and_update();
    let initial = worker.snapshot().await.unwrap();
    assert!(initial.pending.is_empty());
    assert!(initial.state.can_start_turn());
    assert!(!observer.has_changed().unwrap());

    // Delivery after the empty scan but before waiting must still wake both observers.
    let request = delivery(
        &description.address,
        SenderIdentity::User(Uuid::new_v4()),
        "arrived",
    );
    let receipt = worker
        .accept_input(request.clone(), |_| Ok(()))
        .await
        .unwrap();
    for observer in [&mut observer, &mut second_observer] {
        tokio::time::timeout(std::time::Duration::from_secs(5), observer.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*observer.borrow_and_update(), receipt.receipt);
    }
    let snapshot = worker.snapshot().await.unwrap();
    assert_eq!(snapshot.state, initial.state);
    assert_eq!(snapshot.cursor, receipt.receipt);
    assert_eq!(snapshot.pending[0].id, receipt.message_id);
    assert_eq!(
        worker
            .accept_input(request.clone(), |_| panic!("retry reauthorized"))
            .await
            .unwrap(),
        receipt
    );
    let mut conflict = request;
    conflict.payload = InboxPayload::Interjection {
        text: "conflict".into(),
    };
    assert!(worker.accept_input(conflict, |_| Ok(())).await.is_err());
    assert!(!observer.has_changed().unwrap());
    assert!(!second_observer.has_changed().unwrap());

    let key = Uuid::new_v4().into();
    let paused = worker
        .transition(
            snapshot.state.revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    // Commit notification is published before command acknowledgement, without needing another poll.
    assert!(observer.has_changed().unwrap());
    assert_eq!(*observer.borrow_and_update(), paused);
    let snapshot = worker.snapshot().await.unwrap();
    assert!(!snapshot.state.can_start_turn());
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(
        worker
            .transition(
                snapshot.state.revision(),
                FlowTransition::ExecutionPaused { key }
            )
            .await
            .unwrap(),
        paused
    );
    assert!(!observer.has_changed().unwrap());
    worker
        .transition(
            snapshot.state.revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    assert!(observer.has_changed().unwrap());
    observer.borrow_and_update();
    let resumed = worker.snapshot().await.unwrap();
    assert!(resumed.state.can_start_turn());
    assert_eq!(resumed.pending, snapshot.pending);
    let store = worker.shutdown().await.unwrap();
    assert!(observer.changed().await.is_err());
    assert_eq!(store.cursor(), &resumed.cursor);
    drop(store);
    let restored = FlowWorker::start(session.open_flow(&description.address.instance_id).unwrap())
        .await
        .unwrap();
    let latest = restored.snapshot().await.unwrap();
    assert_eq!(latest.pending, resumed.pending);
    assert_eq!(latest.cursor, resumed.cursor);
    assert_eq!(*restored.subscribe_commits().borrow(), latest.cursor);
    restored.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_scheduler_rechecks_preparation_races_and_runs_pending_turns_without_polling() {
    use atman_runtime::flow_runtime::FlowWorker;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert("entry:main.at".into(), r#"flow main(input: int) -> string { return final.answer(message: to_json_string(input)) }"#.into());
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let prepared = std::sync::Arc::new(AtomicUsize::new(0));
    let calls = prepared.clone();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let (release, held) = std::sync::mpsc::channel();
    let executor = atman_runtime::Executor::new();
    let scheduler = worker
        .schedule(executor.clone(), move |snapshot| {
            let count = calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = entered.take() {
                let _ = entered.send(());
                held.recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
            Ok(Some(turn_request(snapshot.pending[0].id, count as i64)))
        })
        .unwrap();
    assert!(worker.schedule(executor.clone(), |_| Ok(None)).is_err());
    assert_eq!(prepared.load(Ordering::SeqCst), 0);
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "one",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "two",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    release.send(()).unwrap();
    assert!(worker.context().messages().is_empty());
    let snapshot = worker.snapshot().await.unwrap();
    assert_eq!(snapshot.pending.len(), 2);
    assert!(!snapshot.state.can_start_turn());
    worker
        .transition(
            snapshot.state.revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.pending.is_empty() && snapshot.state.can_start_turn() {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(prepared.load(Ordering::SeqCst), 3);
    scheduler.stop().await.unwrap();
    let never = worker
        .schedule(executor, |_| panic!("empty inbox was scheduled"))
        .unwrap();
    never.stop().await.unwrap();
    let mut store = worker.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 2);
    let answers: Vec<_> = store
        .context_messages()
        .unwrap()
        .iter()
        .filter(|m| m.origin == atman_runtime::message::MessageOrigin::FinalAnswer)
        .map(Message::text_concat)
        .collect();
    assert_eq!(answers, ["1", "2"]);
}

#[tokio::test]
async fn instance_scheduler_stop_releases_admission_ownership_without_cancelling_a_turn() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert("entry:main.at".into(), r#"flow main(input: int) -> string { execution_probe(); return final.answer(message: "settled") }"#.replace(";", "\n"));
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let executor = atman_runtime::Executor::new();
    let probe = std::sync::Arc::new(ExecutionProbe {
        entered: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        release: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let scheduler = worker
        .schedule(executor.clone(), |snapshot| {
            Ok(Some(turn_request(snapshot.pending[0].id, 0)))
        })
        .unwrap();
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "one",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    scheduler.stop().await.unwrap();
    assert!(matches!(
        worker.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Active(_))
    ));
    let replacement = worker
        .schedule(executor, |_| {
            panic!("active turn must not be scheduled twice")
        })
        .unwrap();
    probe.release.add_permits(1);
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            if worker.state().await.unwrap().can_start_turn() {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    replacement.stop().await.unwrap();
    let mut store = worker.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 1);
    let publication = store.published_messages().last().unwrap().id;
    assert_eq!(
        store
            .published_message(publication)
            .unwrap()
            .unwrap()
            .publication
            .message
            .text_concat(),
        "settled"
    );
}

#[tokio::test]
async fn instance_scheduler_keeps_declined_input_and_never_retries_interrupted_execution() {
    use atman_runtime::flow_runtime::FlowWorker;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        "flow main(input: int) { unavailable_tool() }".into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let prepared = std::sync::Arc::new(AtomicUsize::new(0));
    let calls = prepared.clone();
    let declined = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = declined.clone();
    let scheduler = worker
        .schedule(atman_runtime::Executor::new(), move |snapshot| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                notify.notify_one();
                return Ok(None);
            }
            Ok(Some(turn_request(snapshot.pending[0].id, 0)))
        })
        .unwrap();
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "one",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), declined.notified())
        .await
        .unwrap();
    let snapshot = worker.snapshot().await.unwrap();
    assert_eq!(snapshot.pending.len(), 1);
    assert!(snapshot.state.can_start_turn());
    let key = Uuid::new_v4().into();
    worker
        .transition(
            snapshot.state.revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            if matches!(
                worker.state().await.unwrap().state(),
                InstanceState::Open(InstanceActivity::Interrupted {
                    reason: Interruption::Failed,
                    ..
                })
            ) {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "two",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    scheduler.stop().await.unwrap();
    assert_eq!(prepared.load(Ordering::SeqCst), 2);
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.pending_inputs().count(), 1);
    assert_eq!(store.committed_inputs().len(), 1);
}

#[test]
fn accepted_retries_survive_restart_revocation_and_close_without_duplicate_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let request = delivery(
        &descriptor.address,
        SenderIdentity::User(Uuid::new_v4()),
        "hello",
    );
    let accepted = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    assert_eq!(
        flow.accept_input(request.clone(), |_| panic!(
            "accepted retry must not reauthorize new delivery"
        ))
        .unwrap(),
        accepted
    );
    let mut conflict = request.clone();
    conflict.payload = InboxPayload::Interjection {
        text: "changed".into(),
    };
    assert!(flow.accept_input(conflict, |_| Ok(())).is_err());
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.accept_input(request.clone(), |_| panic!("retry after restart"))
            .unwrap(),
        accepted
    );
    change(&mut flow, FlowTransition::CloseRequested);
    let close_receipt = flow.cursor().clone();
    assert_eq!(flow.pending_inputs().count(), 0);
    let retried = flow
        .accept_input(request.clone(), |_| panic!("retry after close"))
        .unwrap();
    assert_eq!(retried.message_id, accepted.message_id);
    assert_eq!(retried.receipt, accepted.receipt);
    assert_eq!(
        retried.disposition,
        InboxDisposition::Cancelled { by: close_receipt }
    );
    let new = delivery(&descriptor.address, request.sender.clone(), "later");
    assert!(flow.accept_input(new, |_| Ok(())).is_err());
    change(&mut flow, FlowTransition::Closed);
    drop((flow, session));
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.accept_input(request, |_| panic!("closed retry"))
            .unwrap(),
        retried
    );
    assert_eq!(flow.inbox_entries().len(), 1);
}

#[test]
fn paused_and_interrupted_flows_keep_ordered_inputs_without_resuming_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    let sender = SenderIdentity::User(Uuid::new_v4());
    assert!(
        flow.accept_input(
            delivery(&descriptor.address, sender.clone(), "unprepared"),
            |_| Ok(())
        )
        .is_err()
    );
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    let first = flow
        .accept_input(
            delivery(&descriptor.address, sender.clone(), "first"),
            |_| Ok(()),
        )
        .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
        },
    );
    let second = flow
        .accept_input(delivery(&descriptor.address, sender, "second"), |_| Ok(()))
        .unwrap();
    assert!(!flow.state().can_start_turn());
    drop(flow);
    let flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [first.message_id, second.message_id]
    );
    assert!(!flow.state().can_start_turn());
    assert_eq!(flow.state().pauses(), &[key]);
}

#[test]
fn delivery_identity_scope_and_authorization_reject_invalid_new_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let sender = SenderIdentity::Flow(FlowRef {
        session_id: info.id.into(),
        instance_id: FlowInstanceId::now(),
    });
    let request = delivery(&descriptor.address, sender, "peer data");
    let before = flow.cursor().clone();
    assert!(
        flow.accept_input(request.clone(), |_| Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "revoked"
        )))
        .is_err()
    );
    assert_eq!(flow.cursor(), &before);
    let mut impersonation = request.clone();
    impersonation.payload = InboxPayload::UserSubmission {
        content: atman_runtime::flow_store::inbox::UserContent {
            text: "user authority".into(),
            images: Vec::new(),
            presentation: None,
        },
        invocation: BTreeMap::new(),
    };
    assert!(
        flow.accept_input(impersonation, |_| panic!(
            "invalid source passed validation"
        ))
        .is_err()
    );
    let mut impersonation = request.clone();
    impersonation.delivery = DeliveryMode::NextTurn;
    impersonation.payload = InboxPayload::Invocation {
        arguments: BTreeMap::new(),
        invocation: BTreeMap::new(),
        user: Some(atman_runtime::flow_store::inbox::UserContent {
            text: "user authority".into(),
            images: vec![],
            presentation: None,
        }),
    };
    assert!(
        flow.accept_input(impersonation.clone(), |_| panic!(
            "invalid source authorized"
        ))
        .is_err()
    );
    let envelope = atman_runtime::flow_store::inbox::AcceptedEnvelope {
        id: Uuid::new_v4(),
        accepted_at: chrono::Utc::now(),
        request: impersonation,
    };
    assert!(
        atman_runtime::flow_runtime::input::context_message(&envelope, &TurnId::now()).is_err()
    );
    assert_eq!(flow.cursor(), &before);
    let mut foreign = request.clone();
    foreign.recipient.instance_id = FlowInstanceId::now();
    assert!(flow.accept_input(foreign, |_| Ok(())).is_err());
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut other_sender = request;
    other_sender.sender = SenderIdentity::User(Uuid::new_v4());
    let second = flow.accept_input(other_sender, |_| Ok(())).unwrap();
    assert_ne!(first.message_id, second.message_id);
    assert_eq!(flow.pending_inputs().count(), 2);
}

fn invocation_content(lease: &TurnLease<AtmanUuid>) -> InputContent {
    InputContent::InvocationInput {
        command: None,
        arguments: BTreeMap::from([("input".into(), serde_json::json!("accepted input"))]),
        invocation: BTreeMap::from([("effort".into(), serde_json::json!("high"))]),
        messages: vec![Message::user_text(
            lease.turn.clone(),
            "exact projected input",
        )],
    }
}

fn context_content(lease: &TurnLease<AtmanUuid>) -> InputContent {
    InputContent::Context {
        messages: vec![Message::user_text(lease.turn.clone(), "boundary input")],
    }
}

#[test]
fn command_sources_require_user_authority_and_commit_the_selected_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let mut request = delivery(
        &description.address,
        SenderIdentity::User(Uuid::now_v7()),
        "command",
    );
    request.delivery = DeliveryMode::NextTurn;
    request.payload = InboxPayload::Command {
        entry: Box::new(description.entry().unwrap().clone()),
        arguments: BTreeMap::new(),
        invocation: BTreeMap::new(),
        user: Some(atman_runtime::flow_store::inbox::UserContent {
            text: "/main".into(),
            images: vec![],
            presentation: None,
        }),
    };
    let before = flow.cursor().clone();
    for sender in [
        SenderIdentity::Flow(description.address.clone()),
        SenderIdentity::Host(Uuid::now_v7()),
    ] {
        let mut forged = request.clone();
        forged.sender = sender;
        assert!(flow.accept_input(forged, |_| Ok(())).is_err());
    }
    let mut injection = request.clone();
    injection.delivery = DeliveryMode::AtBoundary;
    if let InboxPayload::Command { user, .. } = &mut injection.payload {
        *user = None;
    }
    assert!(flow.accept_input(injection, |_| Ok(())).is_err());
    let mut peer_command = request.clone();
    peer_command.sender = SenderIdentity::Flow(description.address.clone());
    if let InboxPayload::Command { user, .. } = &mut peer_command.payload {
        *user = None;
    }
    assert!(flow.accept_input(peer_command, |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &before);
    let receipt = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    assert_eq!(
        flow.accept_input(request, |_| panic!(
            "retry must not reauthorize accepted input"
        ))
        .unwrap()
        .message_id,
        receipt.message_id
    );
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    let content = |command| InputContent::InvocationInput {
        command,
        arguments: BTreeMap::new(),
        invocation: BTreeMap::new(),
        messages: vec![Message::user_text(lease.turn.clone(), "/main")],
    };
    assert!(flow.commit_input(&reserved, content(None)).is_err());
    assert!(
        flow.commit_input(&reserved, content(Some(Uuid::now_v7())))
            .is_err()
    );
    flow.commit_input(&reserved, content(Some(receipt.message_id)))
        .unwrap();
    assert!(flow.pending_inputs().next().is_none());
    drop(flow);
    drop(session);
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(
        matches!(flow.last_invocation_input().unwrap().unwrap().input.content, InputContent::InvocationInput { command: Some(id), .. } if id == receipt.message_id)
    );
    assert!(flow.pending_inputs().next().is_none());
}

#[test]
fn input_consumption_and_exact_content_survive_restart_without_requeue_or_duplicate_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let request = delivery(
        &descriptor.address,
        SenderIdentity::User(Uuid::new_v4()),
        "first",
    );
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut later = request.clone();
    later.request_id = Uuid::new_v4();
    later.delivery = DeliveryMode::NextTurn;
    let second = flow.accept_input(later, |_| Ok(())).unwrap();
    let before = flow.cursor().sequence();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |envelope| {
            envelope.id == first.message_id
        })
        .unwrap()
        .unwrap();
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.cursor().sequence(), before);
    let lease = batch.lease().clone();
    let actual = invocation_content(&lease);
    let receipt = flow.commit_input(&batch, actual.clone()).unwrap();
    assert_eq!(receipt.sequence(), before + 1);
    assert!(!flow.state().can_start_turn());
    assert!(flow.commit_input(&batch, actual.clone()).is_err());
    assert!(
        flow.reserve_context_input(&lease, |_| true)
            .unwrap()
            .is_none()
    );
    let third = flow
        .accept_input(
            delivery(&descriptor.address, request.sender.clone(), "third"),
            |_| Ok(()),
        )
        .unwrap();
    let fourth = flow
        .accept_input(
            delivery(&descriptor.address, request.sender.clone(), "fourth"),
            |_| Ok(()),
        )
        .unwrap();
    let batch = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[third.message_id, fourth.message_id]);
    flow.commit_input(&batch, context_content(&lease)).unwrap();
    assert_eq!(
        flow.pending_inputs()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        [second.message_id]
    );
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.committed_inputs().len(), 2);
    assert_eq!(
        flow.committed_inputs()[1].message_ids,
        [third.message_id, fourth.message_id]
    );
    assert_eq!(
        flow.committed_input(&flow.committed_inputs()[0].receipt.clone())
            .unwrap()
            .unwrap()
            .input
            .content,
        actual
    );
    assert_eq!(flow.committed_inputs()[0].receipt, receipt);
    assert_eq!(
        flow.pending_inputs()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        [second.message_id]
    );
    let retried = flow
        .accept_input(request, |_| {
            panic!("accepted retry must not be reauthorized")
        })
        .unwrap();
    assert_eq!(retried.receipt, first.receipt);
    assert_eq!(
        retried.disposition,
        InboxDisposition::InputCommitted { by: receipt }
    );
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    change(&mut flow, FlowTransition::CloseRequested);
    assert!(matches!(
        flow.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    assert!(matches!(
        flow.inbox_entries()[1].acceptance.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert_eq!(flow.committed_inputs().len(), 2);
}

#[test]
fn execution_changes_revoke_reservations_without_late_release_or_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &descriptor.address,
            SenderIdentity::User(Uuid::new_v4()),
            "input",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let lease = start(&mut flow);
    let old = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(flow.commit_input(&old, context_content(&lease)).is_err());
    assert!(flow.reserve_context_input(&lease, |_| true).is_err());
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    assert!(!flow.release_input(&old));
    assert!(flow.commit_input(&old, context_content(&lease)).is_err());
    assert!(
        flow.reserve_context_input(&lease, |_| true)
            .unwrap()
            .is_none()
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: TurnPhase::Finishing,
        },
    );
    assert!(
        flow.commit_input(&current, context_content(&lease))
            .is_err()
    );
    assert!(flow.reserve_context_input(&lease, |_| true).is_err());
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: lease.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert_ne!(next.lease().epoch, lease.epoch);
    assert!(!flow.release_input(&current));
    flow.commit_input(&next, invocation_content(next.lease()))
        .unwrap();
    assert!(flow.pending_inputs().next().is_none());
}

#[test]
fn invalid_or_abandoned_input_does_not_start_a_turn_or_consume_messages() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &descriptor.address,
            SenderIdentity::User(Uuid::new_v4()),
            "input",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let cursor = flow.cursor().clone();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert!(
        flow.commit_input(&batch, context_content(batch.lease()))
            .is_err()
    );
    let mut wrong_turn = batch.lease().clone();
    wrong_turn.turn = TurnId::now();
    assert!(
        flow.commit_input(&batch, invocation_content(&wrong_turn))
            .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.pending_inputs().count(), 1);
    drop(batch);
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(
        flow.commit_input(&next, invocation_content(next.lease()))
            .is_err()
    );
    assert!(
        flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
            .is_err()
    );
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert!(flow.release_input(&next));
    assert!(
        flow.commit_input(&next, invocation_content(next.lease()))
            .is_err()
    );
    assert!(flow.state().can_start_turn());
}

#[test]
fn torn_input_batch_restores_pending_messages_without_a_ghost_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let accepted = flow
        .accept_input(
            delivery(
                &descriptor.address,
                SenderIdentity::User(Uuid::new_v4()),
                "input",
            ),
            |_| Ok(()),
        )
        .unwrap();
    let journal = flow.dir().unwrap().join("events.jsonl");
    let prefix = std::fs::metadata(&journal).unwrap().len();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    flow.commit_input(&batch, invocation_content(batch.lease()))
        .unwrap();
    drop(flow);
    let length = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), prefix);
    assert!(flow.state().can_start_turn());
    assert!(flow.committed_inputs().is_empty());
    assert_eq!(
        flow.pending_inputs().next().unwrap().id,
        accepted.message_id
    );
    assert!(
        flow.commit_input(&batch, invocation_content(batch.lease()))
            .is_err()
    );
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    flow.commit_input(&next, invocation_content(next.lease()))
        .unwrap();
    assert_eq!(flow.committed_inputs().len(), 1);
}

fn waiting_flow() -> (
    tempfile::TempDir,
    atman_runtime::flow_store::SessionStore,
    FlowStore,
    ReplyWaiter,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut flow = session.create_flow(descriptor(info.id)).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease,
        sender: SenderIdentity::Flow(FlowRef {
            session_id: info.id.into(),
            instance_id: FlowInstanceId::now(),
        }),
    };
    flow.register_reply_waiter(waiter.clone()).unwrap();
    (dir, session, flow, waiter)
}

fn reply_request(flow: &FlowStore, waiter: &ReplyWaiter) -> DeliveryRequest {
    let mut request = delivery(&flow.descriptor().address, waiter.sender.clone(), "reply");
    request.correlation = Some(waiter.correlation);
    request
}

fn wait_phase(flow: &mut FlowStore, waiter: &ReplyWaiter) {
    change(
        flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Waiting(atman_rt::WaitReason::Reply),
        },
    );
}

fn reply_content(waiter: &ReplyWaiter) -> InputContent {
    InputContent::WaitingContinuation {
        correlation: waiter.correlation,
        value: serde_json::json!({ "result": 42 }),
    }
}

#[test]
fn replies_share_inbox_ownership_and_cannot_be_stolen_by_context_or_foreign_senders() {
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let request = reply_request(&flow, &waiter);
    let mut foreign = request.clone();
    foreign.sender = SenderIdentity::User(Uuid::new_v4());
    let unrelated = flow.accept_input(foreign, |_| Ok(())).unwrap();
    let mut deferred = request.clone();
    deferred.request_id = Uuid::new_v4();
    deferred.delivery = DeliveryMode::NextTurn;
    let deferred = flow.accept_input(deferred, |_| Ok(())).unwrap();
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let extra = flow
        .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
        .unwrap();
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.message_ids(), &[unrelated.message_id]);
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .unwrap()
            .is_none()
    );
    wait_phase(&mut flow, &waiter);
    assert!(!flow.release_input(&ordinary));
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| false)
            .unwrap()
            .is_none(),
        "an ineligible reply must remain available for a later boundary"
    );
    let batch = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[first.message_id]);
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .unwrap()
            .is_none()
    );
    let before = flow.cursor().clone();
    let wrong = InputContent::WaitingContinuation {
        correlation: Uuid::new_v4(),
        value: serde_json::json!(42),
    };
    assert!(flow.commit_input(&batch, wrong).is_err());
    assert_eq!(flow.cursor(), &before);
    let receipt = flow.commit_input(&batch, reply_content(&waiter)).unwrap();
    assert_eq!(
        flow.reply_waiters()[0].status,
        ReplyStatus::Delivered {
            message_id: first.message_id,
            by: receipt.clone()
        }
    );
    assert_eq!(flow.committed_inputs()[0].lease, waiter.lease);
    assert_eq!(
        flow.committed_input(&flow.committed_inputs()[0].receipt.clone())
            .unwrap()
            .unwrap()
            .input
            .content,
        reply_content(&waiter)
    );
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .is_err()
    );
    assert_eq!(
        flow.accept_input(request, |_| panic!("retry"))
            .unwrap()
            .disposition,
        InboxDisposition::InputCommitted { by: receipt }
    );
    assert_eq!(
        flow.pending_inputs()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        [unrelated.message_id, deferred.message_id, extra.message_id]
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Running,
        },
    );
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(
        ordinary.message_ids(),
        &[unrelated.message_id, extra.message_id]
    );
}

#[test]
fn paused_and_cancelled_waiters_retain_replies_and_reject_late_commits() {
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let accepted = flow
        .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
        .unwrap();
    wait_phase(&mut flow, &waiter);
    let old = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(flow.commit_input(&old, reply_content(&waiter)).is_err());
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .is_err()
    );
    assert_eq!(flow.reply_waiters()[0].status, ReplyStatus::Pending);
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
        .unwrap()
        .unwrap();
    let cancelled = flow
        .cancel_reply_waiter(&waiter.lease, waiter.correlation)
        .unwrap();
    assert!(matches!(cancelled.status, ReplyStatus::Cancelled { .. }));
    assert!(flow.commit_input(&current, reply_content(&waiter)).is_err());
    assert_eq!(
        flow.register_reply_waiter(waiter.clone()).unwrap(),
        cancelled
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Running,
        },
    );
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.message_ids(), &[accepted.message_id]);
    assert!(!flow.release_input(&old));
    assert!(!flow.release_input(&current));
    flow.commit_input(&ordinary, context_content(&waiter.lease))
        .unwrap();
    let mut reused = waiter.clone();
    reused.sender = SenderIdentity::User(Uuid::new_v4());
    assert!(flow.register_reply_waiter(reused).is_err());
}

#[test]
fn lost_waiting_turn_releases_uncommitted_replies_but_retains_delivered_input() {
    for committed in [false, true] {
        let (_dir, session, mut flow, waiter) = waiting_flow();
        wait_phase(&mut flow, &waiter);
        let accepted = flow
            .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
            .unwrap();
        let batch = flow
            .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .unwrap()
            .unwrap();
        if committed {
            flow.commit_input(&batch, reply_content(&waiter)).unwrap();
        }
        let id = flow.descriptor().address.instance_id.clone();
        drop(flow);
        let mut flow = session.open_flow(&id).unwrap();
        assert!(matches!(
            flow.state().state(),
            InstanceState::Open(InstanceActivity::Interrupted {
                reason: Interruption::HostLost,
                ..
            })
        ));
        assert!(flow.commit_input(&batch, reply_content(&waiter)).is_err());
        if committed {
            assert!(matches!(
                flow.reply_waiters()[0].status,
                ReplyStatus::Delivered { .. }
            ));
            assert!(flow.pending_inputs().next().is_none());
            assert_eq!(
                flow.committed_input(&flow.committed_inputs()[0].receipt.clone())
                    .unwrap()
                    .unwrap()
                    .input
                    .content,
                reply_content(&waiter)
            );
        } else {
            assert!(matches!(
                flow.reply_waiters()[0].status,
                ReplyStatus::Cancelled { .. }
            ));
            assert_eq!(
                flow.pending_inputs().next().unwrap().id,
                accepted.message_id
            );
            change(&mut flow, FlowTransition::InterruptionRecovered);
            let next = flow
                .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
                .unwrap()
                .unwrap();
            assert_eq!(next.message_ids(), &[accepted.message_id]);
            flow.commit_input(&next, invocation_content(next.lease()))
                .unwrap();
        }
    }
}

#[test]
fn terminal_turn_transitions_cancel_waits_and_late_registration_cannot_capture_old_replies() {
    for close in [false, true] {
        let (_dir, _session, mut flow, waiter) = waiting_flow();
        wait_phase(&mut flow, &waiter);
        flow.accept_input(reply_request(&flow, &waiter), |_| Ok(()))
            .unwrap();
        let batch = flow
            .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
            .unwrap()
            .unwrap();
        if close {
            change(&mut flow, FlowTransition::CloseRequested);
            assert_eq!(flow.pending_inputs().count(), 0);
        } else {
            change(
                &mut flow,
                FlowTransition::PhaseChanged {
                    lease: waiter.lease.clone(),
                    phase: TurnPhase::Finishing,
                },
            );
            assert_eq!(flow.pending_inputs().count(), 1);
        }
        assert!(matches!(
            flow.reply_waiters()[0].status,
            ReplyStatus::Cancelled { .. }
        ));
        assert!(flow.commit_input(&batch, reply_content(&waiter)).is_err());
        assert!(
            flow.reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
                .is_err()
        );
    }
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let mut late = waiter.clone();
    late.correlation = Uuid::new_v4();
    flow.accept_input(reply_request(&flow, &late), |_| Ok(()))
        .unwrap();
    let before = flow.cursor().clone();
    assert!(flow.register_reply_waiter(late).is_err());
    assert_eq!(flow.cursor(), &before);
}

#[test]
fn independent_waiters_use_one_fifo_without_releasing_each_others_reply() {
    let (_dir, _session, mut flow, first) = waiting_flow();
    let mut second = first.clone();
    second.correlation = Uuid::new_v4();
    let registered = flow.register_reply_waiter(second.clone()).unwrap();
    assert_eq!(
        flow.register_reply_waiter(second.clone()).unwrap(),
        registered
    );
    // One waiting branch must not require all other branches to stop running.
    let second_message = flow
        .accept_input(reply_request(&flow, &second), |_| Ok(()))
        .unwrap();
    let first_message = flow
        .accept_input(reply_request(&flow, &first), |_| Ok(()))
        .unwrap();
    let batch = flow
        .reserve_reply_input(&first.lease, first.correlation, |_| true)
        .unwrap()
        .unwrap();
    assert!(
        flow.reserve_reply_input(&second.lease, second.correlation, |_| true)
            .unwrap()
            .is_none()
    );
    drop(batch);
    let batch = flow
        .reserve_reply_input(&second.lease, second.correlation, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[second_message.message_id]);
    flow.commit_input(&batch, reply_content(&second)).unwrap();
    assert_eq!(flow.reply_waiters()[0].status, ReplyStatus::Pending);
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: first.lease.clone(),
            phase: TurnPhase::Waiting(atman_rt::WaitReason::Effect),
        },
    );
    let batch = flow
        .reserve_reply_input(&first.lease, first.correlation, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[first_message.message_id]);
    flow.commit_input(&batch, reply_content(&first)).unwrap();
    assert!(flow.pending_inputs().next().is_none());
    assert_eq!(flow.committed_inputs().len(), 2);
}

#[test]
fn outbox_keeps_body_free_state_and_loads_full_intents_from_the_committed_journal() {
    use atman_runtime::flow_store::inbox_view::{InboxHistoryItem, InboxSide};
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let mut records = Vec::new();
    for label in ["first", "second", "third"] {
        let request = delivery(
            &description.address,
            SenderIdentity::Flow(description.address.clone()),
            &format!("{label}:{}", "body data ".repeat(8192)),
        );
        records.push(flow.queue_send(&lease, request, None, |_| Ok(())).unwrap());
    }
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let cursor = flow.cursor().clone();
    let expected: Vec<_> = records.iter().map(SendSummary::from).collect();
    assert_eq!(flow.sends(), expected);
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.pending_sends().cloned().collect::<Vec<_>>(), expected);
    for record in &records {
        assert_eq!(
            flow.send_record(record.intent.request.request_id)
                .unwrap()
                .as_ref(),
            Some(record)
        );
    }
    let inspection = RuntimeStore::inspect_session(data.path(), info.id).unwrap();
    let first = inspection
        .inspect_inbox(&description.address.instance_id, InboxSide::Sent, None, 1)
        .unwrap();
    assert_eq!(first.total, 3);
    assert!(matches!(&first.items[0], InboxHistoryItem::Sent(record) if **record == records[2]));
    let next = inspection
        .inspect_inbox(
            &description.address.instance_id,
            InboxSide::Sent,
            first.next.as_ref(),
            1,
        )
        .unwrap();
    assert!(matches!(&next.items[0], InboxHistoryItem::Sent(record) if **record == records[1]));
    assert_eq!(flow.cursor(), &cursor);

    // Metadata remains inspectable, but a missing disk body must never be served from a cache.
    std::fs::OpenOptions::new()
        .write(true)
        .open(flow.dir().unwrap().join("events.jsonl"))
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(flow.sends(), expected);
    assert!(
        flow.send_record(records[0].intent.request.request_id)
            .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.pending_sends().count(), records.len());
}

#[test]
fn cross_session_delivery_reconciles_lost_ack_after_restart_and_closure() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let a_info = session_descriptor();
    let b_info = session_descriptor();
    let mut a_session = root.create_session(a_info.clone()).unwrap();
    let mut b_session = root.create_session(b_info.clone()).unwrap();
    let mut a = a_session.create_flow(descriptor(a_info.id)).unwrap();
    let mut b = b_session.create_flow(descriptor(b_info.id)).unwrap();
    change(&mut a, FlowTransition::Activated);
    change(&mut b, FlowTransition::Activated);
    let lease = start(&mut a);
    let request = delivery(
        &b.descriptor().address,
        SenderIdentity::Flow(a.descriptor().address.clone()),
        "cross-session task",
    );
    let queued = a
        .queue_send(&lease, request.clone(), None, |_| Ok(()))
        .unwrap();
    assert_eq!(queued.status, SendStatus::PendingDelivery);
    let accepted = b.accept_input(request.clone(), |_| Ok(())).unwrap();
    let a_id = a.descriptor().address.instance_id.clone();
    let b_id = b.descriptor().address.instance_id.clone();
    drop((a, b));
    let mut a = a_session.open_flow(&a_id).unwrap();
    let mut b = b_session.open_flow(&b_id).unwrap();
    assert_eq!(
        a.pending_sends().next().unwrap(),
        &SendSummary::from(&queued)
    );
    assert_eq!(
        a.send_record(request.request_id).unwrap(),
        Some(queued.clone())
    );
    assert_eq!(
        a.queue_send(&lease, request.clone(), None, |_| panic!("send retry"))
            .unwrap(),
        queued
    );
    change(&mut a, FlowTransition::CloseRequested);
    change(&mut b, FlowTransition::CloseRequested);
    let replayed = b
        .accept_input(request.clone(), |_| {
            panic!("already accepted before revocation")
        })
        .unwrap();
    assert_eq!(replayed.message_id, accepted.message_id);
    assert_eq!(replayed.receipt, accepted.receipt);
    assert!(matches!(
        replayed.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    let ack = DeliveryReceipt::from(&replayed);
    let mut foreign = ack.clone();
    durable_position(&mut foreign.receipt).journal_id = Uuid::new_v4();
    let before = a.cursor().clone();
    assert!(a.confirm_delivery(request.request_id, foreign).is_err());
    assert_eq!(a.cursor(), &before);
    let confirmed = a.confirm_delivery(request.request_id, ack.clone()).unwrap();
    assert!(a.pending_sends().next().is_none());
    assert_eq!(
        a.confirm_delivery(request.request_id, ack.clone()).unwrap(),
        confirmed
    );
    let mut conflicting = ack;
    conflicting.message_id = Uuid::new_v4();
    assert!(a.confirm_delivery(request.request_id, conflicting).is_err());
    drop(a);
    let mut a = a_session.open_flow(&a_id).unwrap();
    assert_eq!(a.sends(), &[SendSummary::from(&confirmed)]);
    assert_eq!(
        a.send_record(request.request_id).unwrap(),
        Some(confirmed.clone())
    );
    assert_eq!(
        a.queue_send(&lease, request.clone(), None, |_| panic!("closed retry"))
            .unwrap(),
        confirmed
    );
    let mut changed = request.clone();
    changed.payload = InboxPayload::Interjection {
        text: "different task".into(),
    };
    assert!(a.queue_send(&lease, changed, None, |_| Ok(())).is_err());
    let mut new = request;
    new.request_id = Uuid::new_v4();
    assert!(a.queue_send(&lease, new, None, |_| Ok(())).is_err());
    assert_eq!(b.inbox_entries().len(), 1);
}

#[test]
fn request_registration_and_send_share_one_commit_and_peers_can_reply_in_the_same_turn() {
    let (_dir, mut session, mut a, initial) = waiting_flow();
    let mut b = session
        .create_flow(descriptor(a.descriptor().address.session_id.0))
        .unwrap();
    change(&mut b, FlowTransition::Activated);
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease: initial.lease.clone(),
        sender: SenderIdentity::Flow(b.descriptor().address.clone()),
    };
    let mut request = delivery(
        &b.descriptor().address,
        SenderIdentity::Flow(a.descriptor().address.clone()),
        "request",
    );
    request.correlation = Some(waiter.correlation);
    let before = a.cursor().clone();
    assert!(
        a.queue_send(&waiter.lease, request.clone(), Some(waiter.clone()), |_| {
            Err(std::io::Error::other("denied"))
        })
        .is_err()
    );
    assert_eq!(a.cursor(), &before);
    assert_eq!(a.reply_waiters().len(), 1);
    assert!(a.sends().is_empty());
    let queued = a
        .queue_send(&waiter.lease, request.clone(), Some(waiter.clone()), |_| {
            Ok(())
        })
        .unwrap();
    assert_eq!(queued.queued.sequence(), before.sequence() + 1);
    let registered = a
        .reply_waiters()
        .iter()
        .find(|record| record.waiter == waiter)
        .unwrap();
    assert_eq!(registered.registered, queued.queued);
    let mut aliased = request.clone();
    aliased.request_id = Uuid::new_v4();
    assert!(
        a.queue_send(&waiter.lease, aliased, Some(waiter.clone()), |_| Ok(()))
            .is_err()
    );
    assert_eq!(a.cursor(), &queued.queued);
    let accepted = b.accept_input(request.clone(), |_| Ok(())).unwrap();
    a.confirm_delivery(request.request_id, DeliveryReceipt::from(&accepted))
        .unwrap();
    wait_phase(&mut a, &waiter);
    let b_lease = start(&mut b);
    let mut reply = delivery(
        &a.descriptor().address,
        SenderIdentity::Flow(b.descriptor().address.clone()),
        "result",
    );
    reply.correlation = Some(waiter.correlation);
    b.queue_send(&b_lease, reply.clone(), None, |_| Ok(()))
        .unwrap();
    let accepted_reply = a.accept_input(reply.clone(), |_| Ok(())).unwrap();
    b.confirm_delivery(reply.request_id, DeliveryReceipt::from(&accepted_reply))
        .unwrap();
    let batch = a
        .reserve_reply_input(&waiter.lease, waiter.correlation, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[accepted_reply.message_id]);
    a.commit_input(&batch, reply_content(&waiter)).unwrap();
    assert_eq!(a.committed_inputs()[0].lease, waiter.lease);
    assert!(a.pending_sends().next().is_none());
    assert!(b.pending_sends().next().is_none());
}

#[test]
fn torn_send_batch_cannot_leave_a_registered_wait_without_its_send_intent() {
    let (_dir, session, mut flow, first) = waiting_flow();
    let mut waiter = first.clone();
    waiter.correlation = Uuid::new_v4();
    let SenderIdentity::Flow(target) = &waiter.sender else {
        unreachable!()
    };
    let mut request = delivery(
        target,
        SenderIdentity::Flow(flow.descriptor().address.clone()),
        "request",
    );
    request.correlation = Some(waiter.correlation);
    let path = flow.dir().unwrap().join("events.jsonl");
    flow.queue_send(&waiter.lease, request, Some(waiter.clone()), |_| Ok(()))
        .unwrap();
    let id = flow.descriptor().address.instance_id.clone();
    drop(flow);
    let length = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let flow = session.open_flow(&id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert!(flow.sends().is_empty());
    assert!(
        !flow
            .reply_waiters()
            .iter()
            .any(|record| record.waiter.correlation == waiter.correlation)
    );
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
}

#[tokio::test]
async fn recorded_source_and_committed_input_execute_with_the_persistent_instance_binding() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry_mut().unwrap().bundle.sources.insert("entry:main.at".into(),
        "flow helper(input: int) -> int { return input * 2 } flow main(input: int) -> int { when env(\"marker\") != \"persisted\" { return 0 } return helper(input).await }".into());
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "execute",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let program = atman_runtime::source_program::load_program_from_bundle(
        &description.entry().unwrap().bundle,
        &description.entry().unwrap().path,
        &Default::default(),
    )
    .unwrap();
    let reservation = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reservation.lease().clone();
    flow.commit_input(
        &reservation,
        InputContent::InvocationInput {
            command: None,
            arguments: BTreeMap::from([("input".into(), serde_json::json!(21))]),
            invocation: BTreeMap::from([("marker".into(), serde_json::json!("persisted"))]),
            messages: Vec::new(),
        },
    )
    .unwrap();
    let InputContent::InvocationInput {
        arguments,
        invocation,
        ..
    } = &flow
        .committed_input(&flow.committed_inputs()[0].receipt.clone())
        .unwrap()
        .unwrap()
        .input
        .content
    else {
        unreachable!()
    };
    let args = arguments
        .iter()
        .map(|(key, value)| (key.clone(), atman_runtime::Value::from_json(value.clone())))
        .collect();
    let env = atman_runtime::invocation_env::InvocationEnv::from_values(
        invocation
            .iter()
            .map(|(key, value)| (key.clone(), atman_runtime::Value::from_json(value.clone()))),
    );
    let mut authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    authority.permission_management = false;
    let context = std::sync::Arc::new(atman_runtime::Session::open_instance_context(
        description.address.clone(),
    ));
    let executor = atman_runtime::Executor::new();
    let result = executor
        .run_linked_with_invocation(
            &program,
            &description.entry().unwrap().name,
            args,
            atman_runtime::RootInvocation {
                session: Some(context.clone()),
                env,
                instance: Some(atman_runtime::InstanceInvocation::new(
                    lease.clone(),
                    authority,
                    Default::default(),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, atman_runtime::Value::Int(42)));
    assert_eq!(
        context
            .flow_registry
            .lookup_run(&lease.run)
            .unwrap()
            .instance
            .as_ref(),
        Some(&description.address)
    );
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    drop(flow);
    let flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.committed_inputs().len(), 1);
    assert!(flow.pending_inputs().next().is_none());
}

#[tokio::test]
async fn instance_worker_persists_real_tool_delivery_and_restores_only_shared_context() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    use atman_runtime::flow_runtime::FlowWorker;
    use atman_runtime::message::MessageOrigin;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let mut stream = context.stream_subscribe();
    let executor = atman_runtime::Executor::new();
    let file = atman_rt::parse_file(
        r#"
        flow inline() -> string {
            context.record(key: "agent.rule.example", content: "Keep records")
            return final.answer(message: "inline delivery")
        }
        flow branch() -> string { return final.answer(message: "isolated delivery") }
        flow main() -> string {
            inline().await
            fanout [branch()]
            return final.answer(message: "shared delivery")
        }
    "#,
    )
    .unwrap();
    let authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    let binding =
        atman_runtime::InstanceInvocation::new(lease.clone(), authority, Default::default())
            .with_storage(worker.clone())
            .unwrap();
    let result = executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(binding),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, atman_runtime::Value::Str(text) if text == "shared delivery"));
    let messages = context.messages().to_vec();
    assert!(
        messages
            .iter()
            .any(|message| message.contains_context_record())
    );
    let answers: Vec<_> = messages
        .iter()
        .filter(|message| message.origin == MessageOrigin::FinalAnswer)
        .map(Message::text_concat)
        .collect();
    assert_eq!(answers, ["inline delivery", "shared delivery"]);
    let mut streamed = Vec::new();
    while let Ok(frame) = stream.try_recv() {
        if let atman_runtime::stream::StreamFrame::AssistantMsg { message, .. } = frame {
            streamed.push(message.text_concat());
        }
    }
    assert_eq!(
        streamed,
        ["inline delivery", "isolated delivery", "shared delivery"]
    );
    let revision = worker.state().await.unwrap().revision();
    worker
        .transition(
            revision,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    let mut flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.published_messages().len(), 4);
    assert_eq!(flow.context_messages().unwrap(), messages);
    assert_eq!(
        flow.published_messages()
            .iter()
            .filter(|record| record.context_run.is_some())
            .count(),
        1
    );
    drop(flow);
    let flow = session.open_flow(&description.address.instance_id).unwrap();
    let worker = FlowWorker::start(flow).await.unwrap();
    assert_eq!(worker.context().messages().to_vec(), messages);
    assert!(worker.context().stream_subscribe().try_recv().is_err());
    assert!(worker.state().await.unwrap().can_start_turn());
    let state = worker.state().await.unwrap();
    let next = state
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    worker
        .transition(
            state.revision(),
            FlowTransition::TurnStarted {
                turn: next.turn.clone(),
                run: next.run.clone(),
            },
        )
        .await
        .unwrap();
    executor.providers.register(std::sync::Arc::new(
        atman_runtime::providers::mock::MockProvider::new("mock")
            .with_model("mock", atman_runtime::Value::Str("model record".into())),
    ));
    let file = atman_rt::parse_file(
        r#"flow main() -> string {
        llm.call(model: "mock", prompt: "continue", context: "session")
        session.push(message.user("explicit input"))
        return final.answer(message: "after restart")
    }"#,
    )
    .unwrap();
    let authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(
                    atman_runtime::InstanceInvocation::new(next, authority, Default::default())
                        .with_storage(worker.clone())
                        .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let restored = worker.context();
    assert!(restored.messages().len() > messages.len() + 1);
    assert_eq!(
        &restored.messages().to_vec()[..messages.len()],
        messages.as_slice()
    );
    assert_eq!(
        restored.messages().last().unwrap().text_concat(),
        "after restart"
    );
    assert!(
        restored
            .messages()
            .iter()
            .any(|message| message.text_concat() == "model record")
    );
    assert!(
        restored
            .messages()
            .iter()
            .any(|message| message.text_concat() == "explicit input")
    );
    let mut store = worker.shutdown().await.unwrap();
    assert_eq!(
        store.context_messages().unwrap(),
        restored.messages().to_vec()
    );
}
