use super::*;
use crate::{
    attachment_store::{AttachmentStore, image_bytes},
    flow_store::{
        FlowInstanceId, FlowTransition, RuntimeStore,
        completion::InvocationResult,
        context::{ContextCheckpoint, context_digest},
        inbox::SenderIdentity,
        launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput},
        publication::MessagePublication,
        source_capture::tests::{prepared_fixture, publish},
    },
    message::MessageRole,
    tools::tool_output::ToolOutputBudget,
};
use atman_rt::{RunId, TurnId, TurnOutcome};

fn image(dir: &Path, suffix: u8, turn: &crate::event::TurnId) -> Message {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.push(suffix);
    let source = AttachmentStore::at(dir)
        .import_bytes(&bytes, Some("image.png"))
        .unwrap();
    Message {
        parts: vec![MessagePart::Image { source }],
        ..Message::user_text(turn.clone(), "")
    }
}

fn request(flow: &FlowStore) -> ForkBoundaryRequest {
    ForkBoundaryRequest {
        observed: SourceSelection {
            flow: flow.state().address().clone(),
            branch: flow.history_head().branch,
            cursor: flow.cursor().require_durable().unwrap().clone(),
        },
        expected_head: flow.history_head(),
        cutoff: ForkCutoff::LatestSafe,
    }
}

#[test]
fn retained_fork_captures_baseline_old_checkpoints_isolated_images_and_historical_outputs() {
    let (source, mut flow) = prepared_fixture();
    let staging = tempfile::tempdir().unwrap();
    let dir = flow.dir().unwrap().to_owned();
    let turn = TurnId::now();
    let run = RunId::now();
    let handle = format!("bg_{}_9", flow.state().address().session_id);
    let log = dir.join(format!("bg_{handle}.log"));
    std::fs::write(&log, "baseline log\n").unwrap();
    let baseline = vec![
        image(&dir, 1, &turn),
        Message {
            parts: vec![MessagePart::ToolUse {
                id: "baseline-call".into(),
                name: "bash.status".into(),
                input: serde_json::json!({"handle":handle}),
                intent: None,
            }],
            ..Message::assistant_text(turn.clone(), "")
        },
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "baseline-call".into(),
                is_error: false,
                content: serde_json::json!({"handle": handle, "log_path":log}).to_string(),
            }],
            ..Message::user_text(turn.clone(), "")
        },
    ];
    flow.activate_entry(
        LaunchInput {
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal: None,
            request: EntryRequest {
                request_id: flow.descriptor().creation_request,
                sender: SenderIdentity::User(Uuid::now_v7()),
                recipient: flow.state().address().clone(),
                arguments: Default::default(),
                invocation: Default::default(),
                start: EntryStart::Waiting,
            },
            inherited_context: baseline,
            inherited_outputs: vec![],
            continued_from: None,
            result_route: None,
            execution: Some(EntryBinding {
                user_routes: false,
                session_start: false,
                first_turn: turn.clone(),
                first_run: run.clone(),
                authority: crate::flow_authority::EffectiveAuthority::root(
                    &Default::default(),
                    true,
                    None,
                ),
            }),
        },
        None,
        |_| Ok(()),
    )
    .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted { turn, run },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("active")
    };
    let lease = active.lease.clone();
    let full = "complete historical output";
    let id = OutputStore::at(&dir).register("history", full).unwrap();
    publish(
        &mut flow,
        &lease,
        Message {
            parts: vec![MessagePart::ToolUse {
                id: "call".into(),
                name: "fs.read".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            ..Message::assistant_text(lease.turn.clone(), "")
        },
    );
    publish(
        &mut flow,
        &lease,
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call".into(),
                is_error: false,
                content:
                    serde_json::json!({"output_id":id,"content":"preview","total_bytes":full.len(),
                "next":{"mode":"bytes","offset":0,"has_more":true}})
                    .to_string(),
            }],
            ..Message::user_text(lease.turn.clone(), "")
        },
    );
    flow.publish_message(MessagePublication {
        tool_artifacts: Vec::new(),
        id: Uuid::now_v7(),
        lease: lease.clone(),
        run: lease.run.clone(),
        context_run: Some(lease.run.clone()),
        message: image(&dir, 2, &lease.turn),
    })
    .unwrap();
    let model_context = vec![
        Message::assistant_text(lease.turn.clone(), "current summary"),
        image(&dir, 3, &lease.turn),
        Message {
            parts: vec![MessagePart::ToolUse {
                id: "model-read".into(),
                name: "fs.read".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            ..Message::assistant_text(lease.turn.clone(), "")
        },
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "model-read".into(),
                is_error: false,
                content: serde_json::json!({"output_id":id,"content":"preview","total_bytes":full.len(),"next":{"mode":"bytes","offset":0,"has_more":true}}).to_string(),
            }],
            ..Message::user_text(lease.turn.clone(), "")
        },
    ];
    for messages in [vec![image(&dir, 3, &lease.turn)], model_context] {
        let current = flow.context_messages().unwrap();
        flow.checkpoint_context(ContextCheckpoint {
            id: Uuid::now_v7(),
            expected_head: Some(flow.history_head()),
            purpose: Default::default(),
            expected: context_digest(&current).unwrap(),
            range: 0..current.len(),
            count: current.len(),
            before_tokens: 100,
            after_tokens: 10,
            summary: "checkpoint".into(),
            messages,
        })
        .unwrap();
    }
    flow.finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    let original = flow.cursor().clone();
    let read_for_review = || {
        flow.capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
            .unwrap()
    };
    let review = read_for_review()
        .review(1024 * 1024, 4096, SourceCaptureControl::default())
        .unwrap();
    assert!(staging.path().read_dir().unwrap().next().is_none());
    assert_ne!(review.snapshot_bytes, 0);
    assert!(
        read_for_review()
            .review(
                review.snapshot_bytes,
                review.asset_bytes - 1,
                SourceCaptureControl::default()
            )
            .is_err()
    );
    assert!(
        read_for_review()
            .review(
                review.snapshot_bytes - 1,
                4096,
                SourceCaptureControl::default()
            )
            .is_err()
    );
    let read = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap();
    let snapshot = read.prepare_retained(staging.path(), 4096).unwrap();
    assert_eq!(
        snapshot.data().boundary.source.cursor,
        *original.require_durable().unwrap()
    );
    assert_eq!(
        snapshot.data().model_context[0].text_concat(),
        "current summary"
    );
    assert_eq!(snapshot.assets().logs().len(), 1);
    assert_eq!(snapshot.assets().outputs().len(), 1);
    let mut images = 0;
    for message in snapshot.data().messages() {
        for part in &message.parts {
            if let MessagePart::Image { source } = part {
                assert_eq!(image_bytes(source).unwrap().len(), 9);
                images += 1;
            }
        }
    }
    assert_eq!(images, 5);
    let assets = snapshot.assets();
    assert_eq!(assets.bytes(), 27 + full.len() + "baseline log\n".len());
    assert_eq!(review.boundary, snapshot.data().boundary);
    assert_eq!(review.history_records, snapshot.data().records.len());
    assert_eq!(review.asset_bytes, assets.bytes());
    assert_eq!(
        OutputStore::at(assets.directory())
            .read_bytes(&id, 0, full.len(), ToolOutputBudget::default())
            .unwrap()
            .content,
        full
    );
    assert_eq!(
        std::fs::read_to_string(assets.directory().join(&assets.logs()[0].filename)).unwrap(),
        "baseline log\n"
    );
    let decoded: ForkSnapshot =
        serde_json::from_slice(&serde_json::to_vec(snapshot.data()).unwrap()).unwrap();
    assert_eq!(decoded.records.len(), snapshot.data().records.len());
    assert!(
        decoded
            .records
            .iter()
            .all(|entry| entry.source.flow == snapshot.data().boundary.source.flow)
    );

    let runtime = RuntimeStore::open(source.path()).unwrap();
    let destination_id = Uuid::now_v7();
    let mut destination = runtime
        .create_session(crate::flow_store::SessionDescriptor {
            id: destination_id,
            project: None,
            configuration: Default::default(),
            title: "Fork destination".into(),
        })
        .unwrap();
    let plan = crate::flow_store::source_capture::ForkImportPlan::from_retained_snapshot(
        Uuid::now_v7(),
        FlowRef {
            session_id: destination_id.into(),
            instance_id: FlowInstanceId::now(),
        },
        None,
        std::env::current_dir().unwrap(),
        &snapshot,
    )
    .unwrap()
    .bind_source_descriptor(flow.descriptor())
    .unwrap();
    let input = LaunchInput {
        name: None,
        display_label: None,
        scope: None,
        workspace: None,
        initial_goal: None,
        request: EntryRequest {
            request_id: plan.operation,
            sender: SenderIdentity::User(Uuid::now_v7()),
            recipient: plan.destination.clone(),
            arguments: Default::default(),
            invocation: Default::default(),
            start: EntryStart::Waiting,
        },
        inherited_context: Vec::new(),
        inherited_outputs: Vec::new(),
        continued_from: None,
        result_route: None,
        execution: Some(EntryBinding {
            user_routes: false,
            session_start: false,
            first_turn: TurnId::now(),
            first_run: RunId::now(),
            authority: crate::flow_authority::EffectiveAuthority::root(
                &Default::default(),
                true,
                None,
            ),
        }),
    };
    let activation = destination
        .create_waiting_fork_import(plan.clone(), &flow, input.clone(), |_, _| Ok(()))
        .unwrap();
    let destination_receipt = match activation.receipt.state.clone() {
        crate::flow_store::source_capture::ForkImportState::Published {
            destination_receipt,
        } => destination_receipt,
        state => panic!("unexpected Fork import state: {state:?}"),
    };
    let inspect = || RuntimeStore::inspect_session(source.path(), destination_id).unwrap();
    assert!(
        inspect()
            .inspect_fork_import_publication(plan.operation)
            .unwrap()
            .is_none()
    );
    let prepare_again = || {
        flow.capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
            .unwrap()
            .prepare_retained(staging.path(), 4096)
            .unwrap()
    };
    let recaptured = prepare_again();
    assert_ne!(
        snapshot.assets().directory(),
        recaptured.assets().directory()
    );
    let seeded_id = Uuid::now_v7();
    let mut seeded = runtime
        .create_session(crate::flow_store::SessionDescriptor {
            id: seeded_id,
            project: None,
            configuration: Default::default(),
            title: "Seeded Fork destination".into(),
        })
        .unwrap();
    let seeded_plan = crate::flow_store::source_capture::ForkImportPlan::from_retained_snapshot(
        Uuid::now_v7(),
        FlowRef {
            session_id: seeded_id.into(),
            instance_id: FlowInstanceId::now(),
        },
        None,
        plan.working_directory.clone(),
        &snapshot,
    )
    .unwrap();
    let mut seed_input = input;
    seed_input.request.request_id = seeded_plan.operation;
    seed_input.request.recipient = seeded_plan.destination.clone();
    let binding = seed_input.execution.as_mut().unwrap();
    binding.first_turn = TurnId::now();
    binding.first_run = RunId::now();
    binding.authority.workspace_root = Some(seeded_plan.working_directory.clone());
    let entry = flow.fork_source_entry();
    assert!(
        seeded
            .create_seeded_fork_import(
                seeded_plan.clone(),
                &entry,
                &recaptured,
                seed_input.clone(),
                |_, _| Err(io::ErrorKind::PermissionDenied.into())
            )
            .is_err()
    );
    assert!(seeded.projection().flows.is_empty());
    assert!(
        seeded
            .inspect_fork_import(seeded_plan.operation)
            .unwrap()
            .is_none()
    );

    let damaged = prepare_again();
    std::fs::remove_file(
        damaged
            .assets()
            .directory()
            .join("tool_outputs")
            .join(format!("{id}.txt")),
    )
    .unwrap();
    assert!(
        seeded
            .create_seeded_fork_import(
                seeded_plan.clone(),
                &entry,
                &damaged,
                seed_input.clone(),
                |_, _| Ok(())
            )
            .is_err()
    );
    assert!(
        seeded
            .inspect_fork_import(seeded_plan.operation)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        seeded
            .open_flow(&seeded_plan.destination.instance_id)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        seeded
            .inspect_fork_import_publication(seeded_plan.operation)
            .unwrap()
            .is_none()
    );
    drop(seeded);
    let mut seeded = runtime.open_session(seeded_id).unwrap();
    let seeded_publication = seeded
        .create_seeded_fork_import(
            seeded_plan.clone(),
            &entry,
            &recaptured,
            seed_input.clone(),
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(
        seeded_publication.model_context[0].text_concat(),
        "current summary"
    );
    let retried = seeded
        .create_seeded_fork_import(
            seeded_plan.clone(),
            &entry,
            &snapshot,
            seed_input.clone(),
            |_, _| panic!("completed import must not activate again"),
        )
        .unwrap();
    assert_eq!(
        retried.destination_receipt,
        seeded_publication.destination_receipt
    );
    seed_input.display_label = Some("changed invocation".into());
    assert!(
        seeded
            .create_seeded_fork_import(
                seeded_plan.clone(),
                &entry,
                &snapshot,
                seed_input,
                |_, _| Ok(())
            )
            .is_err()
    );
    assert_eq!(seeded.projection().flows.len(), 1);
    drop(seeded);

    std::fs::write(&log, "modified log\n").unwrap();
    let changed = prepare_again();
    assert_eq!(changed.digest(), recaptured.digest());
    assert_eq!(changed.assets().bytes(), recaptured.assets().bytes());
    let error = destination
        .publish_fork_snapshot(plan.clone(), &changed, destination_receipt.clone())
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Fork snapshot changed after import review"
    );
    assert!(
        inspect()
            .inspect_fork_import_publication(plan.operation)
            .unwrap()
            .is_none()
    );
    let published = destination
        .publish_fork_snapshot(plan.clone(), &recaptured, destination_receipt.clone())
        .unwrap();
    assert_eq!(published.history.receipt, destination_receipt);
    assert_eq!(published.logs.len(), 1);
    assert_eq!(published.logs[0].handle, handle);
    assert_eq!(
        published.history.records.len(),
        snapshot.data().records.len()
    );
    let before_replay = destination
        .inspect_flow(&plan.destination.instance_id)
        .unwrap()
        .journal
        .cursor;
    let error = destination.replay_fork_publication(&published).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(
        destination
            .inspect_flow(&plan.destination.instance_id)
            .unwrap()
            .journal
            .cursor,
        before_replay
    );
    let error = destination
        .replay_fork_snapshot(plan.clone(), &snapshot, destination_receipt.clone())
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(
        destination
            .inspect_flow(&plan.destination.instance_id)
            .unwrap()
            .journal
            .cursor,
        before_replay
    );
    let target_dir = destination
        .open_flow(&plan.destination.instance_id)
        .unwrap()
        .dir()
        .unwrap()
        .to_owned();
    assert!(target_dir.join("attachments").is_dir());
    assert!(
        target_dir
            .join("tool_outputs")
            .join(format!("{id}.txt"))
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(target_dir.join(format!("bg_{handle}.log"))).unwrap(),
        "baseline log\n"
    );
    let repeated = destination
        .publish_fork_snapshot(plan.clone(), &snapshot, destination_receipt.clone())
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&repeated).unwrap(),
        serde_json::to_vec(&published).unwrap()
    );
    let mut unknown = destination_receipt.clone();
    unknown.digest = "unknown-receipt".into();
    assert!(
        destination
            .publish_fork_snapshot(plan.clone(), &snapshot, unknown)
            .is_err()
    );
    let digest = inspect()
        .inspect_fork_import_publication(plan.operation)
        .unwrap()
        .unwrap()
        .digest;
    let mut target = destination
        .open_flow(&plan.destination.instance_id)
        .unwrap();
    let advanced = target
        .transition(
            target.state().revision(),
            FlowTransition::TurnStarted {
                turn: TurnId::now(),
                run: RunId::now(),
            },
        )
        .unwrap();
    assert!(advanced.sequence() > destination_receipt.sequence);
    drop(target);
    let repeated = destination
        .publish_fork_snapshot(plan.clone(), &recaptured, destination_receipt.clone())
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&repeated).unwrap(),
        serde_json::to_vec(&published).unwrap()
    );
    drop(destination);
    drop(flow);
    std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).unwrap();
    let seeded = runtime.open_session(seeded_id).unwrap();
    let mut seeded_flow = seeded
        .open_flow(&seeded_plan.destination.instance_id)
        .unwrap();
    let restored_context = seeded_flow.context_messages().unwrap();
    assert_eq!(restored_context, seeded_publication.model_context);
    let MessagePart::Image {
        source: restored_image,
    } = &restored_context[1].parts[0]
    else {
        panic!("restored model image")
    };
    let ImageData::Artifact { path, .. } = &restored_image.data else {
        panic!("destination-owned image")
    };
    assert!(path.starts_with(seeded_flow.dir().unwrap()));
    assert_eq!(image_bytes(restored_image).unwrap().len(), 9);
    assert_eq!(
        OutputStore::at(seeded_flow.dir().unwrap())
            .read_bytes(&id, 0, full.len(), ToolOutputBudget::default())
            .unwrap()
            .content,
        full
    );
    assert!(seeded_flow.committed_inputs().is_empty());
    assert!(matches!(
        seeded_flow.state().state(),
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
    ));
    assert_eq!(
        seeded_flow.cursor().require_durable().unwrap(),
        &seeded_publication.destination_receipt
    );
    let inspected = inspect()
        .inspect_fork_import_publication(plan.operation)
        .unwrap()
        .unwrap();
    assert_eq!(
        inspected.publication.destination_receipt,
        destination_receipt
    );
    assert_eq!(inspected.digest, digest);
    let log = &inspected.publication.logs[0];
    let bytes = std::fs::read(target_dir.join(format!("bg_{}.log", log.handle))).unwrap();
    assert_eq!(bytes.len(), log.bytes);
    assert_eq!(blake3::hash(&bytes).to_hex().as_str(), log.digest);

    let location = inspect()
        .dir
        .join("fork-imports")
        .join(plan.operation.to_string())
        .join("publication.json");
    let mut altered = inspected.publication;
    let assets_digest = std::mem::replace(&mut altered.assets_digest, "a".repeat(64));
    std::fs::write(&location, serde_json::to_vec(&altered).unwrap()).unwrap();
    assert!(
        inspect()
            .inspect_fork_import_publication(plan.operation)
            .is_err()
    );
    altered.assets_digest = assets_digest;
    altered.logs[0].handle = format!("bg_{destination_id}_9");
    std::fs::write(location, serde_json::to_vec(&altered).unwrap()).unwrap();
    assert!(
        inspect()
            .inspect_fork_import_publication(plan.operation)
            .is_err()
    );
    drop(source);
}

#[test]
fn inherited_log_paths_do_not_expand_the_retained_sources_file_authority() {
    let source = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let flow = FlowRef {
        session_id: Uuid::now_v7().into(),
        instance_id: atman_rt::FlowInstanceId::now(),
    };
    let handle = format!("bg_{}_1", flow.session_id);
    let path = other.path().join(format!("bg_{handle}.log"));
    std::fs::write(&path, "outside").unwrap();
    let turn = TurnId::now();
    let messages = vec![
        Message {
            parts: vec![MessagePart::ToolUse {
                id: "call".into(),
                name: "bash.status".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            ..Message::assistant_text(turn.clone(), "")
        },
        Message {
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call".into(),
                is_error: false,
                content: serde_json::json!({"handle":handle,"log_path":path}).to_string(),
            }],
            ..Message::user_text(turn, "")
        },
    ];
    let assets = RetainedSourceAssets::capture(
        &messages,
        &OutputStore::at(source.path()),
        staging.path(),
        100,
    )
    .unwrap();
    assert!(
        assets
            .retain_context_logs(&flow, source.path(), &messages, 100)
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "outside");
}
