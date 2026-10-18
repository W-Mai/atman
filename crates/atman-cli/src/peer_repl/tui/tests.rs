use super::*;
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef,
    launch::{EntryBinding, LaunchInput},
};
use atman_runtime::source_program::RevisionBundle;

#[tokio::test]
async fn session_navigation_retains_bound_configuration_execution_and_reply_ownership() {
    use atman_tui::{
        control::FlowPlacement,
        flow_hub::{FlowHubPage, HubRequest},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let ids = [Uuid::now_v7(), Uuid::now_v7()];
    let missing_project = dir.path().join("removed-project");
    std::fs::create_dir(&missing_project).unwrap();
    // An instance ID alone must never identify a view or its services.
    let instance = FlowInstanceId::now();
    for (index, id) in ids.into_iter().enumerate() {
        let config_dir = dir.path().join(format!("config-{index}"));
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), format!(
            "[redact]\nenabled = true\ncustom_patterns = [{{kind = 'sample', regex = 'TOKEN-{index}'}}]\n"
        )).unwrap();
        let config = ConfigHub::from_config_dir(config_dir);
        let project = (index == 1).then(|| {
            atman_runtime::storage::ProjectContext::resolve(&config, &missing_project, dir.path())
                .unwrap()
        });
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                project,
                configuration: SessionConfiguration::Atman(config.source()),
                title: format!("Session {index}"),
            })
            .unwrap();
        let address = FlowRef {
            session_id: id.into(),
            instance_id: instance.clone(),
        };
        let creation = Uuid::now_v7();
        let mut flow = session.create_flow(FlowDescriptor {
            address: address.clone(), creation_request: creation, created_by: None,
            content: EntrySnapshot {
                name: "main".into(), path: dir.path().join("main.at"),
                bundle: RevisionBundle { entry: "entry:main.at".into(), edges: Default::default(), sources: [(
                    "entry:main.at".into(),
                    "flow main(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }".into(),
                )].into() },
            }.into(),
        }).unwrap();
        flow.activate_entry(
            LaunchInput {
                workspace: None,
                initial_goal: None,
                inherited_context: vec![],
                result_route: None,
                execution: Some(EntryBinding {
                    user_routes: false,
                    session_start: false,
                    first_turn: atman_rt::TurnId::now(),
                    first_run: atman_rt::RunId::now(),
                    authority: EffectiveAuthority::root(&Default::default(), false, None),
                }),
                request: atman_runtime::flow_store::launch::EntryRequest {
                    request_id: creation,
                    sender: SenderIdentity::User(USER),
                    recipient: address,
                    arguments: [("input".into(), format!("seed-{index}").into())].into(),
                    invocation: BTreeMap::new(),
                    start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
                },
            },
            None,
            |_| Ok(()),
        )
        .unwrap();
        session.select_default(instance.clone()).unwrap();
    }
    std::fs::remove_dir(&missing_project).unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let initial = Attachment::open(
        dir.path(),
        ids[0],
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    )
    .await
    .unwrap();
    let scopes = Arc::new(scopes::Scopes::new(dir.path(), initial));
    let directory = scopes
        .browse(HubRequest::Sessions { before: None })
        .await
        .unwrap();
    assert!(matches!(*directory, FlowHubPage::Sessions(page) if page.total == 2));
    let directory = scopes
        .browse(HubRequest::Flows {
            session: ids[1],
            before: None,
        })
        .await
        .unwrap();
    assert!(matches!(*directory, FlowHubPage::Flows { page, .. } if !page.instances[0].loaded));
    assert_eq!(
        host.sessions(),
        vec![ids[0]],
        "catalog browsing must not attach execution"
    );
    let (one, two) = tokio::join!(scopes.open(ids[1]), scopes.open(ids[1]));
    assert_eq!(one.unwrap().session.id(), two.unwrap().session.id());
    assert_eq!(host.sessions().len(), 2);
    let (commands, mut incoming) = mpsc::unbounded_channel();
    let forms = Arc::new(atman_runtime::session::FormRegistry::default());
    let mut views = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let Navigation::Focus {
            handle, service, ..
        } = scopes
            .focus(
                FlowRef {
                    session_id: id.into(),
                    instance_id: instance.clone(),
                },
                FlowPlacement::Main,
                commands.clone(),
                forms.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("missing view")
        };
        assert_eq!(
            handle.session_name.as_deref(),
            Some(format!("Session {index}").as_str())
        );
        let snapshot = service.worker.snapshot().await.unwrap();
        assert_eq!(
            snapshot.pending.len(),
            1,
            "selecting must not execute queued input"
        );
        let redactor = service.worker.context().sink().redactor().unwrap();
        let (sample, _) = redactor.redact("TOKEN-0 TOKEN-1");
        assert!(!sample.contains(&format!("TOKEN-{index}")));
        assert!(sample.contains(&format!("TOKEN-{}", 1 - index)));
        service.scope.bind(&service.worker).await.unwrap();
        views.push((handle, service));
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    // Pause one Session, submit to both, then deliver the replies in reverse order.
    views[0]
        .1
        .scope
        .command(&views[0].1.worker, ":pause")
        .await
        .unwrap();
    let mut requests = Vec::new();
    for (index, (handle, _)) in views.iter().enumerate() {
        requests.push(
            handle
                .control_tx
                .as_ref()
                .unwrap()
                .send_tracked(TuiControl::Submit(TuiSubmission {
                    text: format!("answer-{index}"),
                    images: vec![],
                    presentation: None,
                    reasoning: None,
                }))
                .unwrap()
                .unwrap(),
        );
    }
    let controls = [
        incoming.recv().await.unwrap(),
        incoming.recv().await.unwrap(),
    ];
    for control in controls.into_iter().rev() {
        let (_, service) = views
            .iter()
            .find(|(_, service)| service.binding == control.ticket.binding)
            .unwrap();
        service
            .scope
            .tui_control(
                &service.worker,
                control.ticket.request_id,
                control.command,
                &control.replies,
                &forms,
            )
            .await
            .unwrap();
    }
    for ((handle, _), request) in views.iter_mut().zip(requests) {
        let reply = handle
            .control_reply_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap();
        assert_eq!(reply.ticket, request);
        assert!(matches!(
            reply.command,
            TuiCommand::SubmissionResult { result: Ok(()) }
        ));
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(views[0].1.worker.snapshot().await.unwrap().pending.len(), 1);
    assert!(
        views[1]
            .1
            .worker
            .snapshot()
            .await
            .unwrap()
            .pending
            .is_empty()
    );
    views[0]
        .1
        .scope
        .command(&views[0].1.worker, ":resume")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    for (index, (handle, _)) in views.iter_mut().enumerate() {
        let mut model = atman_tui::flow_view::FlowViewModel::default();
        model
            .sync_view(
                handle.flow_view.as_mut().unwrap().refresh().await.unwrap(),
                None,
            )
            .unwrap();
        let answers: Vec<_> = model
            .iter()
            .filter_map(|item| match item {
                atman_tui::app::OutputItem::AssistantMd { md, .. } => Some(md.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![format!("seed-{index}"), format!("answer-{index}")]
        );
    }
    for (index, (handle, service)) in views.iter_mut().enumerate() {
        let commands = service.scope.config.config_dir().join("commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(commands.join("echo.at"), format!("flow echo(value: string) -> string {{ return final.answer(message: \"scope-{index}: \" + value) }}")).unwrap();
        handle
            .control_tx
            .as_ref()
            .unwrap()
            .send(TuiControl::Submit(TuiSubmission {
                text: "/echo selected".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            }))
            .unwrap();
        let control = incoming.recv().await.unwrap();
        service
            .scope
            .tui_control(
                &service.worker,
                control.ticket.request_id,
                control.command,
                &control.replies,
                &forms,
            )
            .await
            .unwrap();
        let TuiCommand::SubmissionResult { result } = handle
            .control_reply_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap()
            .command
        else {
            panic!("missing command acknowledgement")
        };
        result.unwrap();
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    for (index, (handle, _)) in views.iter_mut().enumerate() {
        let mut model = atman_tui::flow_view::FlowViewModel::default();
        model
            .sync_view(
                handle.flow_view.as_mut().unwrap().refresh().await.unwrap(),
                None,
            )
            .unwrap();
        let answers: Vec<_> = model
            .iter()
            .filter_map(|item| match item {
                atman_tui::app::OutputItem::AssistantMd { md, .. } => Some(md.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(answers.last().unwrap(), &format!("scope-{index}: selected"));
    }
    let source = views[1].1.scope.clone();
    let original = views[1].1.worker.clone();
    source.command(&original, ":pause").await.unwrap();
    let original_state = original.state().await.unwrap();
    let original_messages = original.context().messages_full();
    let cfg = source.config.config_dir();
    std::fs::write(cfg.join("on_session_start.at"), "flow broken(").unwrap();
    let rejected = Uuid::now_v7();
    assert!(
        scopes
            .create_session(
                (&source).into(),
                rejected,
                scopes::CreationMode::Interactive
            )
            .await
            .is_err()
    );
    assert!(
        !RuntimeStore::session_ids(dir.path())
            .unwrap()
            .contains(&rejected)
    );
    std::fs::write(
        cfg.join("routes.at"),
        "route \"!\" { flow: echo }\ndefault_route { flow: agent }\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("on_session_start.at"),
        "flow startup() -> string { return final.answer(message: \"ready\") }",
    )
    .unwrap();
    let request = Uuid::now_v7();
    let created = scopes
        .create_session((&source).into(), request, scopes::CreationMode::Interactive)
        .await
        .unwrap();
    host.wait_quiescent().await.unwrap();
    assert_eq!(
        scopes
            .create_session((&source).into(), request, scopes::CreationMode::Interactive)
            .await
            .unwrap(),
        created
    );
    let Navigation::Focus {
        mut handle,
        service,
        ..
    } = scopes
        .focus(
            created.clone(),
            FlowPlacement::Main,
            commands.clone(),
            forms.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("missing new view")
    };
    assert_eq!(service.scope.descriptor.project, source.descriptor.project);
    assert_eq!(service.scope.config.source(), source.config.source());
    assert_eq!(
        service
            .worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat(),
        "ready"
    );
    assert!(service.worker.goal().await.unwrap().goal.is_none());
    service
        .scope
        .submit_tui(
            &service.worker,
            Uuid::now_v7(),
            TuiSubmission {
                text: "! selected route".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            },
        )
        .await
        .unwrap();
    host.wait_quiescent().await.unwrap();
    let mut model = atman_tui::flow_view::FlowViewModel::default();
    model
        .sync_view(
            handle.flow_view.as_mut().unwrap().refresh().await.unwrap(),
            None,
        )
        .unwrap();
    let answers: Vec<_> = model
        .iter()
        .filter_map(|item| match item {
            atman_tui::app::OutputItem::AssistantMd { md, .. } => Some(md.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["ready", "scope-1: selected route"]);
    assert_eq!(original.state().await.unwrap(), original_state);
    assert_eq!(original.context().messages_full(), original_messages);
    // Deferred creation and retries retain input without installing execution services.
    let deferred_id = Uuid::now_v7();
    let deferred = scopes
        .create_session(
            (&source).into(),
            deferred_id,
            scopes::CreationMode::Deferred,
        )
        .await
        .unwrap();
    let worker = host.flow(&deferred).await.unwrap();
    let before = worker.snapshot().await.unwrap();
    assert_eq!(before.pending.len(), 1);
    assert!(worker.context().messages_full().is_empty());
    assert_eq!(
        scopes
            .create_session(
                (&source).into(),
                deferred_id,
                scopes::CreationMode::Deferred
            )
            .await
            .unwrap(),
        deferred
    );
    host.wait_quiescent().await.unwrap();
    assert_eq!(worker.snapshot().await.unwrap().pending, before.pending);
    assert!(worker.context().messages_full().is_empty());
    // Attaching services reuses the existing journal owner; explicit binding starts work.
    let attached = scopes.open(deferred_id).await.unwrap();
    assert!(worker.context().messages_full().is_empty());
    attached.bind(&worker).await.unwrap();
    host.wait_quiescent().await.unwrap();
    assert_eq!(worker.context().messages_full().len(), 2);
    assert_eq!(
        worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat(),
        "ready"
    );
    // The same factory creates an idle entry when no startup command is configured.
    std::fs::remove_file(cfg.join("on_session_start.at")).unwrap();
    let waiting = scopes
        .create_session(
            (&source).into(),
            Uuid::now_v7(),
            scopes::CreationMode::Interactive,
        )
        .await
        .unwrap();
    host.wait_quiescent().await.unwrap();
    let worker = host.flow(&waiting).await.unwrap();
    assert!(worker.context().messages_full().is_empty());
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    assert!(
        worker
            .launch_record()
            .await
            .unwrap()
            .unwrap()
            .acceptance
            .is_none()
    );
    host.shutdown().await.unwrap();
    assert!(
        !missing_project.exists(),
        "history navigation must not recreate a missing project"
    );
}

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
                user_routes: false,
                session_start: false,
                first_turn: atman_rt::TurnId::now(),
                first_run: atman_rt::RunId::now(),
                authority: EffectiveAuthority::root(&Default::default(), false, None),
            }),
            request: atman_runtime::flow_store::launch::EntryRequest {
                request_id: entry.creation_request,
                sender: SenderIdentity::User(USER),
                recipient: address.clone(),
                arguments: [("input".into(), "seed".into())].into(),
                invocation: BTreeMap::new(),
                start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
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
    let mut handle = TuiHandle::from_flow(&worker, binding, commands, attachment.config.clone())
        .await
        .unwrap();
    let source = handle.flow_view.as_mut().unwrap();
    source.next().await.unwrap();
    let sender = handle.control_tx.as_ref().unwrap();
    let replies = handle.control_reply_rx.as_mut().unwrap();

    // Control routing reads the authored prompt even if a caller supplies a quote.
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: "  :pause".into(),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "quoted @/not-a-file.png".into(),
        }),
    };
    let ticket = sender
        .send_tracked(TuiControl::Submit(TuiSubmission {
            text: presentation.model_text(),
            images: vec![],
            reasoning: None,
            presentation: Some(presentation),
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
    let info = replies.recv().await.unwrap();
    assert_eq!(info.ticket, ticket);
    assert!(matches!(info.command, TuiCommand::Toast { .. }));
    let ack = replies.recv().await.unwrap();
    assert_eq!(ack.ticket, ticket);
    assert!(matches!(
        ack.command,
        TuiCommand::SubmissionResult { result: Ok(()) }
    ));
    assert_eq!(worker.state().await.unwrap().pauses().len(), 1);
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
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

    let image_path = dir.path().join("inline.png");
    std::fs::write(&image_path, b"\x89PNG\r\n\x1a\n").unwrap();
    let ticket = sender
        .send_tracked(TuiControl::Submit(TuiSubmission {
            text: format!(":attach {}", image_path.display()),
            images: vec![],
            reasoning: None,
            presentation: None,
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
    let reply = replies.recv().await.unwrap();
    assert_eq!(reply.ticket, ticket);
    let TuiCommand::AttachmentImported { result } = reply.command else {
        panic!("missing imported image")
    };
    let imported = result.unwrap();
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(worker.context().pending_image_count(), 0);
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: format!("inspect\n  this @{}", image_path.display()),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "quoted @/not-an-attachment.png".into(),
        }),
    };
    let content = UserContent {
        text: presentation.model_text(),
        images: vec![],
        presentation: Some(presentation),
    };
    let payload = attachment
        .user_payload(&worker, content, BTreeMap::new())
        .await
        .unwrap();
    let InboxPayload::UserSubmission { content, .. } = &payload else {
        panic!("missing input");
    };
    assert_eq!(
        content.presentation.as_ref().unwrap().prompt,
        "inspect\n  this"
    );
    assert_eq!(
        content.text,
        "> quoted @/not-an-attachment.png\n\ninspect\n  this"
    );
    assert_eq!(content.images.len(), 1);
    let captured = content.images[0].clone();
    assert!(
        attachment
            .user_payload(
                &worker,
                UserContent {
                    text: "relative @./inline.png".into(),
                    images: vec![],
                    presentation: None
                },
                BTreeMap::new()
            )
            .await
            .is_err()
    );
    attachment
        .accept_payload(&worker, Uuid::now_v7(), payload, DeliveryMode::NextTurn)
        .await
        .unwrap();
    std::fs::remove_file(image_path).unwrap();
    assert_eq!(
        atman_runtime::attachment_store::image_bytes(&imported).unwrap(),
        b"\x89PNG\r\n\x1a\n"
    );
    attachment.host.wait_quiescent().await.unwrap();
    assert_eq!(
        atman_runtime::attachment_store::image_bytes(&captured).unwrap(),
        b"\x89PNG\r\n\x1a\n"
    );
    let commands = attachment.config.config_dir().join("commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("quoted.at"),
        "flow quoted(value: string) -> string { return final.answer(message: value) }",
    )
    .unwrap();
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: "/quoted authored".into(),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "/unrelated @/not-a-file.png".into(),
        }),
    };
    let text = presentation.model_text();
    let payload = attachment
        .user_payload(
            &worker,
            UserContent {
                text: text.clone(),
                images: vec![],
                presentation: Some(presentation),
            },
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let InboxPayload::Command {
        arguments,
        user: Some(user),
        ..
    } = payload
    else {
        panic!("quoted command not selected");
    };
    assert_eq!(arguments["value"], "authored");
    assert_eq!(user.text, text);
    assert!(user.images.is_empty());

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
