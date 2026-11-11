use super::*;

use std::sync::Mutex;

use crate::{
    context_plan::{
        ContextRecordAuthority, ContextRecordBody, ContextRecordRetention, ContextRecordSpec,
    },
    flow_store::{SessionDescriptor, source_capture::ConversationBranchId},
    model_registry::{AliasEntry, ModelEntry, ModelRegistry, ProviderConfig},
    provider::{AssistantMessage, LlmRequest, Provider, ProviderRegistry},
    providers::mock::MockProvider,
    source_program::RevisionBundle,
};

struct ObservedProvider {
    calls: Arc<Mutex<Vec<LlmRequest>>>,
    inner: MockProvider,
}

impl Provider for ObservedProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn call<'a>(
        &'a self,
        request: LlmRequest,
    ) -> crate::tool::BoxFut<'a, Result<AssistantMessage, crate::RuntimeError>> {
        self.calls.lock().unwrap().push(request.clone());
        self.inner.call(request)
    }

    fn call_streaming(&self, request: LlmRequest) -> crate::event::Observable<AssistantMessage> {
        self.calls.lock().unwrap().push(request.clone());
        self.inner.call_streaming(request)
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    host: RuntimeHost,
    source: FlowWorker,
    selection: SourceSelection,
    executor: crate::Executor,
    entry: EntrySnapshot,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
    user: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let policy = directory.path().join("quickask.md");
        std::fs::write(
            &policy,
            format!(
                "{}\nWorking directory: {{pwd}}",
                crate::templates::QUICKASK_MD
            ),
        )
        .unwrap();
        let question_source =
            crate::templates::QUICKASK_AT.replace("../prompts/quickask.md", "quickask.md");
        let entry = EntrySnapshot {
            name: "quickask".into(),
            path: directory.path().join("quickask.at"),
            bundle: RevisionBundle {
                entry: "entry:quickask.at".into(),
                sources: [("entry:quickask.at".into(), question_source)].into(),
                edges: Default::default(),
            },
        };
        let host = RuntimeHost::default();
        let id = Uuid::now_v7();
        let session = host
            .create_session(
                directory.path(),
                SessionDescriptor {
                    id,
                    configuration: Default::default(),
                    project: None,
                    title: "Question source".into(),
                },
            )
            .await
            .unwrap();
        let source_address = FlowRef {
            session_id: id.into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let request_id = Uuid::now_v7();
        let user = Uuid::now_v7();
        let cwd = directory.path().join("source-execution-directory");
        std::fs::create_dir(&cwd).unwrap();
        let authority = EffectiveAuthority::root(&Default::default(), true, Some(cwd));
        let source = host.launch_flow_at(FlowDescriptor {
            address: source_address.clone(),
            creation_request: request_id,
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: directory.path().join("main.at"),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: [("entry:main.at".into(), "flow main(question: string) -> string { contract { invocation { user_message: question } } return question }".into())].into(),
                    edges: Default::default(),
                },
            }.into(),
        }, LaunchInput {
            name: None, display_label: None, scope: None, workspace: None, initial_goal: None,
            inherited_context: vec![Message::user_text(TurnId::now(), "Original source evidence")],
            continued_from: None, inherited_outputs: vec![], result_route: None,
            request: EntryRequest {
                request_id, sender: SenderIdentity::User(user), recipient: source_address.clone(),
                arguments: Default::default(), invocation: Default::default(), start: EntryStart::Waiting,
            },
            execution: Some(EntryBinding {
                user_routes: false, session_start: false, first_turn: TurnId::now(),
                first_run: crate::event::FlowRunId::now(), authority,
            }),
        }, session.snapshot().await.unwrap().cursor, |_, _| Ok(())).await.unwrap().worker;
        let cursor = source
            .snapshot()
            .await
            .unwrap()
            .cursor
            .require_durable()
            .unwrap()
            .clone();
        let selection = SourceSelection {
            branch: ConversationBranchId::initial(&source_address, cursor.journal_id),
            flow: source_address,
            cursor,
        };
        let models = Arc::new(ModelRegistry::new());
        let mut config = ProviderConfig::default();
        for (name, context_budget) in [("one", 32_768), ("two", 32_768), ("small", 16_384)] {
            config.models.insert(
                name.into(),
                ModelEntry {
                    model: name.into(),
                    provider: Some("mock".into()),
                    context_budget: Some(context_budget),
                    ..Default::default()
                },
            );
        }
        config.aliases.insert(
            "smart".into(),
            AliasEntry {
                model: "one".into(),
            },
        );
        models.set_provider_config(config);
        let mut executor = crate::Executor::new();
        executor.providers = ProviderRegistry::with_models(models);
        let calls = Arc::new(Mutex::new(Vec::new()));
        executor.providers.register(Arc::new(ObservedProvider {
            calls: calls.clone(),
            inner: MockProvider::new("mock")
                .with_fallback(crate::Value::Str("Independent answer".into())),
        }));
        Self {
            _directory: directory,
            host,
            source,
            selection,
            executor,
            entry,
            calls,
            user,
        }
    }

    fn preparation(&self) -> QuestionPreparation {
        QuestionPreparation {
            entry: self.entry.clone(),
            executor: self.executor.clone(),
            authority: EffectiveAuthority::root(&Default::default(), true, None),
            trust: Default::default(),
            yield_interval: NonZeroUsize::new(16).unwrap(),
        }
    }

    fn request(&self) -> QuestionRequest {
        QuestionRequest {
            id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            source: self.selection.clone(),
            user: self.user,
            content: content("Explain the reference"),
            model: "smart".into(),
            effort: None,
        }
    }
}

fn content(text: &str) -> UserContent {
    UserContent {
        text: text.into(),
        images: vec![],
        presentation: None,
    }
}

async fn completed(worker: &FlowWorker, previous: Option<FlowPosition>) -> FlowPosition {
    let mut changes = worker.subscribe_commits();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            changes.borrow_and_update();
            if let Some(completion) = worker.completion(None).await.unwrap()
                && previous.as_ref() != Some(&completion.receipt)
                && matches!(
                    worker.state().await.unwrap().state(),
                    atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
                )
            {
                return completion.receipt;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn question_service_keeps_creation_frozen_and_refreshes_only_its_idle_context() {
    let fixture = Fixture::new().await;
    let request = fixture.request();
    let preparation = fixture
        .host
        .prepare_question(request, fixture.preparation(), |_| Ok(()))
        .await
        .unwrap();
    let source_position = fixture.source.snapshot().await.unwrap().cursor;
    assert_eq!(preparation.budget().model, "one");
    assert!(preparation.budget().reference_tokens < preparation.budget().context_tokens);
    let session = fixture
        .host
        .session(fixture.selection.flow.session_id.0)
        .unwrap();
    assert!(
        session
            .loaded_flow(&preparation.address().instance_id)
            .is_none()
    );
    let launched = preparation
        .launch(&fixture.host, |_, _| Ok(()))
        .await
        .unwrap();
    let first = completed(&launched.flow.worker, None).await;
    assert_eq!(
        fixture.source.snapshot().await.unwrap().cursor,
        source_position
    );
    let replay = preparation
        .launch(&fixture.host, |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(replay.flow.record.receipt, launched.flow.record.receipt);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    {
        let calls = fixture.calls.lock().unwrap();
        let expected_cwd = preparation
            .input
            .execution
            .as_ref()
            .unwrap()
            .authority
            .workspace_root
            .as_ref()
            .unwrap();
        assert!(
            calls[0]
                .system
                .as_ref()
                .unwrap()
                .contains(&expected_cwd.display().to_string())
        );
        let runtime_tokens = calls[0]
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ContextRecord(record) if record.key() != "quickask.source" => {
                    Some(crate::provider::estimate_tokens(&record.render_for_model()))
                }
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(
            preparation.budget.fixed_tokens,
            crate::context_plan::estimate_fixed_input_tokens(&calls[0].system, &calls[0].tools)
                + runtime_tokens
        );
    }
    assert!(matches!(
        launched.flow.record.receipt,
        FlowPosition::Transient { .. }
    ));
    let inherited = &launched.flow.record.input.inherited_context;
    assert_eq!(
        reference_origin(inherited).unwrap().source,
        preparation.source
    );
    let authority = &launched
        .flow
        .record
        .input
        .execution
        .as_ref()
        .unwrap()
        .authority;
    assert_eq!(&authority.read_only(), authority);
    assert!(launched.flow.record.input.result_route.is_none());

    let followup = question_followup(
        preparation.address().clone(),
        Uuid::now_v7(),
        fixture.user,
        content("Continue this question"),
        "two",
        None,
        &fixture.executor,
    )
    .unwrap();
    fixture.host.deliver(followup, |_| Ok(())).await.unwrap();
    let second = completed(&launched.flow.worker, Some(first)).await;
    assert_eq!(fixture.calls.lock().unwrap()[1].model, "two");

    let source_turn = fixture
        .source
        .context()
        .messages()
        .last()
        .unwrap()
        .turn_id
        .clone();
    fixture
        .source
        .append_idle_context(
            IdleContextUpdate {
                id: Uuid::now_v7(),
                expected: source_position,
                turn: source_turn,
                records: vec![ContextRecordSpec::new(
                    "source.evidence",
                    ContextRecordAuthority::User,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::text("New source evidence"),
                )],
            },
            Arc::default(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let before = launched.flow.worker.context().messages_full();
    let user_turns = |messages: &[Message]| {
        messages
            .iter()
            .filter(|message| message.role == crate::message::MessageRole::User)
            .count()
    };
    let refresh = fixture
        .host
        .prepare_question_refresh(
            QuestionRefreshRequest {
                id: Uuid::now_v7(),
                question: preparation.address().clone(),
                expected: launched.flow.worker.snapshot().await.unwrap().cursor,
                source: preparation.source.clone(),
                model: "two".into(),
                effort: None,
            },
            fixture.preparation(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let receipt = refresh
        .apply(&launched.flow.worker, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        refresh
            .apply(&launched.flow.worker, |_| Ok(()))
            .await
            .unwrap()
            .receipt,
        receipt.receipt
    );
    assert_eq!(fixture.calls.lock().unwrap().len(), 2);
    assert_eq!(
        user_turns(&launched.flow.worker.context().messages_full()),
        user_turns(&before)
    );
    assert_eq!(
        reference_origin(&launched.flow.worker.context().messages())
            .unwrap()
            .source,
        refresh.source
    );
    let recorded = launched
        .flow
        .worker
        .request(|store| store.context_messages())
        .await
        .unwrap();
    assert_eq!(reference_origin(&recorded).unwrap().source, refresh.source);
    fixture
        .host
        .deliver(
            question_followup(
                preparation.address().clone(),
                Uuid::now_v7(),
                fixture.user,
                content("Explain the refreshed reference"),
                "two",
                None,
                &fixture.executor,
            )
            .unwrap(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    completed(&launched.flow.worker, Some(second)).await;
    {
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|call| call.cache_prompt));
        let references = calls[2]
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ContextRecord(record) if record.key() == "quickask.source" => {
                    Some(record)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].revision(), 2);
        assert!(
            references[0]
                .render_for_model()
                .contains("New source evidence")
        );
        assert!(
            !references[0]
                .render_for_model()
                .contains(&fixture.selection.flow.instance_id.to_string())
        );
    }
    drop(replay);
    drop(launched);
    fixture.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn question_service_rejects_denied_stale_and_oversized_preparation_without_owners() {
    let fixture = Fixture::new().await;
    let session = fixture
        .host
        .session(fixture.selection.flow.session_id.0)
        .unwrap();
    let before = session.snapshot().await.unwrap().cursor;
    for mode in 0..3 {
        let mut request = fixture.request();
        let id = request.id;
        if mode == 1 {
            request.source.branch =
                ConversationBranchId::initial(&request.source.flow, Uuid::now_v7());
        } else if mode == 2 {
            request.content.text = "too much input ".repeat(32_768);
        }
        let result = fixture
            .host
            .prepare_question(request, fixture.preparation(), move |_| {
                if mode == 0 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            })
            .await;
        assert!(result.is_err());
        assert!(
            session
                .loaded_flow(&atman_rt::FlowInstanceId(id.into()))
                .is_none()
        );
        assert_eq!(session.snapshot().await.unwrap().cursor, before);
    }
    assert!(fixture.calls.lock().unwrap().is_empty());
    fixture.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn question_model_switch_checks_the_selected_latest_window_without_changing_it() {
    let fixture = Fixture::new().await;
    fixture
        .source
        .append_idle_context(
            IdleContextUpdate {
                id: Uuid::now_v7(),
                expected: fixture.source.snapshot().await.unwrap().cursor,
                turn: fixture.source.context().messages()[0].turn_id.clone(),
                records: vec![ContextRecordSpec::new(
                    "source.evidence",
                    ContextRecordAuthority::User,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::text("large retained evidence ".repeat(2_000)),
                )],
            },
            Arc::default(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let prepared = fixture
        .host
        .prepare_question(fixture.request(), fixture.preparation(), |_| Ok(()))
        .await
        .unwrap();
    let launched = prepared.launch(&fixture.host, |_, _| Ok(())).await.unwrap();
    let worker = &launched.flow.worker;
    completed(worker, None).await;
    let before = worker.snapshot().await.unwrap().cursor;
    let error = validate_question_model_switch(worker, &fixture.executor, "small", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Refresh"), "{error}");
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    assert_eq!(
        validate_question_model_switch(worker, &fixture.executor, "two", None)
            .await
            .unwrap()
            .model,
        "two"
    );
    let messages = worker.context().messages();
    let origin = reference_origin(&messages).unwrap().clone();
    worker
        .append_idle_context(
            IdleContextUpdate {
                id: Uuid::now_v7(),
                expected: before,
                turn: messages.last().unwrap().turn_id.clone(),
                records: vec![ContextRecordSpec::new(
                    "quickask.source",
                    ContextRecordAuthority::Retrieved,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::Reference {
                        text: "Short refreshed evidence".into(),
                        images: vec![],
                        origin: Some(origin),
                    },
                )],
            },
            Arc::default(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let refreshed = worker.snapshot().await.unwrap().cursor;
    assert_eq!(
        validate_question_model_switch(worker, &fixture.executor, "small", None)
            .await
            .unwrap()
            .model,
        "small"
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, refreshed);
    assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    worker
        .append_idle_context(
            IdleContextUpdate {
                id: Uuid::now_v7(),
                expected: refreshed,
                turn: messages.last().unwrap().turn_id.clone(),
                records: vec![ContextRecordSpec::new(
                    "quickask.source",
                    ContextRecordAuthority::Retrieved,
                    ContextRecordRetention::Latest,
                    ContextRecordBody::tombstone(),
                )],
            },
            Arc::default(),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert!(reference_origin(&worker.context().messages()).is_none());
    assert!(
        validate_question_model_switch(worker, &fixture.executor, "small", None)
            .await
            .is_err()
    );
    drop(launched);
    fixture.host.shutdown().await.unwrap();
}
