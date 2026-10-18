use super::*;
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef,
    launch::{EntryBinding, LaunchInput},
};
use atman_runtime::source_program::RevisionBundle;

#[tokio::test]
async fn native_tui_routes_input_ack_goal_and_forms_through_the_bound_flow() {
    let dir = tempfile::tempdir().unwrap();
    let config = ConfigHub::from_config_dir(dir.path().join("config"));
    let descriptor = SessionDescriptor {
        id: Uuid::now_v7(),
        project: None,
        configuration: SessionConfiguration::Atman(config.source()),
        title: "Interactive flow".into(),
    };
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(descriptor.clone()).unwrap())
        .unwrap();
    let address = FlowRef {
        session_id: descriptor.id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let worker = session
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: dir.path().join("main.at"),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    edges: Default::default(),
                    sources: [(
                        "entry:main.at".into(),
                        r#"flow main(input: string) -> string {
                contract { invocation { user_message: input } }
                when input == "ask" {
                    answer = form.ask(kind: "text", prompt: "Reply here")
                    return final.answer(message: answer.text)
                }
                return final.answer(message: input)
            }"#
                        .into(),
                    )]
                    .into(),
                },
            }
            .into(),
        })
        .await
        .unwrap();
    let entry = worker.descriptor().await.unwrap();
    host.launch_flow(
        entry.clone(),
        LaunchInput {
            workspace: None,
            initial_goal: None,
            inherited_context: vec![],
            result_route: None,
            execution: Some(EntryBinding {
                session_start: false,
                first_turn: atman_rt::TurnId::now(),
                first_run: atman_rt::RunId::now(),
                authority: EffectiveAuthority::root(&Default::default(), false, None),
            }),
            request: DeliveryRequest {
                request_id: entry.creation_request,
                sender: SenderIdentity::User(USER),
                recipient: address.clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Invocation {
                    arguments: [("input".into(), "seed".into())].into(),
                    invocation: BTreeMap::new(),
                    user: None,
                },
            },
        },
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    let forms = worker.context().forms();
    let mut executor = Executor::new();
    executor.providers =
        atman_runtime::provider::ProviderRegistry::with_models(config.models().clone());
    executor.attach_provider_lifecycle(config.clone()).unwrap();
    executor
        .tools
        .register(Arc::new(atman_runtime::tools::form::FormAsk));
    executor.tool_ctx.prompt_resolver = Some(Arc::new(
        atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
    ));
    let (notices, _rx) = mpsc::unbounded_channel();
    let attachment = Attachment {
        host,
        session,
        descriptor,
        executor,
        config,
        prompts: Arc::new(prompts::Prompts::new(notices)),
        permissions: None,
    };
    attachment.bind(&worker).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        attachment.host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
    let (commands, mut commands_rx) = mpsc::unbounded_channel();
    let binding = ViewBinding {
        view_id: Uuid::now_v7(),
        revision: 1,
        flow: address.clone(),
    };
    let mut handle = TuiHandle::from_flow(&worker, binding, commands)
        .await
        .unwrap();
    let source = handle.flow_view.as_mut().unwrap();
    source.next().await.unwrap();
    let sender = handle.control_tx.as_ref().unwrap();
    let replies = handle.control_reply_rx.as_mut().unwrap();

    attachment.command(&worker, ":pause").await.unwrap();
    let ticket = sender
        .send_tracked(TuiControl::Submit(TuiSubmission {
            text: "queued".into(),
            images: vec![],
            presentation: None,
            reasoning: Some(atman_runtime::provider::ReasoningSelection::Disabled),
        }))
        .unwrap()
        .unwrap();
    let bound = commands_rx.recv().await.unwrap();
    attachment
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &forms,
        )
        .await
        .unwrap();
    let ack = replies.recv().await.unwrap();
    assert_eq!(ack.ticket, ticket);
    match ack.command {
        TuiCommand::SubmissionResult { result } => result.unwrap(),
        _ => panic!("missing submission acknowledgement"),
    }
    let view = source.refresh().await.unwrap();
    assert_eq!(view.snapshot().pending.len(), 1);
    assert_eq!(
        view.snapshot().pending[0].request.request_id,
        ticket.request_id
    );
    let InboxPayload::UserSubmission { invocation, .. } =
        &view.snapshot().pending[0].request.payload
    else {
        panic!("wrong payload")
    };
    assert_eq!(invocation["effort"], "off");

    // The same original request is idempotent even after its ACK has left the host.
    attachment
        .submit_tui(
            &worker,
            ticket.request_id,
            TuiSubmission {
                text: "queued".into(),
                images: vec![],
                presentation: None,
                reasoning: Some(atman_runtime::provider::ReasoningSelection::Disabled),
            },
        )
        .await
        .unwrap();
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    attachment.command(&worker, ":resume").await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        attachment.host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
    let mut model = atman_tui::flow_view::FlowViewModel::default();
    model
        .sync_view(source.refresh().await.unwrap(), None)
        .unwrap();
    assert!(model.snapshot().unwrap().pending.is_empty());
    assert!(model.iter().any(
        |item| matches!(item, atman_tui::app::OutputItem::AssistantMd { md, .. } if md == "queued")
    ));

    sender
        .send(TuiControl::GoalControl {
            request_id: Some(7),
            target: None,
            goal_id: None,
            revision: None,
            action: atman_runtime::goal::GoalAction::Create {
                text: "keep records".into(),
            },
        })
        .unwrap();
    let bound = commands_rx.recv().await.unwrap();
    attachment
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &forms,
        )
        .await
        .unwrap();
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::GoalSaved {
            request_id: 7,
            result: Ok(())
        }
    ));
    sender
        .send(TuiControl::GoalControl {
            request_id: Some(8),
            target: None,
            goal_id: None,
            revision: None,
            action: atman_runtime::goal::GoalAction::Set {
                text: "stale edit".into(),
            },
        })
        .unwrap();
    let bound = commands_rx.recv().await.unwrap();
    attachment
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &forms,
        )
        .await
        .unwrap();
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::GoalSaved {
            request_id: 8,
            result: Err(_)
        }
    ));
    assert_eq!(
        source
            .refresh()
            .await
            .unwrap()
            .snapshot()
            .status
            .goal
            .as_ref()
            .unwrap()
            .objective,
        "keep records"
    );

    let mut form_updates = forms.subscribe();
    attachment
        .submit_tui(
            &worker,
            Uuid::now_v7(),
            TuiSubmission {
                text: "ask".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            },
        )
        .await
        .unwrap();
    let form = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(form) = form_updates.borrow_and_update().first().cloned() {
                break form;
            }
            form_updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    sender
        .send(TuiControl::FormSubmit {
            request_id: 9,
            form_id: form.form_id.clone(),
            submission: atman_runtime::form::FormSubmission::Submitted {
                answers: vec![atman_runtime::form::FormAnswer::TextEntered {
                    text: "answered".into(),
                }],
            },
        })
        .unwrap();
    let bound = commands_rx.recv().await.unwrap();
    attachment
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &forms,
        )
        .await
        .unwrap();
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::FormSubmissionResult {
            request_id: 9,
            result: Ok(_),
            ..
        }
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        attachment.host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
    model
        .sync_view(source.refresh().await.unwrap(), None)
        .unwrap();
    assert!(model.iter().any(|item| matches!(item, atman_tui::app::OutputItem::AssistantMd { md, .. } if md == "answered")));
    attachment.command(&worker, ":close").await.unwrap();
    sender
        .send(TuiControl::Submit(TuiSubmission {
            text: "closed".into(),
            images: vec![],
            presentation: None,
            reasoning: None,
        }))
        .unwrap();
    let bound = commands_rx.recv().await.unwrap();
    attachment
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &forms,
        )
        .await
        .unwrap();
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::SubmissionResult { result: Err(_) }
    ));
    attachment.host.shutdown().await.unwrap();
}
