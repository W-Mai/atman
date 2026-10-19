use super::*;
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef,
    launch::{EntryBinding, LaunchInput},
};
use atman_runtime::source_program::RevisionBundle;

fn expect_instance(content: atman_tui::FlowViewContent<'_>) -> &FlowView {
    match content {
        atman_tui::FlowViewContent::Instance(view) => view,
        atman_tui::FlowViewContent::Legacy(_) => panic!("expected an execution-backed view"),
    }
}

#[tokio::test]
async fn original_history_navigation_is_read_only_and_keeps_owner_and_reply_identity() {
    use atman_runtime::event_log::legacy::LegacyFlowKey;
    use atman_tui::{
        control::FlowPlacement,
        flow_hub::{FlowHubPage, HubRequest},
    };
    let dir = tempfile::tempdir().unwrap();
    let id = Uuid::now_v7();
    let directory = dir.path().join("sessions").join(id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let run = Uuid::now_v7();
    let child = Uuid::now_v7();
    let source = directory.join("events.jsonl");
    let bytes = [
        serde_json::json!({"type":"flow_start", "run_id":run, "flow_name":"main", "spawned":false}),
        serde_json::json!({"type":"flow_start", "run_id":child, "flow_name":"child", "parent_run_id":run, "spawned":true}),
    ].into_iter().map(|event| format!("{event}\n")).collect::<String>();
    std::fs::write(&source, &bytes).unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let scopes = scopes::Scopes::empty(
        dir.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    );
    let page = scopes
        .browse(HubRequest::LegacyFlows {
            session: id,
            after: None,
        })
        .await
        .unwrap();
    let FlowHubPage::LegacyFlows { flows, total, .. } = *page else {
        panic!("wrong directory")
    };
    assert_eq!(total, 2);
    assert!(flows.contains(&LegacyFlowKey::Default));
    let owner = LegacyFlowKey::SpawnedRun {
        run_id: child.to_string(),
    };
    assert!(flows.contains(&owner));
    let forms = Arc::new(atman_runtime::session::FormRegistry::default());
    let (commands, mut controls) = mpsc::unbounded_channel();
    let mut handles = Vec::new();
    for (owner, placement) in [
        (LegacyFlowKey::Default, FlowPlacement::Main),
        (owner, FlowPlacement::Floating),
    ] {
        let address = ViewAddress::Legacy { session: id, owner };
        let Navigation::Focus {
            mut handle,
            service,
            ..
        } = scopes
            .focus(address.clone(), placement, commands.clone(), forms.clone())
            .await
            .unwrap()
        else {
            panic!("missing view")
        };
        assert!(service.execution.is_none());
        assert!(handle.session.is_none());
        assert_eq!(service.binding.flow, address);
        let mut model = atman_tui::flow_view::FlowViewModel::default();
        let reader = handle.flow_view.as_mut().unwrap();
        model
            .sync_content(reader.next().await.unwrap(), None)
            .unwrap();
        assert!(model.is_archive());
        assert!(model.flow().is_none());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), reader.next())
                .await
                .is_err()
        );
        assert!(
            !model
                .sync_content(reader.refresh().await.unwrap(), None)
                .unwrap()
        );
        let ticket = handle
            .control_tx
            .as_ref()
            .unwrap()
            .send_tracked(TuiControl::Submit(TuiSubmission {
                text: "must remain a draft".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            }))
            .unwrap()
            .unwrap();
        let control = controls.recv().await.unwrap();
        assert_eq!(ticket.binding.flow, address);
        handles.push((handle, ticket, control));
    }
    // Complete requests in reverse order after both views exist.
    for (mut handle, ticket, control) in handles.into_iter().rev() {
        reject_archive_control(control.command, &control.replies);
        let reply = handle
            .control_reply_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap();
        assert_eq!(reply.ticket, ticket);
        assert!(matches!(
            reply.command,
            TuiCommand::SubmissionResult { result: Err(_) }
        ));
    }
    assert!(host.sessions().is_empty());
    assert!(!dir.path().join("runtime-v1").exists());
    assert_eq!(std::fs::read_to_string(&source).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
}

#[tokio::test]
async fn archive_navigation_uses_no_execution_owner_and_retains_bound_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("old.jsonl");
    std::fs::write(&source, b"retained original line\n").unwrap();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let old = Uuid::now_v7();
    let id = atman_runtime::flow_store::migration::destination_id(&source, old).unwrap();
    let descriptor = SessionDescriptor {
        id,
        title: "Archive".into(),
        project: None,
        configuration: Default::default(),
    };
    let receipt = runtime
        .archive_legacy(&source, old, descriptor, true)
        .unwrap();
    std::fs::remove_file(source).unwrap();
    // Both writer leases remain owned elsewhere; browsing must not acquire either.
    let mut session = runtime.open_session(id).unwrap();
    let instance = receipt
        .flows
        .iter()
        .find(|flow| flow.owner == atman_runtime::event_log::legacy::LegacyFlowKey::Unassigned)
        .unwrap()
        .instance
        .clone();
    let writer = session.open_flow(&instance).unwrap();
    session.select_default(instance.clone()).unwrap();
    let before = std::fs::read(writer.dir().join("events.jsonl")).unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        dir.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let Prepared::Archive(view, descriptor) = prepare(&scopes, id).await.unwrap() else {
        panic!("archive startup acquired execution services")
    };
    assert_eq!(view.transcript().flow().instance_id, instance);
    assert_eq!(descriptor.id, id);
    assert!(!dir.path().join("runtime-v1/host").exists());
    assert!(!dir.path().join("runtime-v1/.profile.lock").exists());
    let forms = Arc::new(atman_runtime::session::FormRegistry::default());
    let (commands, mut controls) = mpsc::unbounded_channel();
    for placement in [
        atman_tui::control::FlowPlacement::Main,
        atman_tui::control::FlowPlacement::Floating,
    ] {
        let Navigation::Focus {
            mut handle,
            service,
            ..
        } = scopes
            .focus(
                FlowRef {
                    session_id: id.into(),
                    instance_id: instance.clone(),
                }
                .into(),
                placement,
                commands.clone(),
                forms.clone(),
            )
            .await
            .unwrap()
        else {
            panic!("archive view missing")
        };
        assert!(service.execution.is_none());
        assert!(handle.session.is_none());
        assert!(handle.stream_rx.is_none());
        assert!(handle.live_output.is_none());
        assert!(!service.can_detach());
        let mut model = atman_tui::flow_view::FlowViewModel::default();
        let reader = handle.flow_view.as_mut().unwrap();
        model
            .sync_content(reader.next().await.unwrap(), None)
            .unwrap();
        assert!(model.is_archive());
        assert!(model.iter().any(|item| matches!(item, atman_tui::app::OutputItem::SystemNote { text, .. } if text.contains("retained original line"))));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), reader.next())
                .await
                .is_err()
        );
        assert!(
            !model
                .sync_content(reader.refresh().await.unwrap(), None)
                .unwrap()
        );
        let ticket = handle
            .control_tx
            .as_ref()
            .unwrap()
            .send_tracked(TuiControl::Submit(TuiSubmission {
                text: "must remain a draft".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            }))
            .unwrap()
            .unwrap();
        let control = controls.recv().await.unwrap();
        reject_archive_control(control.command, &control.replies);
        let reply = handle
            .control_reply_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap();
        assert_eq!(reply.ticket, ticket);
        assert!(matches!(
            reply.command,
            TuiCommand::SubmissionResult { result: Err(_) }
        ));
        drop(handle);
        assert!(service.can_detach());
    }
    assert!(host.sessions().is_empty());
    assert_eq!(
        std::fs::read(writer.dir().join("events.jsonl")).unwrap(),
        before
    );
}

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
    let project_reply = crate::tui_projects::request(
        dir.path().to_owned(),
        atman_tui::project_hub::SessionTarget::Persistent(ids[0]),
        None,
        Some(ConfigHub::from_config_dir(dir.path().join("config-0"))),
        Some(&scopes.catalog),
        atman_tui::project_hub::ProjectRequest {
            id: Uuid::new_v4(),
            mutation: None,
        },
    )
    .await;
    let TuiCommand::ProjectCatalogUpdated {
        result: Ok(projects),
        ..
    } = project_reply
    else {
        panic!("project directory failed");
    };
    let fingerprint = atman_runtime::session_meta::fingerprint_from_root(&missing_project);
    assert_eq!(projects.projects.len(), 1);
    assert!(!projects.details[&fingerprint].path_available);
    assert!(
        matches!(projects.sessions[&fingerprint][0].target, atman_tui::project_hub::SessionTarget::Persistent(id) if id == ids[1])
    );
    assert!(
        projects.sessions[&fingerprint][0]
            .activity
            .contains("Detached")
    );
    assert_eq!(
        host.sessions(),
        vec![ids[0]],
        "project discovery cannot attach an executor"
    );
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
                }
                .into(),
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
        let snapshot = service
            .execution
            .as_ref()
            .unwrap()
            .worker
            .snapshot()
            .await
            .unwrap();
        assert_eq!(
            snapshot.pending.len(),
            1,
            "selecting must not execute queued input"
        );
        let redactor = service
            .execution
            .as_ref()
            .unwrap()
            .worker
            .context()
            .sink()
            .redactor()
            .unwrap();
        let (sample, _) = redactor.redact("TOKEN-0 TOKEN-1");
        assert!(!sample.contains(&format!("TOKEN-{index}")));
        assert!(sample.contains(&format!("TOKEN-{}", 1 - index)));
        service
            .execution
            .as_ref()
            .unwrap()
            .scope
            .bind(&service.execution.as_ref().unwrap().worker)
            .await
            .unwrap();
        views.push((handle, service));
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    // Rename only the bound Session and preserve the flow's execution journal.
    let previous = views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .worker
        .snapshot()
        .await
        .unwrap()
        .cursor;
    views[0]
        .0
        .control_tx
        .as_ref()
        .unwrap()
        .send(TuiControl::Submit(TuiSubmission {
            text: ":rename Renamed Session".into(),
            images: vec![],
            presentation: None,
            reasoning: None,
        }))
        .unwrap();
    let control = incoming.recv().await.unwrap();
    views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .scope
        .tui_control(
            &views[0].1.execution.as_ref().unwrap().worker,
            control.ticket.request_id,
            control.command,
            &control.replies,
            &forms,
        )
        .await
        .unwrap();
    let replies = views[0].0.control_reply_rx.as_mut().unwrap();
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::Toast { .. }
    ));
    assert!(matches!(
        replies.recv().await.unwrap().command,
        TuiCommand::SubmissionResult { result: Ok(()) }
    ));
    let names = views[0].0.session_name_rx.as_mut().unwrap();
    names.changed().await.unwrap();
    assert_eq!(
        names.borrow_and_update().as_deref(),
        Some("Renamed Session")
    );
    assert!(
        !views[1]
            .0
            .session_name_rx
            .as_ref()
            .unwrap()
            .has_changed()
            .unwrap()
    );
    assert_eq!(
        views[0]
            .1
            .execution
            .as_ref()
            .unwrap()
            .worker
            .snapshot()
            .await
            .unwrap()
            .cursor,
        previous
    );
    assert!(
        views[0]
            .1
            .execution
            .as_ref()
            .unwrap()
            .scope
            .command(&views[0].1.execution.as_ref().unwrap().worker, ":rename")
            .await
            .unwrap()
            .contains("Renamed Session")
    );
    views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .scope
        .command(
            &views[0].1.execution.as_ref().unwrap().worker,
            ":rename clear",
        )
        .await
        .unwrap();
    let names = views[0].0.session_name_rx.as_mut().unwrap();
    names.changed().await.unwrap();
    assert!(names.borrow_and_update().is_none());
    let (reopened, _, _) = views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .scope
        .prepare_tui_view(
            &views[0].1.execution.as_ref().unwrap().worker,
            commands.clone(),
            &forms,
        )
        .await
        .unwrap();
    assert!(reopened.session_name.is_none());
    assert_eq!(
        views[0]
            .1
            .execution
            .as_ref()
            .unwrap()
            .worker
            .snapshot()
            .await
            .unwrap()
            .cursor,
        previous
    );
    // Pause one Session, submit to both, then deliver the replies in reverse order.
    views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .scope
        .command(&views[0].1.execution.as_ref().unwrap().worker, ":pause")
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
            .execution
            .as_ref()
            .unwrap()
            .scope
            .tui_control(
                &service.execution.as_ref().unwrap().worker,
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
    assert_eq!(
        views[0]
            .1
            .execution
            .as_ref()
            .unwrap()
            .worker
            .snapshot()
            .await
            .unwrap()
            .pending
            .len(),
        1
    );
    assert!(
        views[1]
            .1
            .execution
            .as_ref()
            .unwrap()
            .worker
            .snapshot()
            .await
            .unwrap()
            .pending
            .is_empty()
    );
    views[0]
        .1
        .execution
        .as_ref()
        .unwrap()
        .scope
        .command(&views[0].1.execution.as_ref().unwrap().worker, ":resume")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    for (index, (handle, _)) in views.iter_mut().enumerate() {
        let mut model = atman_tui::flow_view::FlowViewModel::default();
        model
            .sync_content(
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
        let commands = service
            .execution
            .as_ref()
            .unwrap()
            .scope
            .config
            .config_dir()
            .join("commands");
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
            .execution
            .as_ref()
            .unwrap()
            .scope
            .tui_control(
                &service.execution.as_ref().unwrap().worker,
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
            .sync_content(
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
    let source = views[1].1.execution.as_ref().unwrap().scope.clone();
    let original = views[1].1.execution.as_ref().unwrap().worker.clone();
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
        .create_tui_session((&source).into(), request, commands.clone(), forms.clone())
        .await
        .unwrap()
    else {
        panic!("missing new view")
    };
    assert_eq!(
        service.execution.as_ref().unwrap().scope.descriptor.project,
        source.descriptor.project
    );
    assert_eq!(
        service.execution.as_ref().unwrap().scope.config.source(),
        source.config.source()
    );
    let welcome = handle.startup_card.as_ref().unwrap();
    assert!(matches!(
        welcome.directory,
        atman_tui::app::StartupDirectory::Projects
    ));
    assert_eq!(welcome.recent.len(), 1);
    assert!(
        matches!(&welcome.recent[0].target, atman_tui::app::StartupSessionTarget::Flow(flow) if flow == original.address())
    );
    assert!(welcome.recent[0].activity.contains("1 flow"));
    assert!(handle.initial_items.is_empty());
    assert_eq!(
        service
            .execution
            .as_ref()
            .unwrap()
            .worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat(),
        "ready"
    );
    assert!(
        service
            .execution
            .as_ref()
            .unwrap()
            .worker
            .goal()
            .await
            .unwrap()
            .goal
            .is_none()
    );
    service
        .execution
        .as_ref()
        .unwrap()
        .scope
        .submit_tui(
            &service.execution.as_ref().unwrap().worker,
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
        .sync_content(
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
    // An older Session with newer flow activity precedes a newly created Session.
    let read = RuntimeStore::inspect_session(dir.path(), created.session_id.0).unwrap();
    let log = read
        .dir
        .join("flows")
        .join(created.instance_id.to_string())
        .join("events.jsonl");
    let file = std::fs::OpenOptions::new().write(true).open(log).unwrap();
    let activity = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    file.set_modified(activity).unwrap();
    let owners = host.sessions();
    let recent = scopes.startup_recent(&source, 1).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert!(
        matches!(&recent[0].target, atman_tui::app::StartupSessionTarget::Flow(flow) if flow == &created)
    );
    assert_eq!(host.sessions(), owners);
    assert_eq!(worker.snapshot().await.unwrap().cursor, before.cursor);
    assert!(worker.context().messages_full().is_empty());
    // Welcome cards merge original histories without conflating matching UUIDs or writing caches.
    let legacy_dir = dir
        .path()
        .join("sessions")
        .join(created.session_id.to_string());
    std::fs::create_dir_all(&legacy_dir).unwrap();
    let project_root = source.descriptor.project.as_ref().unwrap().root();
    std::fs::write(
        legacy_dir.join("meta.json"),
        serde_json::to_vec(&serde_json::json!({
            "project_root":project_root,
            "project_fingerprint":atman_runtime::session_meta::fingerprint_from_root(project_root),
        }))
        .unwrap(),
    )
    .unwrap();
    let events = legacy_dir.join("events.jsonl");
    std::fs::write(&events, "{}\n").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&events)
        .unwrap()
        .set_modified(activity + std::time::Duration::from_secs(60))
        .unwrap();
    let mixed = scopes.startup_recent(&source, 2).await.unwrap();
    assert_eq!(mixed.len(), 2);
    assert!(
        matches!(&mixed[0].target, atman_tui::app::StartupSessionTarget::Legacy(id) if id == &created.session_id.to_string())
    );
    assert!(
        matches!(&mixed[1].target, atman_tui::app::StartupSessionTarget::Flow(flow) if flow == &created)
    );
    assert_eq!(std::fs::read_dir(&legacy_dir).unwrap().count(), 2);
    assert_eq!(host.sessions(), owners);
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
        flow: address.clone().into(),
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
    // Todo commands update the same view snapshot without creating a turn or resuming it.
    use atman_runtime::{
        flow_store::planning::{PlanningEdit, PlanningRequest},
        memory::todo::TodoStatus,
    };
    worker
        .edit_planning(
            PlanningRequest {
                id: Uuid::now_v7(),
                sender: SenderIdentity::User(USER),
                expected_revision: None,
                edit: PlanningEdit::AddTodo {
                    where_: "view task".into(),
                    why: "test".into(),
                    how: "command".into(),
                    expected_result: "snapshot".into(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let execution = worker.state().await.unwrap();
    for (text, expected) in [
        (":todo done 0", Some(TodoStatus::Done)),
        (":todo cancel 0", Some(TodoStatus::Cancelled)),
        (":todo clear", None),
    ] {
        sender
            .send(TuiControl::Submit(TuiSubmission {
                text: text.into(),
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
            TuiCommand::Toast { .. }
        ));
        assert!(matches!(
            replies.recv().await.unwrap().command,
            TuiCommand::SubmissionResult { result: Ok(()) }
        ));
        let view = expect_instance(source.refresh().await.unwrap());
        assert_eq!(
            view.snapshot()
                .planning
                .todos
                .first()
                .map(|todo| &todo.status),
            expected.as_ref()
        );
        assert_eq!(view.snapshot().status.execution, execution);
    }
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
    let view = expect_instance(source.refresh().await.unwrap());
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
        .sync_content(source.refresh().await.unwrap(), None)
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
        expect_instance(source.refresh().await.unwrap())
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
        .sync_content(source.refresh().await.unwrap(), None)
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
