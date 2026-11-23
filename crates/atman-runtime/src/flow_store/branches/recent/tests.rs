use super::*;
use crate::flow_store::{
    FlowStore, RuntimeStore,
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload, InputContent, SenderIdentity},
    publication::MessagePublication,
    source_capture::tests::{checkpoint, fixture, publish},
};

fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn budget() -> InspectionBudget {
    InspectionBudget {
        work_bytes: nonzero(128 * 1024),
        max_batches: nonzero(2),
        max_record_bytes: nonzero(1024 * 1024),
    }
}

fn request(flow: &FlowStore, limit: usize) -> RecentConversationRequest {
    RecentConversationRequest {
        flow: flow.state().address().clone(),
        expected: None,
        expected_head: None,
        before: None,
        limit: nonzero(limit),
    }
}

fn prepare(
    directory: &std::path::Path,
    request: RecentConversationRequest,
    limits: RecentConversationLimits,
) -> PreparedConversationSource {
    let session = RuntimeStore::inspect_session(directory, request.flow.session_id.0).unwrap();
    let mut preparation = session
        .prepare_recent_conversation(request, limits, SourceCaptureControl::default())
        .unwrap();
    loop {
        match preparation.step(budget()).unwrap() {
            ConversationPreparationStep::Loading(_) => {}
            ConversationPreparationStep::Ready(source) => return *source,
            ConversationPreparationStep::Capacity(capacity) => {
                panic!("unexpected capacity: {capacity:?}")
            }
        }
    }
}

fn page(
    source: &mut PreparedConversationSource,
    request: RecentConversationRequest,
) -> RecentConversationPage {
    let mut page = source
        .start_page(request, SourceCaptureControl::default())
        .unwrap()
        .unwrap();
    loop {
        match page.step(source, budget()).unwrap() {
            ConversationPageStep::Loading => {}
            ConversationPageStep::Ready(page) => return page,
            ConversationPageStep::Capacity(capacity) => panic!("unexpected capacity: {capacity:?}"),
        }
    }
}

fn texts(page: &RecentConversationPage) -> Vec<String> {
    page.items
        .iter()
        .map(|item| match &item.body {
            ConversationItemBody::Inline(message) => message.text_concat(),
            ConversationItemBody::Deferred { .. } => "<deferred>".into(),
        })
        .collect()
}

fn continuation(
    source: &PreparedConversationSource,
    before: ConversationItemId,
    limit: usize,
) -> RecentConversationRequest {
    RecentConversationRequest {
        flow: source.boundary.source.flow.clone(),
        expected: Some(source.boundary.source.clone()),
        expected_head: Some(source.boundary.observed_head.clone()),
        before: Some(before),
        limit: nonzero(limit),
    }
}

#[tokio::test]
async fn artifact_reads_recheck_authority_even_with_a_cached_record() {
    use crate::flow_runtime::catalog::{
        ConversationArtifactRequest, ConversationArtifactResult, ConversationArtifactSources,
        SessionCatalog,
    };
    use crate::message::MessagePart;
    use crate::tools::tool_output::OutputArtifactIdentity;
    use std::cell::Cell;

    let (directory, mut flow, lease) = fixture();
    let text = "🦀工具结果";
    let id = flow.output_store().register("test", text).unwrap();
    let publication = flow
        .publish_message(MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run,
            context_run: None,
            message: Message {
                role: MessageRole::Tool,
                turn_id: lease.turn,
                origin: MessageOrigin::User,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "original-call".into(),
                    content: format!(
                        "Preview\n\n[Output truncated: total_bytes={}, output_id={id}.]",
                        text.len()
                    ),
                    is_error: false,
                }],
            },
            tool_artifacts: vec![ToolResultArtifact {
                part: 0,
                call_id: "original-call".into(),
                output: OutputArtifactIdentity {
                    id: id.clone(),
                    bytes: text.len(),
                    digest: blake3::hash(text.as_bytes()).to_hex().to_string(),
                },
            }],
        })
        .unwrap();
    let query = request(&flow, 1);
    let prepared = prepare(directory.path(), query, RecentConversationLimits::default());
    let catalog = SessionCatalog::new(None, Some(directory.path().to_owned()));
    let cache = Arc::new(ConversationArtifactSources::default());
    let range = || ConversationArtifactRequest {
        source: prepared.boundary.source.clone(),
        head: prepared.boundary.observed_head.clone(),
        item: ConversationItemId {
            record: FlowRecordId {
                commit: publication.receipt.sequence(),
                fact: 0,
                item: 0,
            },
            message: 0,
        },
        part: 0,
        id: id.clone(),
        offset: 0,
        limit: 4,
        verification: None,
    };
    assert!(matches!(
        catalog.read_conversation_artifact(range(), cache.clone(), |_| Ok(())).await.unwrap(),
        ConversationArtifactResult::Ready(page) if page.content == "🦀" && page.has_more
    ));
    let checks = Cell::new(0);
    let rejected = catalog
        .read_conversation_artifact(range(), cache, |target| {
            assert_eq!(target, &prepared.boundary.source.flow);
            let count = checks.get() + 1;
            checks.set(count);
            if count >= 3 {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        })
        .await;
    assert!(matches!(rejected, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
    assert_eq!(checks.get(), 3);
}

#[test]
fn cold_pages_keep_original_shared_record_identity_and_intra_batch_continuation() {
    let (directory, mut flow, lease) = fixture();
    for text in ["one", "two", "three"] {
        flow.accept_input(
            DeliveryRequest {
                expected_head: None,
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::AtBoundary,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection { text: text.into() },
            },
            |_| Ok(()),
        )
        .unwrap();
    }
    let reserved = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let committed = flow
        .commit_input(
            &reserved,
            InputContent::Context {
                messages: ["one", "two", "three"]
                    .into_iter()
                    .map(|text| Message::user_text(lease.turn.clone(), text))
                    .collect(),
            },
        )
        .unwrap();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "four"),
    );
    checkpoint(&mut flow, &lease, "private checkpoint");
    let run = atman_rt::RunId::now();
    flow.publish_message(MessagePublication {
        tool_artifacts: Vec::new(),
        id: Uuid::new_v4(),
        lease: lease.clone(),
        run: run.clone(),
        context_run: Some(run),
        message: Message::assistant_text(lease.turn.clone(), "isolated"),
    })
    .unwrap();
    let request = request(&flow, 2);
    let before_bytes = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    let mut source = prepare(
        directory.path(),
        request.clone(),
        RecentConversationLimits::default(),
    );
    assert!(matches!(
        source.boundary.evidence,
        ConversationEvidence::DiskPrefix { trailing_bytes: 0 }
    ));
    let newest = page(&mut source, request);
    assert_eq!(texts(&newest), ["three", "four"]);
    let expected = ConversationCutoff::BeforeUserTurn {
        lease: lease.clone(),
        input: reserved.message_ids()[0],
        receipt: committed.require_durable().unwrap().clone(),
    };
    assert_eq!(newest.items[0].user_turn.as_ref(), Some(&expected));
    assert!(newest.items[1].user_turn.is_none());
    assert!(newest.has_earlier);
    let before = newest.before.unwrap();
    assert_eq!(before.record.commit, committed.sequence());
    assert_eq!(before.record.item, 0);
    assert_eq!(before.message, 2);
    let older_request = continuation(&source, before, 2);
    let older = page(&mut source, older_request);
    assert_eq!(texts(&older), ["one", "two"]);
    assert!(
        older
            .items
            .iter()
            .all(|item| item.user_turn.as_ref() == Some(&expected))
    );
    assert!(!older.has_earlier);
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        before_bytes
    );
}

#[test]
fn before_rejects_foreign_full_source_hidden_fact_and_missing_message() {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "visible"),
    );
    let visible = flow.cursor().sequence();
    checkpoint(&mut flow, &lease, "hidden");
    let hidden = flow.cursor().sequence();
    let mut source = prepare(
        directory.path(),
        request(&flow, 1),
        RecentConversationLimits::default(),
    );
    let id = ConversationItemId {
        record: FlowRecordId {
            commit: visible,
            fact: 0,
            item: 0,
        },
        message: 0,
    };
    let mut foreign = continuation(&source, id, 1);
    foreign.expected.as_mut().unwrap().cursor.digest.push('0');
    assert!(
        source
            .start_page(foreign, SourceCaptureControl::default())
            .is_err()
    );
    let mut hidden_request = continuation(&source, id, 1);
    hidden_request.before.as_mut().unwrap().record.commit = hidden;
    assert!(
        source
            .start_page(hidden_request, SourceCaptureControl::default())
            .is_err()
    );
    for (fact, message, item) in [(0, 9, 0), (9, 0, 0), (0, 0, 1)] {
        let mut invalid = continuation(&source, id, 1);
        let before = invalid.before.as_mut().unwrap();
        before.record.fact = fact;
        before.record.item = item;
        before.message = message;
        match source.start_page(invalid, SourceCaptureControl::default()) {
            Err(_) => assert_eq!(item, 1),
            Ok(Ok(mut page)) => assert!(page.step(&mut source, budget()).is_err()),
            Ok(Err(capacity)) => panic!("unexpected capacity: {capacity:?}"),
        }
    }
    let mut replacement = prepare(
        directory.path(),
        request(&flow, 1),
        RecentConversationLimits::default(),
    );
    assert_eq!(source.boundary(), replacement.boundary());
    let mut pending = source
        .start_page(request(&flow, 1), SourceCaptureControl::default())
        .unwrap()
        .unwrap();
    assert!(pending.step(&mut replacement, budget()).is_err());
}

#[test]
fn source_freezes_append_and_rejects_shrink_without_a_writer_or_tail_repair() {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "captured"),
    );
    let request = request(&flow, 2);
    let session =
        RuntimeStore::inspect_session(directory.path(), request.flow.session_id.0).unwrap();
    let mut preparation = session
        .prepare_recent_conversation(
            request.clone(),
            RecentConversationLimits::default(),
            SourceCaptureControl::default(),
        )
        .unwrap();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "later"),
    );
    let mut source = loop {
        match preparation.step(budget()).unwrap() {
            ConversationPreparationStep::Loading(_) => {}
            ConversationPreparationStep::Ready(source) => break source,
            ConversationPreparationStep::Capacity(capacity) => {
                panic!("unexpected capacity: {capacity:?}")
            }
        }
    };
    assert_eq!(texts(&page(&mut source, request.clone())), ["captured"]);
    let mut active = source
        .start_page(request, SourceCaptureControl::default())
        .unwrap()
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(flow.dir().unwrap().join("events.jsonl"))
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(
        active.step(&mut source, budget()).err().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert!(active.step(&mut source, budget()).is_err());
}

#[test]
fn source_and_page_cancellation_retire_observation_only() {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "live"),
    );
    let request = request(&flow, 1);
    let session =
        RuntimeStore::inspect_session(directory.path(), request.flow.session_id.0).unwrap();
    let cancelled = SourceCaptureControl::default();
    let mut preparation = session
        .prepare_recent_conversation(
            request.clone(),
            RecentConversationLimits::default(),
            cancelled.clone(),
        )
        .unwrap();
    cancelled.cancel();
    assert_eq!(
        preparation.step(budget()).err().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(preparation.pending.is_none());
    let mut source = prepare(
        directory.path(),
        request.clone(),
        RecentConversationLimits::default(),
    );
    let cancelled = SourceCaptureControl::default();
    let mut pending = source
        .start_page(request, cancelled.clone())
        .unwrap()
        .unwrap();
    cancelled.cancel();
    assert_eq!(
        pending.step(&mut source, budget()).err().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "still running"),
    );
    assert!(matches!(
        flow.state().state(),
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
    ));
}

#[test]
fn source_index_markers_selection_and_body_have_distinct_admission() {
    let (directory, mut flow, lease) = fixture();
    for text in ["one", "two", "three"] {
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), text),
        );
    }
    let request = request(&flow, 2);
    let session =
        RuntimeStore::inspect_session(directory.path(), request.flow.session_id.0).unwrap();
    let mut preparation = session
        .prepare_recent_conversation(
            request.clone(),
            RecentConversationLimits {
                retained_positions: nonzero(1),
                ..Default::default()
            },
            SourceCaptureControl::default(),
        )
        .unwrap();
    loop {
        match preparation.step(budget()).unwrap() {
            ConversationPreparationStep::Loading(_) => {}
            ConversationPreparationStep::Capacity(capacity) => {
                assert_eq!(capacity.kind, ConversationCapacityKind::RetainedIndex);
                assert!(!capacity.restart_required);
                break;
            }
            ConversationPreparationStep::Ready(_) => panic!("retained index capacity bypassed"),
        }
    }
    preparation
        .admit_limits(RecentConversationLimits::default())
        .unwrap();
    loop {
        match preparation.step(budget()).unwrap() {
            ConversationPreparationStep::Loading(_) => {}
            ConversationPreparationStep::Ready(_) => break,
            ConversationPreparationStep::Capacity(capacity) => {
                panic!("unexpected capacity: {capacity:?}")
            }
        }
    }
    let mut preparation = session
        .prepare_recent_conversation(
            request.clone(),
            RecentConversationLimits {
                markers: nonzero(1),
                ..Default::default()
            },
            SourceCaptureControl::default(),
        )
        .unwrap();
    loop {
        match preparation.step(budget()).unwrap() {
            ConversationPreparationStep::Loading(_) => {}
            ConversationPreparationStep::Capacity(capacity) => {
                assert_eq!(capacity.kind, ConversationCapacityKind::BranchMetadata);
                assert!(capacity.restart_required);
                break;
            }
            ConversationPreparationStep::Ready(_) => panic!("marker capacity bypassed"),
        }
    }
    assert!(preparation.step(budget()).is_err());
    let mut source = prepare(
        directory.path(),
        request.clone(),
        RecentConversationLimits {
            selection_work: nonzero(1),
            ..Default::default()
        },
    );
    assert_eq!(
        source
            .start_page(request.clone(), SourceCaptureControl::default())
            .unwrap()
            .err()
            .unwrap()
            .kind,
        ConversationCapacityKind::SelectionWork
    );
    source.limits = RecentConversationLimits {
        page_source_bytes: nonzero(1),
        ..Default::default()
    };
    let mut pending = source
        .start_page(request.clone(), SourceCaptureControl::default())
        .unwrap()
        .unwrap();
    assert!(matches!(
        pending.step(&mut source, budget()).unwrap(),
        ConversationPageStep::Capacity(ConversationCapacity {
            kind: ConversationCapacityKind::PageSourceBytes,
            ..
        })
    ));
    assert_eq!(pending.source_bytes, 0);
    source.limits = RecentConversationLimits {
        inline_bytes: nonzero(1),
        ..Default::default()
    };
    let page = page(&mut source, request);
    assert_eq!(page.items.len(), 2);
    assert!(
        page.items
            .iter()
            .all(|item| matches!(item.body, ConversationItemBody::Deferred { .. }))
    );
    assert!(page.source_bytes <= source.limits.page_source_bytes.get());
}
