use super::*;
use atman_runtime::{
    flow_store::{
        FlowDescriptor, FlowRef,
        launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput},
        source_capture::{ConversationBranchId, SourceSelection},
    },
    model_registry::{ModelEntry, ProviderConfig},
    providers::mock::MockProvider,
    source_program::RevisionBundle,
};

struct Fixture {
    _directory: tempfile::TempDir,
    scope: Attachment,
    source: FlowWorker,
    services: Services,
    controls: mpsc::UnboundedSender<BoundControl>,
    incoming: mpsc::UnboundedReceiver<BoundControl>,
    handle: TuiHandle,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("config"));
        std::fs::create_dir_all(config.config_dir().join("commands")).unwrap();
        std::fs::create_dir_all(config.config_dir().join("prompts")).unwrap();
        std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
        std::fs::write(
            config.config_dir().join("commands/quickask.at"),
            atman_runtime::templates::QUICKASK_AT,
        )
        .unwrap();
        std::fs::write(
            config.config_dir().join("prompts/quickask.md"),
            atman_runtime::templates::QUICKASK_MD,
        )
        .unwrap();
        let host = RuntimeHost::default();
        let session_id = Uuid::now_v7();
        let session = host
            .create_session(
                directory.path(),
                SessionDescriptor {
                    id: session_id,
                    configuration: SessionConfiguration::Atman(config.source()),
                    project: None,
                    title: "Question source".into(),
                },
            )
            .await
            .unwrap();
        let flow = FlowRef {
            session_id: session_id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let creation = Uuid::now_v7();
        let source = host.launch_flow_at(FlowDescriptor {
            address: flow.clone(), creation_request: creation, created_by: None,
            content: EntrySnapshot { name: "source".into(), path: directory.path().join("source.at"), bundle: RevisionBundle {
                entry: "entry:source.at".into(), edges: Default::default(),
                sources: [("entry:source.at".into(), "flow source(question: string) -> string { contract { invocation { user_message: question } } return question }".into())].into(),
            } }.into(),
        }, LaunchInput {
            name: None, display_label: None, scope: None, workspace: None, initial_goal: None,
                    inherited_context: vec![atman_runtime::message::Message::user_text(atman_rt::TurnId::now(), "Source evidence")],
            inherited_outputs: vec![], continued_from: None, result_route: None,
            request: EntryRequest { request_id: creation, sender: SenderIdentity::User(USER), recipient: flow.clone(), arguments: Default::default(), invocation: Default::default(), start: EntryStart::Waiting },
                    execution: Some(EntryBinding { user_routes: false, session_start: false, first_turn: atman_rt::TurnId::now(), first_run: atman_rt::RunId::now(), authority: EffectiveAuthority::root(&Default::default(), false, None) }),
        }, session.snapshot().await.unwrap().cursor, |_, _| Ok(())).await.unwrap().worker;
        let (notices, _) = mpsc::unbounded_channel();
        let scopes = scopes::Scopes::empty(
            directory.path(),
            host,
            None,
            Arc::new(prompts::Prompts::new(notices)),
        );
        let scope = scopes.open(session_id).await.unwrap();
        let mut providers = ProviderConfig::default();
        providers.models.insert(
            "question-model".into(),
            ModelEntry {
                model: "question-model".into(),
                provider: Some("mock".into()),
                context_budget: Some(32_768),
                ..Default::default()
            },
        );
        scope.config.models().set_provider_config(providers);
        scope.executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(atman_runtime::Value::Str("Answer".into())),
        ));
        let cursor = source
            .snapshot()
            .await
            .unwrap()
            .cursor
            .require_durable()
            .unwrap()
            .clone();
        let question = QuickAsk::new(
            SourceSelection {
                branch: ConversationBranchId::initial(&flow, cursor.journal_id),
                flow,
                cursor,
            },
            "question-model".into(),
        );
        let (controls, incoming) = mpsc::unbounded_channel();
        let Navigation::Question {
            handle, service, ..
        } = open(
            scope.clone(),
            question,
            Uuid::now_v7(),
            controls.clone(),
            source.context().forms(),
        )
        .await
        .unwrap()
        else {
            panic!("question draft")
        };
        Self {
            _directory: directory,
            scope,
            source,
            services: service.question.unwrap(),
            controls,
            incoming,
            handle: *handle,
        }
    }

    async fn send(&mut self, command: TuiControl) -> (ControlTicket, Navigation) {
        let ticket = self
            .handle
            .control_tx
            .as_ref()
            .unwrap()
            .send_tracked(command)
            .unwrap()
            .unwrap();
        let bound = self.incoming.recv().await.unwrap();
        let navigation = dispatch(
            self.services.clone(),
            bound.ticket,
            bound.replies,
            bound.command,
            self.controls.clone(),
            self.source.context().forms(),
        )
        .await
        .unwrap();
        (ticket, navigation)
    }

    async fn settle(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.scope.host.wait_quiescent(),
        )
        .await
        .unwrap()
        .unwrap();
    }
}

fn message(text: &str) -> TuiControl {
    TuiControl::Submit(TuiSubmission {
        text: text.into(),
        images: vec![],
        reasoning: None,
        presentation: None,
    })
}

#[tokio::test]
async fn question_handoff_retries_original_delivery_without_another_accepted_input() {
    let mut fixture = Fixture::new().await;
    let source_before = fixture.source.snapshot().await.unwrap().cursor;
    let (original, initial) = fixture.send(message("Explain")).await;
    let Navigation::Question {
        kind: Some(HandoffKind::Launch),
        ..
    } = initial
    else {
        panic!("first launch attachment")
    };
    fixture.settle().await;
    let worker = fixture.services.state.lock().await.worker.clone().unwrap();
    let before = worker.snapshot().await.unwrap().cursor;
    let (retry, attachment) = fixture
        .send(TuiControl::Question(Command::Retry(Retry::Handoff {
            kind: HandoffKind::Launch,
            request_id: original.request_id,
        })))
        .await;
    assert_ne!(retry.request_id, original.request_id);
    let Navigation::Question {
        request,
        kind: Some(kind),
        handle,
        service,
        source: Some(source),
        delivery: Some(delivery),
    } = attachment
    else {
        panic!("retried original attachment")
    };
    assert_eq!(request, original.request_id);
    assert_eq!(source, original.binding);
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    delivery
        .send(TuiCommand::QuestionHandoffFailed {
            kind,
            request_id: request,
            error: "controlled document preparation failure".into(),
        })
        .unwrap();
    let failure = fixture
        .handle
        .control_reply_rx
        .as_mut()
        .unwrap()
        .recv()
        .await
        .unwrap();
    assert_eq!(failure.ticket, original);
    let pending = PendingQuestionHandoff {
        source,
        kind,
        request,
        destination: service,
    };
    let destination = handle
        .control_tx
        .as_ref()
        .unwrap()
        .binding()
        .unwrap()
        .clone();
    assert!(pending.accepts(&destination, kind, request));
    assert!(!pending.accepts(&original.binding, kind, request));
    assert!(!pending.accepts(&destination, HandoffKind::Keep, request));
    assert!(!pending.accepts(&destination, kind, Uuid::now_v7()));
    fixture.handle = *handle;
    fixture
        .send(TuiControl::Question(Command::Installed {
            kind,
            request_id: request,
        }))
        .await;
    {
        let service = fixture.services.state.lock().await;
        assert!(service.handoff.is_none() && service.launch.is_none());
    }
    fixture
        .send(TuiControl::Question(Command::Operate(
            Operation::Discard,
            None,
        )))
        .await;
    assert!(
        fixture
            .scope
            .session
            .loaded_flow(&worker.address().instance_id)
            .is_none()
    );
    assert_eq!(
        fixture.source.snapshot().await.unwrap().cursor,
        source_before
    );
    fixture.scope.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_attachment_keeps_execution_and_discard_finds_an_unacknowledged_host_owner() {
    let mut fixture = Fixture::new().await;
    let binding = ViewBinding {
        revision: u64::MAX,
        ..fixture
            .handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone()
    };
    let (sender, replies) = atman_tui::ControlSender::for_flow(binding, fixture.controls.clone());
    fixture.handle.control_tx = Some(sender);
    fixture.handle.control_reply_rx = Some(replies);
    let (original, navigation) = fixture.send(message("Explain")).await;
    assert!(matches!(navigation, Navigation::Applied));
    let failure = fixture
        .handle
        .control_reply_rx
        .as_mut()
        .unwrap()
        .recv()
        .await
        .unwrap();
    assert_eq!(failure.ticket, original);
    assert!(matches!(
        failure.command,
        TuiCommand::QuestionHandoffFailed { .. }
    ));
    fixture.settle().await;
    let worker = {
        let mut service = fixture.services.state.lock().await;
        assert!(service.handoff.is_some() && service.launch.is_some());
        // A lost launch receipt leaves only its prepared identity; disposal must consult the host.
        service.worker.take().unwrap()
    };
    let (_, discarded) = fixture
        .send(TuiControl::Question(Command::Operate(
            Operation::Discard,
            None,
        )))
        .await;
    assert!(
        matches!(discarded, Navigation::Reply(command) if matches!(*command, TuiCommand::QuestionOperationResult { result: Ok(_), .. }))
    );
    assert!(
        fixture
            .scope
            .session
            .loaded_flow(&worker.address().instance_id)
            .is_none()
    );
    assert!(fixture.services.discarded());
    fixture.scope.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_question_input_is_acknowledged_before_resume_failure_and_retry_never_resubmits() {
    let mut fixture = Fixture::new().await;
    let (_, launched) = fixture.send(message("Explain")).await;
    let Navigation::Question {
        request,
        kind: Some(kind),
        handle,
        ..
    } = launched
    else {
        panic!("launch attachment")
    };
    fixture.handle = *handle;
    fixture
        .send(TuiControl::Question(Command::Installed {
            kind,
            request_id: request,
        }))
        .await;
    fixture.settle().await;
    let (_, kept) = fixture
        .send(TuiControl::Question(Command::Operate(
            Operation::Keep,
            None,
        )))
        .await;
    let Navigation::Question {
        request,
        kind: Some(kind),
        handle,
        ..
    } = kept
    else {
        panic!("retention attachment")
    };
    fixture.handle = *handle;
    fixture
        .send(TuiControl::Question(Command::Installed {
            kind,
            request_id: request,
        }))
        .await;
    fixture.services.question.set_retention_pending(false);
    let receipt = {
        let mut service = fixture.services.state.lock().await;
        let receipt = service.receipt.clone().unwrap();
        service.receipt.as_mut().unwrap().request = Uuid::now_v7();
        receipt
    };
    let (submitted, result) = fixture.send(message("Followup once")).await;
    assert!(matches!(result, Navigation::Applied));
    let replies = fixture.handle.control_reply_rx.as_mut().unwrap();
    let acknowledged = replies.recv().await.unwrap();
    assert_eq!(acknowledged.ticket, submitted);
    assert!(matches!(
        acknowledged.command,
        TuiCommand::SubmissionResult { result: Ok(()) }
    ));
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::Toast { .. }
    ));
    let (worker, accepted) = {
        let mut service = fixture.services.state.lock().await;
        let accepted = service.resume.clone().unwrap();
        assert_eq!(service.question.retry(), Some(Retry::Resume));
        service.receipt = Some(receipt);
        (service.worker.clone().unwrap(), accepted)
    };
    let pending = worker.snapshot().await.unwrap();
    assert!(
        pending
            .pending
            .iter()
            .any(|entry| entry.id == accepted.message_id)
    );
    fixture
        .send(TuiControl::Question(Command::Retry(Retry::Resume)))
        .await;
    fixture.settle().await;
    assert!(fixture.services.state.lock().await.resume.is_none());
    assert_eq!(
        worker
            .context()
            .messages_full()
            .iter()
            .filter(|message| {
                message.origin == atman_runtime::message::MessageOrigin::User
                    && message.text_concat() == "Followup once"
            })
            .count(),
        1
    );
    fixture.scope.host.shutdown().await.unwrap();
}
