use super::*;
use atman_runtime::storage::ProjectContext;
use atman_tui::control::FlowPlacement;

#[tokio::test]
async fn moved_session_views_keep_registered_project_services_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "").unwrap();
    let config = ConfigHub::from_config_dir(&config_dir);
    let alpha = dir.path().join("alpha");
    let beta = dir.path().join("beta");
    for (path, label) in [(&alpha, "alpha"), (&beta, "beta")] {
        std::fs::create_dir_all(path.join(".atman/lib")).unwrap();
        std::fs::create_dir_all(path.join(".atman/commands")).unwrap();
        std::fs::write(
            path.join(".atman/commands/label.at"),
            format!(
                "flow label() -> string {{ return final.answer(message: \"{label}-command\") }}"
            ),
        )
        .unwrap();
        std::fs::write(path.join(format!("{label}.png")), b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(path.join("AGENTS.md"), format!("# scope-rule\n{label}")).unwrap();
        std::fs::write(
            path.join(".atman/lib/helper.at"),
            format!("pub flow tag() -> string {{ return \"{label}\" }}"),
        )
        .unwrap();
    }
    let entry = config_dir.join("entry.at");
    std::fs::write(
        &entry,
        r#"
        use "project:helper.at"::tag
        flow main(input: string) -> string {
            contract { invocation { user_message: input } }
            label = tag().await
            rule = rule.fetch(name: "scope-rule")
            return final.answer(message: label + "|" + rule + "|" + input)
        }
    "#,
    )
    .unwrap();
    let id = Uuid::now_v7();
    let runtime = RuntimeStore::open(dir.path()).unwrap();
    let mut session = runtime
        .create_session(SessionDescriptor {
            id,
            project: Some(ProjectContext::resolve(&config, &alpha, dir.path()).unwrap()),
            configuration: SessionConfiguration::Atman(config.source()),
            title: "Project scopes".into(),
        })
        .unwrap();
    let mut bindings = Vec::new();
    for (index, root) in [&alpha, &beta, &beta].into_iter().enumerate() {
        if index == 2 {
            std::fs::write(
                config_dir.join("config.toml"),
                "[storage]\nscope = 'local'\n",
            )
            .unwrap();
        }
        let project = ProjectContext::resolve(&config, root, dir.path()).unwrap();
        session
            .set_project(&session.cursor().clone(), Some(project.clone()))
            .unwrap();
        let program = atman_runtime::source_program::load_program(
            &entry,
            &session
                .projection()
                .descriptor
                .source_roots_for(Some(&project)),
        )
        .unwrap();
        let address = FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let request = Uuid::now_v7();
        let mut flow = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: request,
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: entry.clone(),
                    bundle: program.revision_bundle(),
                }
                .into(),
            })
            .unwrap();
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
                    authority: EffectiveAuthority::root(
                        &Default::default(),
                        false,
                        Some(project.root().to_owned()),
                    ),
                }),
                request: atman_runtime::flow_store::launch::EntryRequest {
                    request_id: request,
                    sender: SenderIdentity::User(USER),
                    recipient: address.clone(),
                    arguments: [("input".into(), "seed".into())].into(),
                    invocation: BTreeMap::new(),
                    start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
                },
            },
            None,
            |_| Ok(()),
        )
        .unwrap();
        bindings.push((address, project));
    }
    session
        .select_default(&session.cursor().clone(), bindings[2].0.instance_id.clone())
        .unwrap();
    drop(session);
    // Source bundles, rather than mutable source files, restore all existing entries.
    std::fs::remove_file(&entry).unwrap();
    for root in [&alpha, &beta] {
        std::fs::remove_file(root.join(".atman/lib/helper.at")).unwrap();
    }
    for round in 0..2 {
        let host = RuntimeHost::default();
        let (notices, _rx) = mpsc::unbounded_channel();
        let scopes = Arc::new(scopes::Scopes::empty(
            dir.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        let initial = scopes.open(id).await.unwrap();
        assert_eq!(initial.project.as_ref(), Some(&bindings[2].1));
        assert_ne!(initial.project, initial.descriptor.project);
        let (commands, _rx) = mpsc::unbounded_channel();
        let forms = Arc::new(atman_runtime::session::FormRegistry::default());
        for (index, (address, project)) in bindings.iter().enumerate() {
            for placement in [FlowPlacement::Main, FlowPlacement::Floating] {
                let Navigation::Focus {
                    handle, service, ..
                } = scopes
                    .focus(
                        address.clone().into(),
                        placement,
                        commands.clone(),
                        forms.clone(),
                    )
                    .await
                    .unwrap()
                else {
                    panic!("missing flow view")
                };
                assert_eq!(handle.project_root.as_deref(), project.root().to_str());
                let execution = service.execution.as_ref().unwrap();
                assert_eq!(execution.scope.project.as_ref(), Some(project));
                assert_eq!(execution.scope.executor.tool_ctx.project(), Some(project));
                let scope = initial.for_worker(&execution.worker).await.unwrap();
                assert_eq!(scope.project.as_ref(), Some(project));
                // Selecting a different project must preserve the current view's prompt resolver.
                let switched = execution
                    .scope
                    .for_project(Some(bindings[(index + 1) % 3].1.clone()))
                    .await
                    .unwrap();
                assert!(Arc::ptr_eq(
                    switched.executor.tool_ctx.prompt_resolver.as_ref().unwrap(),
                    execution
                        .scope
                        .executor
                        .tool_ctx
                        .prompt_resolver
                        .as_ref()
                        .unwrap(),
                ));
                scope
                    .submit_tui(
                        &execution.worker,
                        Uuid::now_v7(),
                        TuiSubmission {
                            text: format!("round-{round}"),
                            images: vec![],
                            presentation: None,
                            reasoning: None,
                        },
                    )
                    .await
                    .unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
                    .await
                    .unwrap()
                    .unwrap();
                let label = if index == 0 { "alpha" } else { "beta" };
                let relative = std::path::PathBuf::from(format!("{label}.png"));
                assert_eq!(
                    scope
                        .import_image(&execution.worker, None, &relative)
                        .unwrap(),
                    execution
                        .worker
                        .context()
                        .import_image_path(project.root().join(relative))
                        .unwrap(),
                );
                let messages = execution.worker.context().messages_full();
                assert_eq!(
                    messages.last().unwrap().text_concat(),
                    format!("{label}|# scope-rule\n{label}|round-{round}")
                );
                scope
                    .submit_tui(
                        &execution.worker,
                        Uuid::now_v7(),
                        TuiSubmission {
                            text: "/label".into(),
                            images: vec![],
                            presentation: None,
                            reasoning: None,
                        },
                    )
                    .await
                    .unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(10), host.wait_quiescent())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    execution
                        .worker
                        .context()
                        .messages_full()
                        .last()
                        .unwrap()
                        .text_concat(),
                    format!("{label}-command"),
                );
            }
        }
        assert_eq!(initial.executors.lock().unwrap().len(), 3);
        // Changing the default only changes future selection, not existing scopes.
        let catalog = initial.session.snapshot().await.unwrap();
        initial
            .session
            .select_default(catalog.cursor, bindings[0].0.instance_id.clone())
            .await
            .unwrap();
        assert_eq!(
            scopes.open(id).await.unwrap().project.as_ref(),
            Some(&bindings[0].1)
        );
        assert_eq!(initial.project.as_ref(), Some(&bindings[2].1));
        let catalog = initial.session.snapshot().await.unwrap();
        initial
            .session
            .select_default(catalog.cursor, bindings[2].0.instance_id.clone())
            .await
            .unwrap();
        host.shutdown().await.unwrap();
        drop(initial);
        drop(scopes);
    }
}
