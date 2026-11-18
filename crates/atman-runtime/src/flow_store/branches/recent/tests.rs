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
    assert!(newest.has_earlier);
    let before = newest.before.unwrap();
    assert_eq!(before.record.commit, committed.sequence());
    assert_eq!(before.record.item, 0);
    assert_eq!(before.message, 2);
    let older_request = continuation(&source, before, 2);
    let older = page(&mut source, older_request);
    assert_eq!(texts(&older), ["one", "two"]);
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
