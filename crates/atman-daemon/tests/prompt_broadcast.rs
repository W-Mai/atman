use std::sync::Arc;
use std::time::Duration;

use atman_daemon::DaemonState;
use atman_daemon::prompt_bridge::DaemonPromptResolver;
use atman_rt::parse_file;
use atman_runtime::event::{Event, EventSink};
use atman_runtime::{Executor, tools};
use tempfile::TempDir;

#[tokio::test]
async fn persistent_peers_share_transport_without_sharing_prompt_events_or_cancellation() {
    use atman_runtime::flow_authority::EffectiveAuthority;
    use atman_runtime::flow_runtime::{FlowWorker, execution::TurnRequest};
    use atman_runtime::flow_store::inbox::{
        DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity,
    };
    use atman_runtime::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowRef, FlowTransition, RuntimeStore, SessionDescriptor,
    };
    use atman_runtime::rendezvous::{PromptContext, PromptId, PromptResolver};
    use atman_runtime::source_program::RevisionBundle;
    use atman_runtime::trust::{TrustConfig, TrustMode};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct RecordingResolver {
        transport: DaemonPromptResolver,
        owners: Mutex<BTreeMap<uuid::Uuid, Arc<atman_runtime::Session>>>,
    }

    impl PromptResolver for RecordingResolver {
        fn register(&self, id: PromptId) -> tokio::sync::oneshot::Receiver<serde_json::Value> {
            self.transport.register(id)
        }

        fn register_in_context(
            &self,
            id: PromptId,
            kind: &str,
            payload: serde_json::Value,
            context: &PromptContext,
        ) -> tokio::sync::oneshot::Receiver<serde_json::Value> {
            self.owners
                .lock()
                .unwrap()
                .insert(id.0, context.session.clone().unwrap());
            self.transport
                .register_in_context(id, kind, payload, context)
        }

        fn drop_pending(&self, id: &PromptId) {
            self.transport.drop_pending(id);
        }

        fn expire_pending(&self, id: &PromptId) -> bool {
            self.transport.expire_pending(id)
        }
    }

    async fn start(
        worker: &FlowWorker,
        executor: Executor,
    ) -> atman_runtime::flow_runtime::execution::TurnTicket {
        let trust = TrustConfig {
            mode: TrustMode::Reckless,
            ..Default::default()
        };
        worker
            .context()
            .update_trust(trust.clone(), |_| Ok(()))
            .unwrap();
        let input = worker
            .accept_input(
                DeliveryRequest {
                    request_id: uuid::Uuid::new_v4(),
                    sender: SenderIdentity::User(uuid::Uuid::new_v4()),
                    recipient: worker.address().clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Interjection { text: "ask".into() },
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        worker
            .start_turn(
                executor,
                TurnRequest {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                    messages: vec![input.message_id],
                    arguments: BTreeMap::new(),
                    invocation: BTreeMap::new(),
                    context: Vec::new(),
                    authority: EffectiveAuthority::root(&trust, false, None),
                    yield_interval: std::num::NonZeroUsize::new(64).unwrap(),
                },
            )
            .await
            .unwrap()
    }

    async fn wait_pending(state: &DaemonState, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.pending_prompt_ids().len() != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    let tmp = TempDir::new().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("daemon")));
    let mut executor = Executor::new();
    let resolver = Arc::new(RecordingResolver {
        transport: DaemonPromptResolver::new(&state),
        owners: Mutex::new(BTreeMap::new()),
    });
    executor.tool_ctx.prompt_resolver = Some(resolver.clone());
    let root = RuntimeStore::open(tmp.path()).unwrap();
    let id = uuid::Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            id,
            title: "Prompts".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let mut workers = Vec::new();
    for parallel in [false, true] {
        let call = if parallel {
            "fanout [helper()]"
        } else {
            "helper().await"
        };
        let descriptor = FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: atman_rt::FlowInstanceId::now(),
            },
            creation_request: uuid::Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    edges: BTreeMap::new(),
                    sources: BTreeMap::from([(
                        "entry:main.at".into(),
                        format!(
                            r#"
                        flow main() -> any {{ return {call} }}
                        flow helper() -> any {{ return form.ask(kind: "confirm", prompt: "Proceed?") }}
                    "#
                        ),
                    )]),
                },
            }.into(),
        };
        let mut store = session.create_flow(descriptor).unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        workers.push(FlowWorker::start(store).await.unwrap());
    }
    let first = start(&workers[0], executor.clone()).await;
    let second = start(&workers[1], executor.clone()).await;
    wait_pending(&state, 2).await;
    let mut prompts = Vec::new();
    for worker in &workers {
        let trace = worker.execution_events().await.unwrap();
        let ids: Vec<_> = trace
            .iter()
            .flat_map(|batch| &batch.events)
            .filter_map(|event| match event.event {
                Event::PendingPrompt { prompt_id, .. } => Some(prompt_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 1);
        assert!(Arc::ptr_eq(
            &resolver.owners.lock().unwrap()[&ids[0]],
            &worker.context()
        ));
        prompts.push(atman_proto::PromptId(ids[0]));
    }
    assert_ne!(prompts[0], prompts[1]);
    let answer = serde_json::to_value(atman_runtime::form::FormSubmission::Submitted {
        answers: vec![atman_runtime::form::FormAnswer::Confirmed { value: true }],
    })
    .unwrap();
    for prompt in prompts.iter().rev() {
        assert!(state.resolve_prompt(prompt, answer.clone()).await.unwrap());
        assert!(!state.resolve_prompt(prompt, answer.clone()).await.unwrap());
    }
    for ticket in [first, second] {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), ticket.wait())
                .await
                .unwrap()
                .unwrap()
                .result
                .is_ok()
        );
    }
    let cancelled = start(&workers[0], executor.clone()).await;
    wait_pending(&state, 1).await;
    let cancelled_prompt = state.pending_prompt_ids().remove(0);
    workers[0]
        .transition(
            workers[0].state().await.unwrap().revision(),
            FlowTransition::PhaseChanged {
                lease: cancelled.lease.clone(),
                phase: atman_rt::TurnPhase::Cancelling,
            },
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), cancelled.wait())
            .await
            .unwrap()
            .unwrap()
            .result
            .is_err()
    );
    assert!(state.pending_prompt_ids().is_empty());
    assert!(executor.events.snapshot().is_empty());
    for (index, worker) in workers.into_iter().enumerate() {
        let address = worker.address().clone();
        let store = worker.shutdown().await.unwrap();
        let trace = store.execution_events();
        let resolved: Vec<_> = trace
            .iter()
            .flat_map(|batch| &batch.events)
            .filter_map(|event| match &event.event {
                Event::PromptResolved { prompt_id, answer } => Some((*prompt_id, answer.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(resolved[0], (prompts[index].0, answer.clone()));
        assert_eq!(resolved.len(), if index == 0 { 2 } else { 1 });
        if index == 0 {
            assert_eq!(resolved[1], (cancelled_prompt.0, serde_json::Value::Null));
        }
        let expected = serde_json::to_value(trace).unwrap();
        drop(store);
        let reopened = session.open_flow(&address.instance_id).unwrap();
        assert_eq!(
            serde_json::to_value(reopened.execution_events()).unwrap(),
            expected
        );
        if index == 0 {
            let worker = FlowWorker::start(reopened).await.unwrap();
            let context = worker.context();
            let owner = atman_proto::SessionId(context.id().0);
            let prompt = atman_proto::PromptId(uuid::Uuid::now_v7());
            let payload = serde_json::to_value(atman_runtime::form::CompositeForm {
                questions: vec![atman_runtime::form::FormQuestion {
                    id: "choice".into(),
                    kind: atman_runtime::form::FormKind::Confirm {
                        prompt: "Proceed?".into(),
                    },
                }],
            })
            .unwrap();
            let waiting = state.register_pending_prompt_broadcast_with_session(
                prompt.clone(),
                "form_ask",
                payload.clone(),
                EventSink::new(),
                Some(&context),
            );
            assert!(state.expire_pending_prompt(&prompt));
            assert!(waiting.await.is_err());
            let wrong_owner = atman_proto::SessionId(uuid::Uuid::now_v7());
            assert!(
                !state
                    .resolve_prompt_for_session(&prompt, answer.clone(), Some(&wrong_owner))
                    .await
                    .unwrap()
            );
            let (first, retry) = tokio::join!(
                state.resolve_prompt_for_session(&prompt, answer.clone(), Some(&owner)),
                state.resolve_prompt_for_session(&prompt, answer.clone(), Some(&owner)),
            );
            assert_eq!(usize::from(first.unwrap()) + usize::from(retry.unwrap()), 1);
            assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
            let unavailable = atman_proto::PromptId(uuid::Uuid::now_v7());
            let waiting = state.register_pending_prompt_broadcast_with_session(
                unavailable.clone(),
                "form_ask",
                payload,
                EventSink::new(),
                Some(&context),
            );
            assert!(state.expire_pending_prompt(&unavailable));
            assert!(waiting.await.is_err());
            drop(worker.shutdown().await.unwrap());
            assert!(
                state
                    .resolve_prompt_for_session(&unavailable, answer.clone(), Some(&owner))
                    .await
                    .is_err()
            );
            assert_eq!(state.pending_prompt_ids(), vec![unavailable]);
            let reopened = FlowWorker::start(session.open_flow(&address.instance_id).unwrap())
                .await
                .unwrap();
            assert_eq!(reopened.snapshot().await.unwrap().pending.len(), 1);
            drop(reopened.shutdown().await.unwrap());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hunk_review_emits_pending_and_resolved_events_to_shared_sink() {
    let tmp = TempDir::new().unwrap();
    let daemon_state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let sink = EventSink::new();

    let file_path = tmp.path().join("input.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    let mut ex = Executor::with_events(sink.clone());
    tools::register_tier_zero(&ex.tools);
    ex.tool_ctx.prompt_resolver = Some(Arc::new(DaemonPromptResolver::new(&daemon_state)));

    let src = format!(
        r#"flow t() -> string {{
    proposal = hunk.plan_edit(
        path: "{}",
        new_content: "line1\nrewritten\nline3\n"
    )
    review = hunk.review(proposal: proposal)
    return to_json_string(review)
}}
"#,
        file_path.display()
    );
    let file = parse_file(&src).unwrap();

    let state_for_resolver = daemon_state.clone();
    let resolver_answer = serde_json::json!({"hunks": [1]});
    let answer_clone = resolver_answer.clone();
    let resolver_task = tokio::spawn(async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let pending = state_for_resolver.pending_prompt_ids();
            if let Some(pid) = pending.first() {
                let ok = state_for_resolver
                    .resolve_prompt(pid, answer_clone)
                    .await
                    .unwrap();
                assert!(ok, "resolve should succeed");
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for pending prompt");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let local = tokio::task::LocalSet::new();
    let _result = local
        .run_until(async move { ex.run(&file, "t", vec![]).await })
        .await
        .expect("flow ok");
    resolver_task.await.expect("resolver join");

    let events = sink.snapshot();

    let pending = events.iter().find_map(|e| match e {
        Event::PendingPrompt {
            prompt_id,
            kind,
            payload,
            ..
        } => Some((*prompt_id, kind.clone(), payload.clone())),
        _ => None,
    });
    let (pending_id, kind, payload) = pending.expect("expected PendingPrompt event");
    assert_eq!(kind, "hunk_selection");
    let hunks_json = payload
        .get("hunks")
        .and_then(|v| v.as_array())
        .expect("payload.hunks array");
    assert!(!hunks_json.is_empty(), "at least one hunk expected");
    let first = &hunks_json[0];
    assert!(first.get("id").is_some(), "hunk missing id: {first:?}");
    assert!(
        first
            .get("unified_diff")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("rewritten"))
            .unwrap_or(false),
        "unified_diff must include the change: {first:?}"
    );
    let path_str = payload
        .get("path")
        .and_then(|v| v.as_str())
        .expect("payload.path");
    assert!(path_str.contains("input.txt"), "path: {path_str}");

    let resolved = events.iter().find_map(|e| match e {
        Event::PromptResolved {
            prompt_id, answer, ..
        } if *prompt_id == pending_id => Some(answer.clone()),
        _ => None,
    });
    let answer = resolved.expect("expected PromptResolved for the same prompt_id");
    assert_eq!(answer, resolver_answer);

    let mut pending_seq = None;
    let mut resolved_seq = None;
    let envelopes = sink.snapshot_envelopes();
    for e in &envelopes {
        match &e.event {
            Event::PendingPrompt { prompt_id, .. } if *prompt_id == pending_id => {
                pending_seq = Some(e.seq);
            }
            Event::PromptResolved { prompt_id, .. } if *prompt_id == pending_id => {
                resolved_seq = Some(e.seq);
            }
            _ => {}
        }
    }
    let (p, r) = (pending_seq.unwrap(), resolved_seq.unwrap());
    assert!(p < r, "resolved seq {r} must come after pending seq {p}");
}

#[tokio::test]
async fn expired_form_answer_is_recorded_once_for_its_session() {
    let tmp = TempDir::new().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = atman_runtime::Session::open_ephemeral();
    let sink = session.sink().clone();
    let prompt_id = atman_proto::PromptId(uuid::Uuid::now_v7());
    let form = atman_runtime::form::CompositeForm {
        questions: vec![atman_runtime::form::FormQuestion {
            id: "question".into(),
            kind: atman_runtime::form::FormKind::Confirm {
                prompt: "Proceed?".into(),
            },
        }],
    };
    let rx = state.register_pending_prompt_broadcast_with_session(
        prompt_id.clone(),
        "form_ask",
        serde_json::to_value(form).unwrap(),
        sink.clone(),
        Some(&session),
    );
    state.expire_pending_prompt(&prompt_id);
    assert!(rx.await.is_err());
    let answer = serde_json::to_value(atman_runtime::form::FormSubmission::Submitted {
        answers: vec![atman_runtime::form::FormAnswer::Confirmed { value: true }],
    })
    .unwrap();
    let wrong_session = atman_proto::SessionId(uuid::Uuid::now_v7());
    assert!(
        !state
            .resolve_prompt_for_session(&prompt_id, answer.clone(), Some(&wrong_session))
            .await
            .unwrap()
    );
    let owner = atman_proto::SessionId(session.id().0);
    assert!(
        state
            .resolve_prompt_for_session(&prompt_id, answer.clone(), Some(&owner))
            .await
            .unwrap()
    );
    assert!(
        !state
            .resolve_prompt_for_session(&prompt_id, answer, Some(&owner))
            .await
            .unwrap()
    );
    assert_eq!(session.deferred_form_inbox().pending_count(), 1);
    assert!(
        sink.snapshot()
            .iter()
            .any(|event| matches!(event, Event::PromptExpired { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn drop_pending_prompt_emits_null_answer_event() {
    let tmp = TempDir::new().unwrap();
    let daemon_state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let sink = EventSink::new();

    let pid = atman_proto::PromptId(uuid::Uuid::now_v7());
    let _rx = daemon_state.register_pending_prompt_broadcast(
        pid.clone(),
        "user_ask",
        serde_json::json!({"prompt": "still there?"}),
        sink.clone(),
    );

    daemon_state.drop_pending_prompt(&pid);

    let events = sink.snapshot();
    let has_pending = events
        .iter()
        .any(|e| matches!(e, Event::PendingPrompt { prompt_id, .. } if *prompt_id == pid.0));
    assert!(has_pending, "pending should be broadcast");
    let resolved = events.iter().find_map(|e| match e {
        Event::PromptResolved {
            prompt_id, answer, ..
        } if *prompt_id == pid.0 => Some(answer.clone()),
        _ => None,
    });
    assert_eq!(resolved.unwrap(), serde_json::Value::Null);
}
