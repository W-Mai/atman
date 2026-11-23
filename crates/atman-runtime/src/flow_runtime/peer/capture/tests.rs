use super::*;
use crate::{
    flow_runtime::{
        host::{HandoffBlocker, SessionHandoffOutcome},
        peer::tests::Peers,
    },
    flow_store::{FlowTransition, completion::InvocationResult, publication::MessagePublication},
    message::{ImageData, MessageOrigin, MessagePart, MessageRole},
    tools::tool_output::{ToolOutputBudget, truncate_tool_result_content_with_budget},
};

struct Seed {
    source: SourceSelection,
    reference: String,
    output: String,
}

async fn seed(peers: &Peers) -> Seed {
    peers
        .target
        .request(|store| {
            store.transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )?;
            let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
                store.state().state()
            else {
                panic!("source turn must be active")
            };
            let lease = active.lease.clone();
            let image = crate::attachment_store::AttachmentStore::at(store.dir().unwrap())
                .import_bytes(b"\x89PNG\r\n\x1a\n\0\0\0\r", Some("captured.png"))
                .map_err(io::Error::other)?;
            let output = "retained source output\n".repeat(12_000);
            let reference = truncate_tool_result_content_with_budget(
                &output,
                "result",
                Some(&OutputStore::at(store.dir().unwrap())),
                ToolOutputBudget {
                    max_lines: 1,
                    max_bytes: 8,
                    max_line_bytes: 8,
                },
            );
            for (role, parts) in [
                (
                    MessageRole::User,
                    vec![MessagePart::Image { source: image }],
                ),
                (
                    MessageRole::Assistant,
                    vec![MessagePart::ToolUse {
                        id: "read".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    }],
                ),
                (
                    MessageRole::Tool,
                    vec![MessagePart::ToolResult {
                        tool_use_id: "read".into(),
                        content: reference.clone(),
                        is_error: false,
                    }],
                ),
            ] {
                store.publish_message(MessagePublication {
                    tool_artifacts: Vec::new(),
                    id: Uuid::now_v7(),
                    lease: lease.clone(),
                    run: lease.run.clone(),
                    context_run: None,
                    message: Message {
                        role,
                        parts,
                        turn_id: lease.turn.clone(),
                        origin: MessageOrigin::Internal,
                    },
                })?;
            }
            store.finish_turn(
                lease,
                InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
            )?;
            Ok(Seed {
                source: SourceSelection {
                    flow: store.state().address().clone(),
                    branch: store.history_head().branch,
                    cursor: store.cursor().require_durable()?.clone(),
                },
                reference,
                output,
            })
        })
        .await
        .unwrap()
}

async fn begin(peers: &Peers) -> Transfer {
    match peers
        .client
        .peer_operation(PeerOperation::Capture {
            request: CaptureRequest::Reference(Box::new(ReferenceRequest::current(
                peers.target.address().clone(),
            ))),
        })
        .await
        .unwrap()
        .unwrap()
    {
        PeerReply::Capture(transfer) => transfer,
        _ => panic!("capture must return an independent transfer"),
    }
}

fn release(peers: &Peers, transfer: &Transfer) -> Release {
    Release {
        runtime: tokio::runtime::Handle::current(),
        host: peers.source_host.clone(),
        port: peers.source_host.peer_port().unwrap(),
        owner: transfer.owner.clone(),
        principal: peers.client.peer_principal(),
        target: peers.target.address().clone(),
        token: transfer.token,
        kind: ReadKind::Reference,
    }
}

async fn await_released(peers: &Peers) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while peers.target_host.peer_transfers_pending() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_pages_preserve_original_source_assets_without_a_publication_directory_or_fallback()
 {
    let peers = Peers::new().await;
    let seeded = seed(&peers).await;
    let transfer = begin(&peers).await;
    assert!(transfer.length > CHUNK * 2);
    assert_eq!(
        transfer.owner.canonical_data_root,
        peers.data.path().canonicalize().unwrap()
    );
    drop(release(&peers, &transfer));
    await_released(&peers).await;
    let captured = peers
        .client
        .capture_reference(ReferenceRequest::current(peers.target.address().clone()))
        .await
        .unwrap();
    assert_eq!(captured.source, seeded.source);
    assert_eq!(
        captured.origin.catalog.journal_id,
        peers.target.address().session_id.0
    );
    assert_eq!(
        &*captured
            .product
            .outputs
            .retained_content(&seeded.reference)
            .unwrap(),
        seeded.output
    );
    assert_eq!(
        captured.product.outputs.captured_contents().unwrap().len(),
        1
    );
    assert!(
        captured
            .product
            .outputs
            .register("forbidden", "new content")
            .is_none()
    );
    let MessagePart::Image { source } = &captured.product.messages[0].parts[0] else {
        panic!("captured image must be retained")
    };
    assert!(matches!(source.data, ImageData::Base64 { .. }));
    assert_eq!(
        crate::attachment_store::image_bytes(source).unwrap(),
        b"\x89PNG\r\n\x1a\n\0\0\0\r"
    );
    let unknown = peers
        .target
        .request(|store| {
            Ok(truncate_tool_result_content_with_budget(
                &"another source output\n".repeat(100),
                "unknown",
                Some(&OutputStore::at(store.dir().unwrap())),
                ToolOutputBudget {
                    max_lines: 1,
                    max_bytes: 8,
                    max_line_bytes: 8,
                },
            ))
        })
        .await
        .unwrap();
    assert!(
        captured
            .product
            .outputs
            .retained_content(&unknown)
            .is_none()
    );
    let unknown_id = crate::tools::tool_output::reference_identity(&unknown)
        .unwrap()
        .0;
    assert!(
        captured
            .product
            .outputs
            .read_lines(&unknown_id, 0, 1, ToolOutputBudget::default())
            .is_err()
    );
    await_released(&peers).await;
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_snapshot_page_rechecks_current_grants_and_original_actor_while_release_remains_available()
 {
    let peers = Peers::new().await;
    seed(&peers).await;
    let transfer = begin(&peers).await;
    assert!(peers.target_host.peer_transfers_pending());
    assert!(!peers.target_host.try_shutdown_idle().await.unwrap());
    let first = peers
        .client
        .peer_operation_at(
            PeerOperation::CaptureChunk {
                target: peers.target.address().clone(),
                token: transfer.token,
                offset: 0,
                read: ReadKind::Reference,
            },
            Some(transfer.owner.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, PeerReply::CaptureChunk { offset: 0, bytes } if bytes.len() == CHUNK));
    assert!(
        peers
            .client
            .peer_operation_at(
                PeerOperation::CaptureChunk {
                    target: peers.target.address().clone(),
                    token: Uuid::now_v7(),
                    offset: 0,
                    read: ReadKind::Reference
                },
                Some(transfer.owner.clone())
            )
            .await
            .is_err()
    );
    let other = FlowClient::bind(
        peers.source_host.clone(),
        &peers.source,
        peers.client.lease.clone(),
    )
    .unwrap();
    let wrong_actor = other
        .peer_operation_at(
            PeerOperation::CaptureChunk {
                target: peers.target.address().clone(),
                token: transfer.token,
                offset: CHUNK,
                read: ReadKind::Reference,
            },
            Some(transfer.owner.clone()),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(wrong_actor.kind(), io::ErrorKind::PermissionDenied);
    other
        .peer_operation_at(
            PeerOperation::ReleaseCapture {
                target: peers.target.address().clone(),
                token: transfer.token,
                read: ReadKind::Reference,
            },
            Some(transfer.owner.clone()),
        )
        .await
        .unwrap();
    assert!(peers.target_host.peer_transfers_pending());
    peers.revoke().await;
    let revoked = peers
        .client
        .peer_operation_at(
            PeerOperation::CaptureChunk {
                target: peers.target.address().clone(),
                token: transfer.token,
                offset: CHUNK,
                read: ReadKind::Reference,
            },
            Some(transfer.owner.clone()),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(revoked.kind(), io::ErrorKind::PermissionDenied);
    drop(release(&peers, &transfer));
    await_released(&peers).await;
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_reader_off_runtime_retires_its_transfer_without_waiting_for_an_inflight_chunk()
 {
    let peers = Peers::new().await;
    seed(&peers).await;
    let (blocked, resume) = peers.block_next_chunk();
    let client = peers.client.clone();
    let target = peers.target.address().clone();
    let mut reader = Box::pin(async move {
        client
            .capture_reference(ReferenceRequest::current(target))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = &mut reader => panic!("reader completed before its blocked chunk"),
            ready = blocked => ready.unwrap(),
        }
    })
    .await
    .unwrap();
    assert!(peers.target_host.peer_transfers_pending());
    std::thread::spawn(move || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(reader);
    })
    .join()
    .unwrap();
    await_released(&peers).await;
    resume.notify_one();
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_retention_fences_only_preparation_and_completed_transfer_survives_source_handoff() {
    let _saturation = crate::read_work::storage_saturation_gate().await;
    let peers = Peers::new().await;
    seed(&peers).await;
    struct Readers(Vec<std::sync::mpsc::Sender<()>>);
    impl Drop for Readers {
        fn drop(&mut self) {
            for sender in self.0.drain(..) {
                let _ = sender.send(());
            }
        }
    }
    let mut release_readers = Readers(Vec::new());
    let mut readers = Vec::new();
    for _ in 0..2 {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        release_readers.0.push(release);
        readers.push(tokio::spawn(crate::read_work::capture(move || {
            let _ = entered.send(());
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
        })));
        tokio::time::timeout(Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
    }
    let client = peers.client.clone();
    let target = peers.target.address().clone();
    let capture = tokio::spawn(async move {
        client
            .peer_operation(PeerOperation::Capture {
                request: CaptureRequest::Reference(Box::new(ReferenceRequest::current(target))),
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if peers
                .target_host
                .shutdown_readiness()
                .await
                .unwrap()
                .contains(&HandoffBlocker::SourceCapture)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let operation = Uuid::now_v7();
    assert!(
        matches!(peers.target_host.handoff_session(peers.target.address().session_id.0, operation).await.unwrap(), SessionHandoffOutcome::Pending { blockers, .. } if blockers.contains(&HandoffBlocker::SourceCapture))
    );
    drop(release_readers);
    for reader in readers {
        reader.await.unwrap().unwrap();
    }
    let PeerReply::Capture(transfer) = capture.await.unwrap().unwrap().unwrap() else {
        panic!("capture must return a transfer")
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                peers
                    .target_host
                    .handoff_session(peers.target.address().session_id.0, operation)
                    .await
                    .unwrap(),
                SessionHandoffOutcome::Released { .. }
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(peers.target_host.peer_transfers_pending());
    assert!(!peers.target_host.try_shutdown_idle().await.unwrap());
    let chunk = peers
        .client
        .peer_operation_at(
            PeerOperation::CaptureChunk {
                target: peers.target.address().clone(),
                token: transfer.token,
                offset: 0,
                read: ReadKind::Reference,
            },
            Some(transfer.owner.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(chunk, PeerReply::CaptureChunk { offset: 0, bytes } if bytes.len() == CHUNK));
    drop(release(&peers, &transfer));
    await_released(&peers).await;
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_owned_reads_return_original_typed_snapshots_without_a_wire_transfer() {
    let peers = Peers::new().await;
    let lease = peers.client.lease.clone();
    let text = "native owned conversation\n".repeat(12_000);
    let original = text.clone();
    peers
        .source
        .request(move |store| {
            store.publish_message(MessagePublication {
                tool_artifacts: Vec::new(),
                id: Uuid::now_v7(),
                lease: lease.clone(),
                run: lease.run.clone(),
                context_run: None,
                message: Message {
                    origin: MessageOrigin::Internal,
                    ..Message::assistant_text(lease.turn, text)
                },
            })?;
            Ok(())
        })
        .await
        .unwrap();
    let reply = peers
        .client
        .peer_operation(PeerOperation::Capture {
            request: CaptureRequest::Context(peers.source.address().clone()),
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(&reply, PeerReply::NativeSnapshot(_)));
    assert!(serde_json::to_vec(&reply).is_err());
    assert!(!peers.source_host.peer_transfers_pending());
    let context = peers
        .client
        .read_context(peers.source.address().clone())
        .await
        .unwrap();
    assert!(
        context
            .iter()
            .any(|message| message.text_concat() == original)
    );
    assert!(!peers.source_host.peer_transfers_pending());
    peers.shutdown().await;
}
