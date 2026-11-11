use super::*;
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef,
    launch::{EntryBinding, LaunchInput},
};
use atman_runtime::source_program::RevisionBundle;
mod project_scope;

#[tokio::test]
async fn routed_paste_submission_retains_authored_text_and_its_receipt() {
    let data = tempfile::tempdir().unwrap();
    let config = ConfigHub::from_config_dir(data.path().join("config"));
    std::fs::create_dir_all(config.config_dir().join("commands")).unwrap();
    std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
    std::fs::write(
        config.config_dir().join("routes.at"),
        "default_route { flow: echo }\n",
    )
    .unwrap();
    std::fs::write(config.config_dir().join("commands/echo.at"),
        "flow echo(limit: int = 2, text: string) -> string { contract { invocation { user_message: text } } return final.answer(message: text) }\n").unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let flow = scopes
        .create_session(
            scopes::SessionSeed {
                config: config.clone(),
                project: None,
                directory: None,
                prompt_resolver: None,
            },
            Uuid::now_v7(),
            scopes::CreationMode::Deferred,
        )
        .await
        .unwrap();
    let scope = scopes.open(flow.session_id.0).await.unwrap();
    let worker = host.flow(&flow).await.unwrap();
    scope.bind(&worker).await.unwrap();
    let mut editor = atman_tui::input::InputEditor::default();
    editor.insert_str("原来如此，请检查这些配置\n");
    let pasted = (0..52)
        .map(|index| format!("  line{index}: value=\"a=b\" C:\\new\\file"))
        .collect::<Vec<_>>()
        .join("\n");
    editor.ingest_paste(&pasted);
    editor.insert_str("\n和 https://code.example/repo?ref=main&view=tree");
    assert!(editor.buf().contains("[Pasted Text:"));
    let draft = editor.prepare_submission();
    let text = draft.content().text.clone();
    assert!(text.contains(&pasted));
    assert!(!text.contains("[Pasted Text:"));
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: text.clone(),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "/not-a-command @/not-an-image.png x=y".into(),
        }),
    };
    let (commands, mut incoming) = mpsc::unbounded_channel();
    let mut handle = TuiHandle::from_flow(
        &worker,
        ViewBinding {
            view_id: Uuid::now_v7(),
            revision: 1,
            flow: flow.clone().into(),
        },
        commands,
        scope.config.clone(),
    )
    .await
    .unwrap();
    let ticket = handle
        .control_tx
        .as_ref()
        .unwrap()
        .send_tracked(TuiControl::Submit(TuiSubmission {
            text: presentation.model_text(),
            images: vec![],
            reasoning: None,
            presentation: Some(presentation),
        }))
        .unwrap()
        .unwrap();
    let bound = incoming.recv().await.unwrap();
    scope
        .tui_control(
            &worker,
            bound.ticket.request_id,
            bound.command,
            &bound.replies,
            &worker.context().forms(),
        )
        .await
        .unwrap();
    let acknowledgement = handle
        .control_reply_rx
        .as_mut()
        .unwrap()
        .recv()
        .await
        .unwrap();
    assert_eq!(acknowledgement.ticket, ticket);
    let TuiCommand::SubmissionResult { result } = acknowledgement.command else {
        panic!("missing submission acknowledgement");
    };
    result.unwrap();
    assert_eq!(editor.prepare_submission().content().text, text);
    assert!(editor.acknowledge_submission(draft));
    assert!(editor.buf().is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let invocation = worker.snapshot().await.unwrap().last_invocation.unwrap();
    assert_eq!(invocation.arguments["text"], text);
    assert_eq!(invocation.arguments.len(), 1);
    assert!(
        worker
            .context()
            .messages_full()
            .iter()
            .any(
                |message| message.origin == atman_runtime::message::MessageOrigin::FinalAnswer
                    && message.text_concat() == text
            )
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_choices_defer_legacy_ownership_until_open_without_attaching_execution() {
    use atman_runtime::event_log::legacy::LegacyFlowKey;
    use atman_tui::control::FlowPlacement;
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (notices, _) = mpsc::unbounded_channel();
    let scopes = scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    );
    for (bytes, expected) in [
        ("", LegacyFlowKey::Default),
        ("retained invalid record\n", LegacyFlowKey::Unassigned),
    ] {
        let id = Uuid::now_v7();
        let path = data.path().join("sessions").join(id.to_string());
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("events.jsonl"), bytes).unwrap();
        atman_runtime::session_meta::SessionStats::load_or_rebuild(&path).unwrap();
        std::fs::remove_file(path.join("stats.json")).unwrap();
        let current = ViewAddress::Legacy {
            session: id,
            owner: expected.clone(),
        };
        let rows = scopes.session_choices(current.clone()).await.unwrap();
        let choice = rows.iter().find(|row| row.is_current).unwrap();
        assert!(
            !path.join("stats.json").exists(),
            "browsing must not repair the statistics cache"
        );
        assert_eq!(choice.id, format!("legacy:{id}"));
        let selection = choice.selection.as_ref().unwrap();
        assert!(
            selection.destination.is_none(),
            "listing must not resolve history partitions"
        );
        assert!(selection.unavailable.is_none());
        for placement in [FlowPlacement::Main, FlowPlacement::Floating] {
            let resolved = scopes.resolve_legacy_session(id).await.unwrap();
            assert_eq!(resolved, current);
            let (commands, _) = mpsc::unbounded_channel();
            let Navigation::Focus {
                flow,
                service,
                placement: actual,
                ..
            } = scopes
                .focus(resolved, placement, commands, Arc::default())
                .await
                .unwrap()
            else {
                panic!("expected original history view");
            };
            assert_eq!(flow, current);
            assert_eq!(actual, placement);
            assert!(service.execution.is_none());
        }
        assert_eq!(
            std::fs::read_to_string(path.join("events.jsonl")).unwrap(),
            bytes
        );
    }
    assert!(host.sessions().is_empty());
    assert!(!data.path().join("runtime-v1").exists());
}

#[tokio::test]
async fn opening_a_saved_conversation_restores_idle_peers_without_releasing_pauses() {
    use atman_runtime::flow_runtime::scheduler::SchedulingStatus;
    use atman_runtime::flow_store::FlowTransition;
    use atman_runtime::flow_store::launch::{EntryRequest, EntryStart};
    for placement in [
        atman_tui::control::FlowPlacement::Main,
        atman_tui::control::FlowPlacement::Floating,
    ] {
        let data = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        std::fs::create_dir_all(config.config_dir()).unwrap();
        std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let id = Uuid::now_v7();
        let mut saved = root
            .create_session(SessionDescriptor {
                id,
                title: "Saved".into(),
                project: None,
                configuration: SessionConfiguration::Atman(config.source()),
            })
            .unwrap();
        let mut addresses = Vec::new();
        let pause: atman_runtime::event::AtmanUuid = Uuid::now_v7().into();
        for index in 0..3 {
            let address = FlowRef {
                session_id: id.into(),
                instance_id: FlowInstanceId::now(),
            };
            let creation = Uuid::now_v7();
            let mut flow = saved.create_flow(FlowDescriptor {
                address: address.clone(), creation_request: creation, created_by: addresses.first().cloned(),
                content: EntrySnapshot {
                    name: "main".into(), path: data.path().join("main.at"),
                    bundle: RevisionBundle {
                        entry: "entry:main.at".into(), edges: Default::default(),
                        sources: [("entry:main.at".into(), "flow main(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }".into())].into(),
                    },
                }.into(),
            }).unwrap();
            flow.activate_entry(
                LaunchInput {
                    name: None,
                    display_label: None,
                    scope: None,
                    workspace: None,
                    initial_goal: None,
                    inherited_outputs: vec![],
                    continued_from: None,
                    inherited_context: vec![],
                    result_route: None,
                    execution: Some(EntryBinding {
                        user_routes: false,
                        session_start: false,
                        first_turn: atman_rt::TurnId::now(),
                        first_run: atman_rt::RunId::now(),
                        authority: EffectiveAuthority::root(&Default::default(), false, None),
                    }),
                    request: EntryRequest {
                        request_id: creation,
                        sender: SenderIdentity::User(USER),
                        recipient: address.clone(),
                        arguments: [("input".into(), "seed".into())].into(),
                        invocation: BTreeMap::new(),
                        start: EntryStart::Waiting,
                    },
                },
                None,
                |_| Ok(()),
            )
            .unwrap();
            if index == 2 {
                flow.transition(
                    flow.state().revision(),
                    FlowTransition::ExecutionPaused { key: pause },
                )
                .unwrap();
            }
            addresses.push(address);
        }
        saved
            .select_default(&saved.cursor().clone(), addresses[0].instance_id.clone())
            .unwrap();
        drop(saved);
        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = scopes::Scopes::empty(
            data.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        );
        let (controls, _requests) = mpsc::unbounded_channel();
        let forms = Arc::new(atman_runtime::session::FormRegistry::default());
        let view = scopes
            .focus(
                addresses[0].clone().into(),
                placement,
                controls.clone(),
                forms.clone(),
            )
            .await
            .unwrap();
        let session = host.session(id).unwrap();
        for address in &addresses {
            let status = session
                .scheduling_status(address.instance_id.clone())
                .await
                .unwrap();
            assert!(
                status.is_some_and(|status| *status.borrow() == SchedulingStatus::Running),
                "missing scheduler for {address:?}"
            );
        }
        for address in &addresses[1..] {
            host.deliver(
                DeliveryRequest {
                    request_id: Uuid::now_v7(),
                    sender: SenderIdentity::Flow(addresses[0].clone()),
                    recipient: address.clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Interjection {
                        text: "peer follow-up".into(),
                    },
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        let child = host.flow(&addresses[1]).await.unwrap();
        assert!(child.snapshot().await.unwrap().pending.is_empty());
        assert!(
            child
                .context()
                .messages_full()
                .iter()
                .any(|message| message.origin
                    == atman_runtime::message::MessageOrigin::FinalAnswer
                    && message.text_concat().contains("peer follow-up"))
        );
        let paused = host.flow(&addresses[2]).await.unwrap();
        assert_eq!(paused.state().await.unwrap().pauses(), &[pause]);
        assert_eq!(paused.snapshot().await.unwrap().pending.len(), 1);
        assert!(paused.completion(None).await.unwrap().is_none());
        let Navigation::Focus { service, .. } = view else {
            panic!("expected attached conversation");
        };
        assert!(service.can_reuse());
        let previous = service.execution.as_ref().unwrap().worker.clone();
        let messages = previous.context().messages_full().to_vec();
        drop(previous.clone().shutdown().await.unwrap());
        assert!(
            !service.can_reuse(),
            "a cached view cannot reuse a stopped writer"
        );
        let Navigation::Focus {
            service: recovered, ..
        } = scopes
            .focus(addresses[0].clone().into(), placement, controls, forms)
            .await
            .unwrap()
        else {
            panic!("expected recovered conversation");
        };
        assert!(recovered.can_reuse());
        let execution = recovered.execution.as_ref().unwrap();
        assert!(!Arc::ptr_eq(
            &previous.context(),
            &execution.worker.context()
        ));
        assert_eq!(
            execution.worker.context().messages_full().as_slice(),
            messages
        );
        execution
            .scope
            .submit_tui(
                &execution.worker,
                Uuid::now_v7(),
                TuiSubmission {
                    text: "after storage recovery".into(),
                    images: vec![],
                    reasoning: None,
                    presentation: None,
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        assert!(
            execution
                .worker
                .context()
                .messages_full()
                .iter()
                .any(|message| message.origin
                    == atman_runtime::message::MessageOrigin::FinalAnswer
                    && message.text_concat() == "after storage recovery")
        );
        session
            .stop_scheduling(addresses[1].instance_id.clone())
            .await
            .unwrap();
        let scope = scopes.open(id).await.unwrap();
        let before = child.snapshot().await.unwrap().cursor;
        assert!(scope.restore_scheduling().await.unwrap().is_empty());
        assert_eq!(child.snapshot().await.unwrap().cursor, before);
        assert!(
            session
                .scheduling_status(addresses[1].instance_id.clone())
                .await
                .unwrap()
                .is_none()
        );
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn default_flow_details_and_selection_do_not_load_execution_or_replace_stale_catalogs() {
    use atman_tui::flow_hub::{FlowHubPage, HubRequest};
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Example".into(),
        })
        .unwrap();
    let mut addresses = Vec::new();
    let mut originals = Vec::new();
    for name in ["first", "second"] {
        let address = FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let writer = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: name.into(),
                    path: "/missing/main.at".into(),
                    bundle: RevisionBundle {
                        entry: "entry:main.at".into(),
                        sources: [(
                            "entry:main.at".into(),
                            format!("flow {name}() {{ return 1 }}"),
                        )]
                        .into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        let path = writer.dir().unwrap().join("events.jsonl");
        originals.push((path.clone(), std::fs::read(path).unwrap()));
        addresses.push(address);
    }
    drop(session);
    let host = RuntimeHost::default();
    let (notices, _received) = mpsc::unbounded_channel();
    let scopes = scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    );
    let request = HubRequest::FlowDetails {
        owner: addresses[1].clone(),
    };
    let scoped = HubRequest::SessionOverview {
        session: id,
        after: None,
    };
    let page = scopes.browse(scoped.clone()).await.unwrap();
    assert_eq!(page.first(), scoped);
    let FlowHubPage::Overview(directory) = *page else {
        panic!("overview expected")
    };
    assert_eq!(directory.page.entries.len(), 2);
    assert!(directory.page.entries.iter().all(|row| matches!(row,
        atman_runtime::flow_runtime::catalog::FlowCatalogEntry::Flow { owner, snapshot: Ok(_), .. } if owner.session_id.0 == id)));
    assert!(
        host.sessions().is_empty(),
        "overview must not attach execution"
    );
    assert!(
        scopes
            .browse(HubRequest::SessionOverview {
                session: id,
                after: Some(atman_runtime::flow_runtime::catalog::FlowCatalogCursor {
                    session_id: Uuid::now_v7(),
                    before: None
                }),
            })
            .await
            .is_err()
    );
    let FlowHubPage::FlowDetails(details) = *scopes.browse(request.clone()).await.unwrap() else {
        panic!("flow details expected")
    };
    assert!(!details.is_default);
    assert!(
        host.sessions().is_empty(),
        "reading details cannot open an owner"
    );
    let choices = scopes
        .session_choices(addresses[0].clone().into())
        .await
        .unwrap();
    assert_eq!(choices.len(), 1);
    assert!(choices[0].is_current);
    assert_eq!(choices[0].message_count, 0);
    assert!(!choices[0].updated_at.is_empty());
    assert_eq!(
        choices[0].selection.as_ref().unwrap().destination,
        Some(addresses[0].clone().into())
    );
    let from_peer = scopes
        .session_choices(addresses[1].clone().into())
        .await
        .unwrap();
    assert!(
        from_peer[0].is_current,
        "the current Session marker is independent of its focused Flow"
    );
    assert_eq!(
        from_peer[0].selection.as_ref().unwrap().destination,
        Some(addresses[0].clone().into())
    );
    assert!(
        host.sessions().is_empty(),
        "browsing choices cannot attach execution"
    );
    let owner = host.open_session(data.path(), id).await.unwrap();
    owner
        .set_title(details.selection.expected.clone(), Some("Renamed".into()))
        .await
        .unwrap();
    assert!(scopes.select_default_flow(details.selection).await.is_err());
    assert!(
        scopes
            .rename_session_choice(atman_tui::session_switcher::RenameChoice {
                selection: choices[0].selection.clone().unwrap(),
                title: Some("Stale".into())
            })
            .await
            .is_err()
    );
    let choices = scopes
        .session_choices(addresses[0].clone().into())
        .await
        .unwrap();
    scopes
        .rename_session_choice(atman_tui::session_switcher::RenameChoice {
            selection: choices[0].selection.clone().unwrap(),
            title: Some("Picker title".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        scopes
            .catalog
            .overview(id)
            .await
            .unwrap()
            .summary
            .title
            .as_deref(),
        Some("Picker title")
    );
    assert_eq!(
        owner.snapshot().await.unwrap().catalog.default_flow,
        Some(addresses[0].instance_id.clone())
    );
    let FlowHubPage::FlowDetails(details) = *scopes.browse(request).await.unwrap() else {
        panic!("flow details expected")
    };
    let selection = details.selection;
    let FlowHubPage::FlowDetails(updated) =
        *scopes.select_default_flow(selection.clone()).await.unwrap()
    else {
        panic!("updated flow details expected")
    };
    assert!(updated.is_default);
    assert_eq!(updated.selection.flow, addresses[1]);
    let cursor = owner.snapshot().await.unwrap().cursor;
    scopes.select_default_flow(selection).await.unwrap();
    assert_eq!(owner.snapshot().await.unwrap().cursor, cursor);
    assert!(
        owner.observe().await.unwrap().loaded.is_empty(),
        "selection cannot load a flow writer or scheduler"
    );
    for (path, bytes) in originals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let request = HubRequest::FlowDetails {
        owner: addresses[0].clone(),
    };
    let FlowHubPage::FlowDetails(details) = *scopes.browse(request.clone()).await.unwrap() else {
        panic!("details expected")
    };
    assert!(!details.archived);
    let change = atman_tui::flow_hub::FlowArchiveChange {
        selection: details.selection,
        archived: true,
    };
    assert!(scopes.archive_flow(change.clone()).await.is_err());
    assert!(owner.observe().await.unwrap().loaded.is_empty());
    let worker = owner
        .open_flow(addresses[0].instance_id.clone())
        .await
        .unwrap();
    for transition in [
        atman_runtime::flow_store::FlowTransition::CloseRequested,
        atman_runtime::flow_store::FlowTransition::Closed,
    ] {
        worker
            .transition(worker.state().await.unwrap().revision(), transition)
            .await
            .unwrap();
    }
    let FlowHubPage::FlowDetails(details) = *scopes.archive_flow(change.clone()).await.unwrap()
    else {
        panic!("archive details expected")
    };
    assert!(details.archived);
    let directory = scopes
        .catalog
        .instances(id, None, 50, |_| Ok(()))
        .await
        .unwrap();
    assert!(
        directory
            .instances
            .iter()
            .find(|row| row.address == addresses[0])
            .unwrap()
            .archived
    );
    assert!(
        !directory
            .instances
            .iter()
            .find(|row| row.address == addresses[1])
            .unwrap()
            .archived
    );
    assert!(
        scopes
            .archive_flow(atman_tui::flow_hub::FlowArchiveChange {
                archived: false,
                ..change
            })
            .await
            .is_err()
    );
    let FlowHubPage::FlowDetails(restored) = *scopes
        .archive_flow(atman_tui::flow_hub::FlowArchiveChange {
            selection: details.selection,
            archived: false,
        })
        .await
        .unwrap()
    else {
        panic!("restored details expected")
    };
    assert!(!restored.archived);
    assert!(matches!(
        restored.recorded_state,
        atman_rt::InstanceState::Closed
    ));
    assert_eq!(
        owner.snapshot().await.unwrap().catalog.default_flow,
        Some(addresses[1].instance_id.clone())
    );
    host.shutdown().await.unwrap();
    let read = RuntimeStore::inspect_session(data.path(), id).unwrap();
    assert_eq!(
        read.projection.default_flow,
        Some(addresses[1].instance_id.clone())
    );
    assert_eq!(read.projection.title.as_deref(), Some("Picker title"));
}

#[tokio::test]
async fn operator_control_batches_keep_identity_partial_results_and_selective_resume() {
    use atman_runtime::flow_store::{
        FlowTransition, control::ControlAction, profile::ControlProgress,
    };
    use atman_tui::flow_hub::{FlowHubPage, HubRequest, OperatorControl, OperatorControls};
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let (notices, _received) = mpsc::unbounded_channel();
    let scopes = scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    );
    let mut commands = Vec::new();
    let peer_key = Uuid::new_v4();
    for index in 0..2 {
        let mut session = root
            .create_session(SessionDescriptor {
                id: Uuid::new_v4(),
                project: None,
                configuration: Default::default(),
                title: "Operator target".into(),
            })
            .unwrap();
        let address = FlowRef {
            session_id: session.projection().descriptor.id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut flow = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::new_v4(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "entry:main.at".into(),
                        sources: [("entry:main.at".into(), "flow main() { return 1 }".into())]
                            .into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
        flow.transition(
            flow.state().revision(),
            FlowTransition::ExecutionPaused {
                key: peer_key.into(),
            },
        )
        .unwrap();
        commands.push(OperatorControl {
            target: address,
            expected_revision: flow.state().revision() + index,
            action: ControlAction::Pause,
            pause_key: None,
        });
    }
    let request = OperatorControls {
        id: Uuid::new_v4(),
        commands,
    };
    let mut invalid = request.clone();
    invalid.commands[0].pause_key = Some(peer_key);
    assert!(scopes.submit_controls(invalid).await.is_err());
    assert!(!data.path().join("runtime-v1/host/events.jsonl").exists());
    scopes.submit_controls(request.clone()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let FlowHubPage::ControlBatch(page) = *scopes
        .browse(HubRequest::ControlBatch {
            request_id: request.id,
        })
        .await
        .unwrap()
    else {
        panic!("missing results")
    };
    assert_eq!(page.record.sender, SenderIdentity::User(USER));
    assert!(matches!(
        page.record.items[0].history.last().unwrap().progress,
        ControlProgress::Applied { .. }
    ));
    assert!(matches!(
        page.record.items[1].history.last().unwrap().progress,
        ControlProgress::Failed { .. }
    ));
    let worker = host.flow(&request.commands[0].target).await.unwrap();
    let paused = worker.state().await.unwrap();
    assert_eq!(paused.pauses().len(), 2);
    let operator_key = Uuid::from_bytes(*b"atman-user-pause");
    let release = OperatorControls {
        id: Uuid::new_v4(),
        commands: vec![OperatorControl {
            target: request.commands[0].target.clone(),
            expected_revision: paused.revision(),
            action: ControlAction::ReleasePause,
            pause_key: Some(peer_key),
        }],
    };
    scopes.submit_controls(release.clone()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let released = worker.state().await.unwrap();
    assert_eq!(released.pauses(), &[operator_key.into()]);
    let resume = OperatorControls {
        id: Uuid::new_v4(),
        commands: vec![OperatorControl {
            expected_revision: released.revision(),
            action: ControlAction::Resume,
            pause_key: None,
            ..request.commands[0].clone()
        }],
    };
    scopes.submit_controls(resume).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(worker.state().await.unwrap().pauses().is_empty());
    let cursor = worker.snapshot().await.unwrap().cursor;
    let FlowHubPage::ControlBatch(retry) = *scopes.submit_controls(request.clone()).await.unwrap()
    else {
        panic!("missing retry")
    };
    assert_eq!(retry.record, page.record);
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    // History retries retain transport-supplied command identities, not only the batch UUID.
    let distinct = atman_runtime::flow_store::profile::ControlBatchRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(USER),
        commands: vec![atman_runtime::flow_store::control::ControlRequest {
            id: Uuid::new_v4(),
            sender: SenderIdentity::User(USER),
            target: request.commands[0].target.clone(),
            expected_revision: worker.state().await.unwrap().revision(),
            pause_key: operator_key.into(),
            action: ControlAction::Pause,
        }],
    };
    scopes
        .profile()
        .await
        .unwrap()
        .submit_controls(distinct.clone(), |_| Ok(()))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let original = scopes.operator_control_record(distinct.id).await.unwrap();
    let cursor = worker.snapshot().await.unwrap().cursor;
    let FlowHubPage::ControlBatch(retried) = *scopes.retry_controls(distinct.id).await.unwrap()
    else {
        panic!("missing retry")
    };
    assert_eq!(retried.record, original);
    assert_eq!(retried.record.items[0].request.id, distinct.commands[0].id);
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    assert!(scopes.retry_controls(Uuid::new_v4()).await.is_err());
    host.shutdown().await.unwrap();
    drop(scopes);
    let restarted = RuntimeHost::default();
    let (notices, _received) = mpsc::unbounded_channel();
    let scopes = scopes::Scopes::empty(
        data.path(),
        restarted.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    );
    let FlowHubPage::ControlBatch(restored) = *scopes
        .browse(HubRequest::ControlBatch {
            request_id: request.id,
        })
        .await
        .unwrap()
    else {
        panic!("missing history")
    };
    assert_eq!(restored.record, page.record);
    let FlowHubPage::ControlHistory(history) = *scopes
        .browse(HubRequest::ControlHistory { before: None })
        .await
        .unwrap()
    else {
        panic!("missing history directory")
    };
    assert_eq!(history.total, 4);
    assert_eq!(history.batches[0].id, distinct.id);
    assert!(
        history
            .batches
            .iter()
            .any(|batch| batch.id == request.id && batch.applied == 1 && batch.failed == 1)
    );
    assert!(restarted.sessions().is_empty());
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_directory_and_edits_share_profile_cas_without_scheduling() {
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        profile::ReplaceManagement,
    };
    use atman_tui::flow_hub::{FlowHubPage, HubRequest};
    let data = tempfile::tempdir().unwrap();
    let config = ConfigHub::from_config_dir(data.path().join("config"));
    std::fs::create_dir_all(config.config_dir()).unwrap();
    std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
    let host = RuntimeHost::default();
    let (notices, _received) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let FlowHubPage::Management(initial) = *scopes
        .browse(HubRequest::Management { grant: None })
        .await
        .unwrap()
    else {
        panic!("missing profile")
    };
    assert!(initial.view.profile.management.is_none());
    assert!(host.sessions().is_empty());
    let source = scopes.open_management(config).await.unwrap();
    let worker = host.flow(&source).await.unwrap();
    let before = worker.snapshot().await.unwrap();
    let profile = scopes.profile().await.unwrap();
    let view = profile.snapshot().await.unwrap();
    let mut binding = view.profile.management.unwrap();
    let grant = FlowGrant {
        id: Uuid::now_v7(),
        grantee: source.clone(),
        target: AccessTarget::AllSessions,
        actions: [FlowAction::Discover, FlowAction::Watch].into(),
    };
    let retained = FlowGrant {
        id: Uuid::now_v7(),
        actions: [FlowAction::ReadOutput].into(),
        ..grant.clone()
    };
    binding.grants = vec![grant.clone(), retained.clone()];
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::now_v7(),
            expected: view.profile.cursor,
            binding,
        })
        .await
        .unwrap();
    let FlowHubPage::Management(detail) = *scopes
        .browse(HubRequest::Management {
            grant: Some(grant.id),
        })
        .await
        .unwrap()
    else {
        panic!("missing grant")
    };
    let mut binding = detail.view.profile.management.clone().unwrap();
    binding.grants.retain(|entry| entry.id != grant.id);
    let change = ReplaceManagement {
        request_id: Uuid::now_v7(),
        expected: detail.view.profile.cursor.clone(),
        binding,
    };
    let FlowHubPage::Management(saved) = *scopes.update_management(change.clone()).await.unwrap()
    else {
        panic!("missing saved profile")
    };
    assert_eq!(
        saved.view.profile.management.as_ref().unwrap().grants,
        vec![retained.clone()]
    );
    let mut stale = change.clone();
    stale.request_id = Uuid::now_v7();
    assert!(scopes.update_management(stale).await.is_err());
    assert!(
        scopes
            .browse(HubRequest::Management {
                grant: Some(grant.id)
            })
            .await
            .is_err()
    );
    // An acknowledged retry cannot reinstall the earlier authority after another edit.
    let mut binding = saved.view.profile.management.unwrap();
    binding.grants.clear();
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::now_v7(),
            expected: saved.view.profile.cursor,
            binding,
        })
        .await
        .unwrap();
    let current = profile.snapshot().await.unwrap();
    let FlowHubPage::Management(retried) = *scopes.update_management(change).await.unwrap() else {
        panic!("missing retry state")
    };
    assert_eq!(retried.view.profile.cursor, current.profile.cursor);
    assert!(retried.view.profile.management.unwrap().grants.is_empty());
    let after = worker.snapshot().await.unwrap();
    assert_eq!(before.cursor, after.cursor);
    assert!(worker.context().messages_full().is_empty());
    assert!(
        host.session(source.session_id.0)
            .unwrap()
            .scheduling_status(source.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    host.shutdown().await.unwrap();
}

fn expect_instance(content: atman_tui::FlowViewContent<'_>) -> &FlowView {
    match content {
        atman_tui::FlowViewContent::Instance(view)
        | atman_tui::FlowViewContent::Materialized { view, .. } => view,
        atman_tui::FlowViewContent::Legacy(_) | atman_tui::FlowViewContent::Initial { .. } => {
            panic!("expected an execution-backed view")
        }
    }
}

#[tokio::test]
async fn management_selection_is_atomic_retryable_and_does_not_schedule_sessions() {
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    use atman_runtime::flow_store::profile::ReplaceManagement;
    use atman_tui::flow_hub::ManagementSelection;
    let data = tempfile::tempdir().unwrap();
    let config = ConfigHub::from_config_dir(data.path().join("config"));
    std::fs::create_dir_all(config.config_dir()).unwrap();
    std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
    let host = RuntimeHost::default();
    let (notices, _received) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let root = RuntimeStore::open(data.path()).unwrap();
    let destination = Uuid::now_v7();
    let detached = root
        .create_session(SessionDescriptor {
            id: destination,
            project: None,
            title: "Existing session".into(),
            configuration: atman_runtime::flow_store::SessionConfiguration::Atman(config.source()),
        })
        .unwrap();
    let profile = scopes.profile().await.unwrap();
    let initial = profile.snapshot().await.unwrap();
    scopes
        .select_management(ManagementSelection {
            request_id: Uuid::now_v7(),
            expected: initial.profile.cursor,
            session_id: destination,
        })
        .await
        .unwrap();
    assert!(
        host.sessions().is_empty(),
        "Selecting a binding must not load its Session"
    );
    drop(detached);
    let source = scopes.open_management(config).await.unwrap();
    let worker = host.flow(&source).await.unwrap();
    let before = worker.snapshot().await.unwrap().cursor;
    let view = profile.snapshot().await.unwrap();
    let mut binding = view.profile.management.unwrap();
    binding.grants.push(FlowGrant {
        id: Uuid::now_v7(),
        grantee: source.clone(),
        target: AccessTarget::AllSessions,
        actions: [FlowAction::Discover].into(),
    });
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::now_v7(),
            expected: view.profile.cursor,
            binding,
        })
        .await
        .unwrap();
    let replacement = Uuid::now_v7();
    let other = root
        .create_session(SessionDescriptor {
            id: replacement,
            project: None,
            title: "Replacement".into(),
            configuration: Default::default(),
        })
        .unwrap();
    let request = ManagementSelection {
        request_id: Uuid::now_v7(),
        expected: profile.snapshot().await.unwrap().profile.cursor,
        session_id: replacement,
    };
    let missing = ManagementSelection {
        request_id: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
        ..request.clone()
    };
    assert!(scopes.select_management(missing).await.is_err());
    assert_eq!(
        profile.snapshot().await.unwrap().profile.cursor,
        request.expected
    );
    assert_eq!(
        profile
            .snapshot()
            .await
            .unwrap()
            .profile
            .management
            .unwrap()
            .grants
            .len(),
        1
    );
    scopes.select_management(request.clone()).await.unwrap();
    let saved = profile.snapshot().await.unwrap();
    let binding = saved.profile.management.unwrap();
    assert_eq!(binding.session.id, replacement);
    assert!(binding.grants.is_empty());
    let stale = ManagementSelection {
        request_id: Uuid::now_v7(),
        ..request.clone()
    };
    assert!(scopes.select_management(stale).await.is_err());
    scopes
        .select_management(ManagementSelection {
            request_id: Uuid::now_v7(),
            expected: saved.profile.cursor,
            session_id: destination,
        })
        .await
        .unwrap();
    let current = profile.snapshot().await.unwrap();
    scopes.select_management(request).await.unwrap();
    assert_eq!(
        profile.snapshot().await.unwrap().profile.cursor,
        current.profile.cursor
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    assert!(worker.context().messages_full().is_empty());
    assert!(host.session(replacement).is_err());
    assert!(
        host.session(source.session_id.0)
            .unwrap()
            .scheduling_status(source.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    drop(other);
    host.shutdown().await.unwrap();
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
async fn history_submission_attaches_the_continuation_and_replies_to_the_original_ticket() {
    use atman_runtime::{
        event::{Event, EventEnvelope},
        event_log::legacy::LegacyFlowKey,
        message::Message,
    };
    let data = tempfile::tempdir().unwrap();
    let config = ConfigHub::from_config_dir(data.path().join("config"));
    std::fs::create_dir_all(config.config_dir().join("commands")).unwrap();
    std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
    std::fs::write(config.config_dir().join("commands/echo.at"), "flow echo(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }").unwrap();
    std::fs::write(
        config.config_dir().join("routes.at"),
        "default_route { flow: echo }\n",
    )
    .unwrap();
    let session = Uuid::now_v7();
    let directory = data.path().join("sessions").join(session.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let turn = atman_rt::TurnId::now();
    let event = EventEnvelope::new(
        1,
        Event::UserMsg {
            turn_id: turn.clone(),
            flow_run_id: None,
            message: Message::user_text(turn, "past request"),
            presentation: None,
            injection_id: None,
        },
    );
    std::fs::write(
        directory.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        data.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let source = ViewAddress::Legacy {
        session,
        owner: LegacyFlowKey::Default,
    };
    let target = scopes
        .continue_with_config(source.clone(), Some(config))
        .await
        .unwrap();
    let (commands, mut controls) = mpsc::unbounded_channel();
    let forms = Arc::new(atman_runtime::session::FormRegistry::default());
    let Navigation::Focus { mut handle, .. } = scopes
        .focus(
            source.clone(),
            atman_tui::control::FlowPlacement::Floating,
            commands.clone(),
            forms.clone(),
        )
        .await
        .unwrap()
    else {
        panic!("missing historical view")
    };
    let ticket = handle
        .control_tx
        .as_ref()
        .unwrap()
        .send_tracked(TuiControl::Submit(TuiSubmission {
            text: "continued input".into(),
            images: vec![],
            presentation: None,
            reasoning: None,
        }))
        .unwrap()
        .unwrap();
    let control = controls.recv().await.unwrap();
    let TuiControl::Submit(submission) = control.command else {
        panic!("missing submission")
    };
    let Navigation::Continued {
        flow,
        handle: attached,
        service,
    } = scopes
        .continue_tui(source, ticket.request_id, submission, commands, forms)
        .await
        .unwrap()
    else {
        panic!("missing continuation")
    };
    assert_eq!(flow, ViewAddress::Instance(target.clone()));
    assert_eq!(service.binding.flow, flow);
    assert_eq!(attached.continued_session.as_ref(), Some(&target));
    assert!(service.execution.is_some());
    control
        .replies
        .send(TuiCommand::SubmissionContinued {
            flow,
            attachment: attached,
        })
        .unwrap();
    assert_eq!(
        handle
            .control_reply_rx
            .as_mut()
            .unwrap()
            .recv()
            .await
            .unwrap()
            .ticket,
        ticket
    );
    let worker = host.flow(&target).await.unwrap();
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if worker.context().messages_full().iter().any(|message| {
                message.is_visible_assistant_reply() && message.text_concat() == "continued input"
            }) {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let scope = &service.execution.as_ref().unwrap().scope;
    let mut descriptor = worker.descriptor().await.unwrap();
    descriptor.address.instance_id = atman_rt::FlowInstanceId::now();
    descriptor.creation_request = Uuid::now_v7();
    descriptor.created_by = Some(target.clone());
    let peer = scope.session.create_flow(descriptor).await.unwrap();
    scope
        .session
        .select_default(
            scope.session.snapshot().await.unwrap().cursor,
            peer.address().instance_id.clone(),
        )
        .await
        .unwrap();
    let peer_scope = scope.for_worker(&peer).await.unwrap();
    let (commands, _received) = mpsc::unbounded_channel();
    let (peer_handle, _, _) = peer_scope
        .prepare_tui_view(&peer, commands, &Arc::default())
        .await
        .unwrap();
    assert_eq!(peer_handle.continued_session, Some(target));
    host.shutdown().await.unwrap();
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
    session
        .select_default(&session.cursor().clone(), instance.clone())
        .unwrap();
    session
        .set_title(&session.cursor().clone(), Some("Renamed archive".into()))
        .unwrap();
    let config = ConfigHub::from_config_dir(dir.path().join("unused-config"));
    let project =
        atman_runtime::storage::ProjectContext::resolve(&config, dir.path(), dir.path()).unwrap();
    session
        .set_project(&session.cursor().clone(), Some(project))
        .unwrap();
    let before = std::fs::read(writer.dir().unwrap().join("events.jsonl")).unwrap();
    let host = RuntimeHost::default();
    let (notices, _rx) = mpsc::unbounded_channel();
    let scopes = Arc::new(scopes::Scopes::empty(
        dir.path(),
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
    ));
    let Prepared::Archive(archive) = prepare(&scopes, id).await.unwrap() else {
        panic!("archive startup acquired execution services")
    };
    assert_eq!(archive.view.transcript().flow().instance_id, instance);
    assert_eq!(archive.view.transcript().flow().session_id.0, id);
    assert_eq!(archive.title.as_deref(), Some("Renamed archive"));
    assert!(archive.project.is_none());
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
        assert_eq!(handle.session_name.as_deref(), Some("Renamed archive"));
        assert!(handle.project_root.is_none());
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
        std::fs::read(writer.dir().unwrap().join("events.jsonl")).unwrap(),
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
    let missing_project = missing_project.canonicalize().unwrap();
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
                name: None,
                display_label: None,
                scope: None,
                workspace: None,
                initial_goal: None,
                inherited_outputs: vec![],
                continued_from: None,
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
        session
            .select_default(&session.cursor().clone(), instance.clone())
            .unwrap();
        if index == 1 {
            use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
            let cursor = session.cursor().clone();
            session
                .grant_access(
                    &cursor,
                    FlowGrant {
                        id: Uuid::now_v7(),
                        grantee: FlowRef {
                            session_id: id.into(),
                            instance_id: instance.clone(),
                        },
                        target: AccessTarget::Flow(FlowRef {
                            session_id: ids[0].into(),
                            instance_id: instance.clone(),
                        }),
                        actions: [FlowAction::Watch].into_iter().collect(),
                    },
                )
                .unwrap();
        }
    }
    std::fs::remove_dir(&missing_project).unwrap();
    let host = RuntimeHost::with_services(HostServices {
        session_redactor: Some(Arc::new(session_redactor)),
        ..Default::default()
    });
    let (notices, _rx) = mpsc::unbounded_channel();
    let initial = Attachment::open_selected(
        dir.path(),
        ids[0],
        host.clone(),
        None,
        Arc::new(prompts::Prompts::new(notices)),
        None,
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
        None,
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
    let destination = scopes.resolve_default_session(ids[1]).await.unwrap();
    assert_eq!(
        destination,
        FlowRef {
            session_id: ids[1].into(),
            instance_id: instance.clone()
        }
        .into()
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
    let process_owner = FlowRef {
        session_id: ids[0].into(),
        instance_id: instance.clone(),
    };
    let before = host.session(ids[0]).unwrap().observe().await.unwrap();
    let directory = scopes
        .browse(HubRequest::ProcessSources {
            owner: process_owner.clone(),
        })
        .await
        .unwrap();
    assert!(
        matches!(*directory, FlowHubPage::ProcessSources { owner, sources } if owner == process_owner && sources.is_empty())
    );
    let after = host.session(ids[0]).unwrap().observe().await.unwrap();
    assert_eq!(before.snapshot.cursor, after.snapshot.cursor);
    assert_eq!(before.loaded.len(), after.loaded.len());
    let watched_owner = FlowRef {
        session_id: ids[1].into(),
        instance_id: instance.clone(),
    };
    for side in [
        atman_runtime::flow_store::inbox_view::InboxSide::Received,
        atman_runtime::flow_store::inbox_view::InboxSide::Sent,
    ] {
        let directory = scopes
            .browse(HubRequest::Inbox {
                owner: watched_owner.clone(),
                side,
                before: None,
            })
            .await
            .unwrap();
        let FlowHubPage::Inbox(page) = *directory else {
            panic!("wrong inbox page")
        };
        assert_eq!(page.owner, watched_owner);
        assert_eq!(page.side, side);
        match side {
            atman_runtime::flow_store::inbox_view::InboxSide::Received => {
                assert_eq!(page.items.len(), 1);
                assert!(
                    matches!(&page.items[0], atman_runtime::flow_store::inbox_view::InboxHistoryItem::Received(entry) if entry.acceptance.disposition == atman_runtime::flow_store::inbox::InboxDisposition::Pending)
                );
            }
            atman_runtime::flow_store::inbox_view::InboxSide::Sent => {
                assert!(page.items.is_empty())
            }
        }
        assert_eq!(
            host.sessions(),
            vec![ids[0]],
            "inbox inspection cannot attach an executor"
        );
    }
    let directory = scopes
        .browse(HubRequest::Watchers {
            owner: watched_owner.clone(),
        })
        .await
        .unwrap();
    assert!(
        matches!(*directory, FlowHubPage::Watchers(page) if page.owner == watched_owner && page.snapshot(&watched_owner).unwrap().records.is_empty())
    );
    assert_eq!(
        host.sessions(),
        vec![ids[0]],
        "watcher inspection cannot attach an executor"
    );
    let directory = scopes
        .browse(HubRequest::WatchCatalog {
            owner: watched_owner.clone(),
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(*directory, FlowHubPage::Watchers(page) if page.snapshot(&watched_owner).is_some())
    );
    assert_eq!(
        host.sessions(),
        vec![ids[0]],
        "global watcher discovery cannot attach execution"
    );
    let after_global = host.session(ids[0]).unwrap().observe().await.unwrap();
    assert_eq!(before.snapshot.cursor, after_global.snapshot.cursor);
    assert_eq!(before.loaded.len(), after_global.loaded.len());
    let directory = scopes
        .browse(HubRequest::Relations {
            session: ids[0],
            focus: Some(FlowRef {
                session_id: ids[0].into(),
                instance_id: instance.clone(),
            }),
            range: atman_tui::flow_hub::RelationRange::Catalog { before: None },
        })
        .await
        .unwrap();
    assert!(
        matches!(*directory, FlowHubPage::Relations { relations, progress: Some(progress), .. }
        if relations.len() == 1 && relations[0].source.session_id.0 == ids[1]
            && progress.scanned == 2 && progress.next.is_none() && progress.errors.is_empty())
    );
    assert_eq!(
        host.sessions(),
        vec![ids[0]],
        "catalog browsing must not attach execution"
    );
    // Background attachment must retain the same per-Session policy as later navigation.
    host.open_session(dir.path(), ids[1]).await.unwrap();
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
    let Navigation::Created {
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
    config
        .set_trust_config(&atman_runtime::trust::TrustConfig {
            mode: atman_runtime::trust::TrustMode::Reckless,
            ..Default::default()
        })
        .unwrap();
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
                contract { invocation { user_message: input } capabilities { shell: true } }
                when input == "terminal" {
                    term.spawn(cmd: "printf ready", cwd: TEST_CWD, rows: 5, cols: 40, wait_ms: 0)
                    return final.answer(message: "terminal ready")
                }
                when input == "ask" {
                    answer = form.ask(kind: "text", prompt: "Reply here")
                    return final.answer(message: answer.text)
                }
                return final.answer(message: input)
            }"#
                        .replace("TEST_CWD", &serde_json::to_string(dir.path()).unwrap()),
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
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal: None,
            inherited_outputs: vec![],
            continued_from: None,
            inherited_context: vec![],
            result_route: None,
            execution: Some(EntryBinding {
                user_routes: false,
                session_start: false,
                first_turn: atman_rt::TurnId::now(),
                first_run: atman_rt::RunId::now(),
                authority: EffectiveAuthority::root(&config.trust_config().unwrap(), true, None),
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
    executor
        .tools
        .register(Arc::new(atman_runtime::tools::term::TermSpawn));
    executor.tool_ctx.prompt_resolver = Some(Arc::new(
        atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
    ));
    let (notices, _rx) = mpsc::unbounded_channel();
    let attachment = Attachment {
        data_root: dir.path().to_owned(),
        host,
        session,
        project: descriptor.project.clone(),
        executors: Default::default(),
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
    // Queue configuration writes before observing any result; their durable order and IDs survive dispatch.
    {
        use atman_tui::settings::{SettingsMutation, SettingsMutationRequest};
        let (updates, mut results) = mpsc::unbounded_channel();
        let panels = crate::tui_panels::PanelServices::for_scope(
            worker.context(),
            attachment.executor.clone(),
            updates.clone(),
            vec![],
            attachment.config.clone(),
            None,
        )
        .unwrap();
        let queue = controls::Controls::start(
            attachment.clone(),
            worker.clone(),
            panels,
            forms.clone(),
            updates,
        );
        let mut requests = Vec::new();
        for action in [
            SettingsMutation::CreateAlias {
                name: "queued".into(),
                model: "first".into(),
            },
            SettingsMutation::UpdateAlias {
                original: Some("queued".into()),
                name: "queued".into(),
                model: "second".into(),
            },
            SettingsMutation::RemoveAlias {
                name: "queued".into(),
            },
        ] {
            let id = Uuid::now_v7();
            sender
                .send(TuiControl::MutateSettings(SettingsMutationRequest {
                    id,
                    action,
                }))
                .unwrap();
            let bound = commands_rx.recv().await.unwrap();
            queue
                .send(bound.ticket.request_id, bound.command, bound.replies)
                .unwrap();
            requests.push(id);
        }
        for expected in requests {
            let TuiCommand::SettingsMutationResult { request, result } =
                results.recv().await.unwrap()
            else {
                panic!("configuration receipt expected")
            };
            assert_eq!(request.id, expected);
            assert!(result.is_ok(), "{result:?}");
        }
        assert!(
            !attachment
                .config
                .models()
                .all_aliases()
                .iter()
                .any(|(name, _)| name == "queued")
        );
    }
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
    // Presentation commands keep their original view receipt and do not enter the flow inbox.
    let cursor = worker.snapshot().await.unwrap().cursor;
    for text in [":sidebar off", ":copy last-message", ":copy invalid"] {
        let ticket = sender
            .send_tracked(TuiControl::Submit(TuiSubmission {
                text: text.into(),
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
        let result = replies.recv().await.unwrap();
        assert_eq!(result.ticket, ticket);
        match text {
            ":sidebar off" => {
                assert!(matches!(
                    result.command,
                    TuiCommand::SetSidebar(atman_tui::sidebar::SidebarMode::Closed)
                ));
                let notice = replies.recv().await.unwrap();
                assert_eq!(notice.ticket, ticket);
                assert!(matches!(notice.command, TuiCommand::Toast { .. }));
            }
            ":copy last-message" => {
                assert!(matches!(result.command, TuiCommand::CopyText(text) if text == "seed"));
            }
            _ => {
                assert!(matches!(
                    result.command,
                    TuiCommand::SubmissionResult { result: Err(_) }
                ));
                continue;
            }
        }
        let ack = replies.recv().await.unwrap();
        assert_eq!(ack.ticket, ticket);
        assert!(matches!(
            ack.command,
            TuiCommand::SubmissionResult { result: Ok(()) }
        ));
    }
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
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
        view.snapshot().pending[0].envelope.request.request_id,
        ticket.request_id
    );
    let InboxPayload::UserSubmission { invocation, .. } =
        &view.snapshot().pending[0].envelope.request.payload
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
    let pending = expect_instance(source.refresh().await.unwrap())
        .snapshot()
        .pending[0]
        .clone();
    let edit = atman_runtime::flow_store::inbox::InboxEditRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(USER),
        message: pending.envelope.id,
        expected: pending.revision,
        change: atman_runtime::flow_store::inbox::InboxChange::Replace {
            content: atman_runtime::flow_store::inbox::UserContent {
                text: "edited queued input".into(),
                images: vec![],
                presentation: None,
            },
            delivery: atman_runtime::flow_store::inbox::DeliveryMode::NextTurn,
        },
    };
    let (floating_commands, mut floating_rx) = mpsc::unbounded_channel();
    let mut floating = TuiHandle::from_flow(
        &worker,
        ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: address.clone().into(),
        },
        floating_commands,
        attachment.config.clone(),
    )
    .await
    .unwrap();
    let mut receipt = None;
    // Both surfaces submit the same durable operation through their own reply mailbox.
    for is_floating in [false, true] {
        let (control, incoming, replies) = if is_floating {
            (
                floating.control_tx.as_ref().unwrap(),
                &mut floating_rx,
                floating.control_reply_rx.as_mut().unwrap(),
            )
        } else {
            (sender, &mut commands_rx, &mut *replies)
        };
        let ticket = control
            .send_tracked(TuiControl::EditInbox(Box::new(edit.clone())))
            .unwrap()
            .unwrap();
        let bound = incoming.recv().await.unwrap();
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
        let TuiCommand::InboxEdited { request_id, result } = reply.command else {
            panic!("missing edit receipt")
        };
        assert_eq!(request_id, edit.id);
        let committed = result.unwrap();
        if let Some(previous) = receipt.replace(committed.clone()) {
            assert_eq!(previous, committed);
        }
        let view = expect_instance(source.refresh().await.unwrap());
        assert_eq!(view.snapshot().pending[0].revision, committed);
        assert_eq!(view.snapshot().pending[0].acceptance, pending.acceptance);
        assert_eq!(worker.state().await.unwrap().pauses().len(), 1);
    }
    let mut forged = edit.clone();
    forged.sender = SenderIdentity::User(Uuid::new_v4());
    sender
        .send(TuiControl::EditInbox(Box::new(forged)))
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
        TuiCommand::InboxEdited { result: Err(_), .. }
    ));
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
        |item| matches!(item, atman_tui::app::OutputItem::AssistantMd { md, .. } if md == "edited queued input")
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
    #[cfg(unix)]
    {
        attachment
            .submit_tui(
                &worker,
                Uuid::now_v7(),
                TuiSubmission {
                    text: "terminal".into(),
                    images: vec![],
                    presentation: None,
                    reasoning: None,
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            attachment.host.wait_quiescent(),
        )
        .await
        .unwrap()
        .unwrap();
        let terminals = worker
            .resources()
            .terminals()
            .list(&worker.context().id().to_string());
        assert_eq!(terminals.len(), 1);
        let terminal = &terminals[0].0;
        let entry = worker.resources().terminals().get(terminal).unwrap();
        assert_eq!((entry.snapshot().rows, entry.snapshot().cols), (5, 40));
        let mut peer = worker.descriptor().await.unwrap();
        peer.address.instance_id = FlowInstanceId::now();
        peer.creation_request = Uuid::now_v7();
        let peer = attachment.session.create_flow(peer).await.unwrap();
        let cursor = worker.snapshot().await.unwrap().cursor;
        for (target, rows, cols, succeeds) in [
            (&peer, 9, 60, false),
            (&worker, 0, 60, false),
            (&worker, 9, 60, true),
        ] {
            sender
                .send(TuiControl::TermResize {
                    handle: terminal.clone(),
                    rows,
                    cols,
                })
                .unwrap();
            let bound = commands_rx.recv().await.unwrap();
            let result = attachment
                .tui_control(
                    target,
                    bound.ticket.request_id,
                    bound.command,
                    &bound.replies,
                    &forms,
                )
                .await;
            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            let expected = if succeeds { (9, 60) } else { (5, 40) };
            assert_eq!((entry.snapshot().rows, entry.snapshot().cols), expected);
        }
        assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    }
    // Cancelling an interactive wait leaves the conversation usable for a new user turn.
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
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !form_updates.borrow_and_update().is_empty() {
                break;
            }
            form_updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    sender.send(TuiControl::CancelFlow).unwrap();
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
    attachment
        .submit_tui(
            &worker,
            Uuid::now_v7(),
            TuiSubmission {
                text: "after cancellation".into(),
                images: vec![],
                presentation: None,
                reasoning: None,
            },
        )
        .await
        .unwrap();
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
    assert!(model.iter().any(|item| matches!(item, atman_tui::app::OutputItem::AssistantMd { md, .. } if md == "after cancellation")));
    // Recovery prepares scheduling while the interrupted gate still protects retained input.
    for text in ["ask", "after recovery"] {
        attachment
            .submit_tui(
                &worker,
                Uuid::now_v7(),
                TuiSubmission {
                    text: text.into(),
                    images: vec![],
                    reasoning: None,
                    presentation: None,
                },
            )
            .await
            .unwrap();
        if text == "ask" {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if !form_updates.borrow_and_update().is_empty() {
                        break;
                    }
                    form_updates.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
        }
    }
    attachment.command(&worker, ":cancel").await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        attachment.host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
    let interrupted = worker.state().await.unwrap();
    assert!(matches!(
        interrupted.state(),
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. })
    ));
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    attachment
        .session
        .stop_scheduling(address.instance_id.clone())
        .await
        .unwrap();
    let mut unavailable = attachment.clone();
    unavailable.executor = Executor::new();
    assert!(unavailable.command(&worker, ":recover").await.is_err());
    assert_eq!(
        worker.state().await.unwrap(),
        interrupted,
        "failed scheduling must retain the interruption"
    );
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    attachment.command(&worker, ":recover").await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        attachment.host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        worker
            .context()
            .messages_full()
            .iter()
            .filter(
                |message| message.origin == atman_runtime::message::MessageOrigin::FinalAnswer
                    && message.text_concat() == "after recovery"
            )
            .count(),
        1
    );
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
