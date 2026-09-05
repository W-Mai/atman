use anyhow::{Context, Result, bail};
use atman_client::{Client, ClientIdentity, SessionClient, UnixTransport};
use atman_tui::daemon_adapter::{DaemonCommandOutcome, DaemonTuiAdapter};
use atman_tui::session_switcher::SessionScope;
use atman_tui::{SessionPickerRow, TuiCommand, TuiControl, TuiHandle, TuiNote};
use tokio::sync::mpsc;

enum NextSession {
    Attached {
        session: SessionClient,
        intro: Option<atman_tui::app::StartupIntro>,
    },
}

pub(crate) async fn run(resume: Option<String>) -> Result<()> {
    crate::load_model_config_from_disk();
    let provider_lifecycle = settings_provider_lifecycle().await?;
    let client = connect_local_daemon_as("atman-tui").await?;
    let project_root = std::env::current_dir()?.to_string_lossy().into_owned();
    let first = match resume {
        Some(prefix) => {
            let session_id = resolve_session_prefix(&client, &prefix).await?;
            client.attach_session(session_id).await?
        }
        None => {
            client
                .create_session(Some(project_root.clone()), None)
                .await?
        }
    };

    let _terminal_guard = atman_tui::terminal_guard::TerminalGuard::install()?;
    let _sink_guard = atman_runtime::notify::ScopedSink::tui();
    let mut current = NextSession::Attached {
        session: first,
        intro: None,
    };
    loop {
        let NextSession::Attached { session, intro } = current;
        let Some(next) =
            run_session(client.clone(), provider_lifecycle.clone(), session, intro).await?
        else {
            return Ok(());
        };
        current = next;
    }
}

async fn run_session(
    client: Client,
    provider_lifecycle: atman_runtime::ProviderLifecycle,
    session: SessionClient,
    intro: Option<atman_tui::app::StartupIntro>,
) -> Result<Option<NextSession>> {
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (note_tx, note_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (next_tx, mut next_rx) = mpsc::unbounded_channel();

    let sync_session = session.clone();
    let sync_note_tx = note_tx.clone();
    let sync_task = tokio::spawn(async move {
        if let Err(error) = sync_session.synchronize().await {
            let _ = sync_note_tx.send(TuiNote::Error(format!(
                "daemon session synchronization stopped: {error}"
            )));
        }
    });

    let adapter = DaemonTuiAdapter::new(client.clone(), session.clone());
    let control_client = client.clone();
    let control_session = session.clone();
    let control_note_tx = note_tx.clone();
    let control_task = tokio::spawn(async move {
        let mut shutdown_tx = Some(shutdown_tx);
        while let Some(control) = control_rx.recv().await {
            match control {
                TuiControl::Domain(atman_tui::TuiDomainCommand::FormSubmit {
                    form_id,
                    submission,
                }) if form_id.starts_with("session_move_path:") => {
                    if matches!(submission, atman_runtime::form::FormSubmission::Rejected) {
                        let _ = command_tx.send(TuiCommand::CloseSessionMoveForm(form_id));
                        continue;
                    }
                    let result = submitted_text(&submission)
                        .ok_or_else(|| anyhow::anyhow!("a working directory is required"))
                        .and_then(|path| {
                            if path.trim().is_empty() {
                                Err(anyhow::anyhow!("a working directory is required"))
                            } else {
                                Ok(path.to_owned())
                            }
                        });
                    match result {
                        Ok(path) => match control_session.move_to(path).await {
                            Ok(response) => {
                                let _ = command_tx.send(TuiCommand::CloseSessionMoveForm(form_id));
                                let _ = control_note_tx.send(TuiNote::Info(format!(
                                    "session moved to {}",
                                    response.session.project_root.unwrap_or_default()
                                )));
                            }
                            Err(error) => {
                                let _ = control_note_tx.send(TuiNote::Error(format!(
                                    "could not move session: {error}"
                                )));
                            }
                        },
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                        }
                    }
                }
                TuiControl::Domain(command) => match adapter.dispatch(command).await {
                    Ok(DaemonCommandOutcome::Applied) => {}
                    Ok(DaemonCommandOutcome::SessionRenamed {
                        session_id,
                        title: Some(title),
                    }) if session_id == *control_session.session_id() => {
                        let _ = command_tx.send(TuiCommand::SessionNameUpdated(title));
                    }
                    Ok(DaemonCommandOutcome::SessionRenamed { .. })
                    | Ok(DaemonCommandOutcome::SessionDelete(_)) => {}
                    Err(error) => {
                        let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                    }
                },
                TuiControl::ListSessions { scope } => {
                    let project = match scope {
                        SessionScope::Project => control_session
                            .current()
                            .projection()
                            .metadata
                            .project_root
                            .clone(),
                        SessionScope::All => None,
                    };
                    match control_client.list_sessions(project, None, Some(200)).await {
                        Ok(summaries) => {
                            let rows = summaries
                                .into_iter()
                                .map(|summary| session_row(&summary, control_session.session_id()))
                                .collect();
                            let _ = command_tx.send(TuiCommand::SessionListUpdated { scope, rows });
                        }
                        Err(error) => {
                            let _ = control_note_tx
                                .send(TuiNote::Error(format!("could not list sessions: {error}")));
                        }
                    }
                }
                TuiControl::SwitchSession { sid, intro } => {
                    let attached = match resolve_session_prefix(&control_client, &sid).await {
                        Ok(session_id) => control_client
                            .attach_session(session_id)
                            .await
                            .map_err(anyhow::Error::from),
                        Err(error) => Err(error),
                    };
                    match attached {
                        Ok(session) => {
                            let _ = next_tx.send(NextSession::Attached {
                                session,
                                intro: Some(intro),
                            });
                            if let Some(tx) = shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                            break;
                        }
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                        }
                    }
                }
                TuiControl::NewSession => {
                    let project_root = control_session
                        .current()
                        .projection()
                        .metadata
                        .project_root
                        .clone();
                    match control_client.create_session(project_root, None).await {
                        Ok(session) => {
                            let _ = next_tx.send(NextSession::Attached {
                                session,
                                intro: None,
                            });
                            if let Some(tx) = shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                            break;
                        }
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                        }
                    }
                }
                TuiControl::AutoNameSession => match control_session.auto_name().await {
                    Ok(response) => {
                        let _ =
                            command_tx.send(TuiCommand::SessionNameUpdated(response.session.title));
                        let _ =
                            control_note_tx.send(TuiNote::Info("session name generated".into()));
                    }
                    Err(error) => {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not generate session name: {error}"
                        )));
                    }
                },
                TuiControl::MoveSession => {
                    let kind = atman_runtime::form::FormKind::Text {
                        prompt: "New working directory:".into(),
                        placeholder: Some("/path/to/project".into()),
                        multiline: false,
                    };
                    let form_id = format!("session_move_path:{}", uuid::Uuid::now_v7());
                    let _ = command_tx.send(TuiCommand::OpenSessionMoveForm(
                        atman_runtime::form::PendingForm {
                            form_id,
                            run_id: atman_runtime::event::FlowRunId::now(),
                            tool_use_id: "session_move_path".into(),
                            kind: kind.clone(),
                            form: atman_runtime::form::CompositeForm {
                                questions: vec![atman_runtime::form::FormQuestion {
                                    id: "question".into(),
                                    kind,
                                }],
                            },
                            emitted_at: chrono::Utc::now(),
                        },
                    ));
                }
                TuiControl::OnboardingInit => {
                    if let Ok(config_dir) = atman_runtime::storage::config_dir() {
                        let _ = crate::init::init_config_dir_with_mode(&config_dir, None);
                        crate::load_model_config_from_disk();
                    }
                }
                TuiControl::MutateProvider(request) => {
                    let result = crate::execute_provider_mutation(
                        &provider_lifecycle,
                        request.action.clone(),
                    )
                    .await
                    .map_err(|error| format!("{error:#}"));
                    let _ = command_tx.send(TuiCommand::ProviderMutationResult { request, result });
                }
                TuiControl::UpsertConfigModel {
                    old_name,
                    name,
                    model,
                    provider,
                    context_budget,
                    reasoning,
                    max_tokens,
                    enabled,
                } => {
                    let result = atman_runtime::config_hub::ConfigHub::global().and_then(|hub| {
                        hub.upsert_model(atman_runtime::model_registry::ModelConfigUpdate {
                            old_name: old_name.as_deref(),
                            name: &name,
                            model: &model,
                            provider: provider.as_deref(),
                            context_budget,
                            reasoning,
                            capabilities: None,
                            image_detail: None,
                            max_tokens,
                            enabled,
                        })
                    });
                    match result {
                        Ok(()) => {
                            crate::load_model_config_from_disk();
                            let _ = command_tx.send(TuiCommand::ProviderCatalogChanged {
                                added_provider: None,
                            });
                        }
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(format!(
                                "Model \"{name}\" save failed: {error}"
                            )));
                        }
                    }
                }
                TuiControl::OpenAliasManager { .. } => {}
                TuiControl::SwitchModel { request_id, model } => {
                    let result = switch_model(&provider_lifecycle, &model);
                    let _ = command_tx.send(TuiCommand::ModelSwitchResult {
                        request_id,
                        model,
                        result,
                    });
                }
                TuiControl::TestProvider { name, entry } => {
                    let result = crate::test_provider_endpoint(&name, &entry).await;
                    let _ = command_tx.send(TuiCommand::ProviderTestResult(result));
                }
                TuiControl::McpTest { name } => {
                    let (message, ok) = test_mcp(&name).await;
                    let _ = command_tx.send(TuiCommand::McpTestResult { name, message, ok });
                }
                TuiControl::McpListResources { name } => {
                    let resources = match connect_mcp(&name).await {
                        Ok(client) => client.list_resources().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };
                    let _ = command_tx.send(TuiCommand::McpResourcesResult { name, resources });
                }
                TuiControl::McpListPrompts { name } => {
                    let prompts = match connect_mcp(&name).await {
                        Ok(client) => client.list_prompts().await.unwrap_or_default(),
                        Err(_) => Vec::new(),
                    };
                    let _ = command_tx.send(TuiCommand::McpPromptsResult { name, prompts });
                }
                TuiControl::McpReload => match control_session.reload_mcp().await {
                    Ok(response) => {
                        let _ = command_tx.send(TuiCommand::McpReloaded {
                            active_runs: Some(response.active_runs),
                        });
                    }
                    Err(error) => {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not reload MCP servers: {error}"
                        )));
                    }
                },
                _ => {
                    let _ = control_note_tx.send(TuiNote::Warn(
                        "this control is not available through the daemon yet".into(),
                    ));
                }
            }
        }
    });

    let mut handle = TuiHandle::from_daemon(&session);
    handle.control_tx = Some(control_tx);
    handle.cmd_rx = Some(command_rx);
    handle.note_rx = Some(note_rx);
    handle.shutdown_rx = Some(shutdown_rx);
    handle.flow_names = crate::discover_flow_names();
    handle.startup_intro = intro;
    handle.onboarding_recommended = atman_runtime::model_registry::is_first_run();
    let result = atman_tui::run_tui(handle).await;

    sync_task.abort();
    control_task.await.context("join daemon TUI control task")?;
    result?;
    Ok(next_rx.try_recv().ok())
}

fn submitted_text(submission: &atman_runtime::form::FormSubmission) -> Option<&str> {
    let atman_runtime::form::FormSubmission::Submitted { answers } = submission else {
        return None;
    };
    let atman_runtime::form::FormAnswer::TextEntered { text } = answers.first()? else {
        return None;
    };
    Some(text)
}

async fn settings_provider_lifecycle() -> Result<atman_runtime::ProviderLifecycle> {
    let lifecycle = atman_runtime::ProviderLifecycle::new(
        atman_runtime::config_hub::ConfigHub::global()?,
        atman_runtime::provider::ProviderRegistry::new(),
    );
    lifecycle.reload_config_providers()?;
    atman_daemon::bootstrap::prepare_auth_provider_runtime(&lifecycle).await?;
    Ok(lifecycle)
}

fn switch_model(
    provider_lifecycle: &atman_runtime::ProviderLifecycle,
    requested_model: &str,
) -> Result<String, String> {
    let info = atman_runtime::model_registry::model_info(requested_model);
    if info.context_budget == 0 {
        return Err("model or provider is disabled".into());
    }
    let active_model = info.name;
    if provider_lifecycle
        .provider_registry()
        .resolve(&active_model)
        .is_none()
    {
        return Err("provider is not available in this process".into());
    }
    atman_runtime::config_hub::ConfigHub::global()
        .map_err(|error| error.to_string())?
        .update_alias(Some("smart"), "smart", &active_model)
        .map_err(|error| error.to_string())?;
    crate::load_model_config_from_disk();
    Ok(active_model)
}

async fn test_mcp(name: &str) -> (String, bool) {
    let Some(config) = crate::load_mcp_configs()
        .into_iter()
        .find(|config| config.name == name)
    else {
        return ("not found in config".into(), false);
    };
    let registry = atman_runtime::ToolRegistry::new();
    let mut results = atman_runtime::mcp::register_from_configs(&registry, &[config]).await;
    match results.pop() {
        Some(Ok(status)) => (format!("{} tools discovered", status.tool_count), true),
        Some(Err(error)) => (error.error.to_string(), false),
        None => ("MCP probe returned no result".into(), false),
    }
}

async fn connect_mcp(name: &str) -> Result<atman_runtime::mcp::McpClient> {
    let config = crate::load_mcp_configs()
        .into_iter()
        .find(|config| config.name == name)
        .with_context(|| format!("MCP server `{name}` is not configured"))?;
    match config.transport {
        atman_runtime::mcp::TransportKind::Stdio => atman_runtime::mcp::McpClient::connect_stdio(
            &config.name,
            &config.command,
            &config.args,
            &config.env,
            config.timeout_ms,
        )
        .await
        .map_err(anyhow::Error::from),
        _ => {
            let url = config
                .url
                .as_deref()
                .with_context(|| format!("MCP server `{name}` requires a URL"))?;
            atman_runtime::mcp::McpClient::connect_http(
                &config.name,
                url,
                config.auth_token,
                config.timeout_ms,
            )
            .await
            .map_err(anyhow::Error::from)
        }
    }
}

fn session_row(
    summary: &atman_proto::SessionSummary,
    current: &atman_proto::SessionId,
) -> SessionPickerRow {
    SessionPickerRow {
        id: summary.id.to_string(),
        is_current: &summary.id == current,
        name: (summary.title != "Untitled session").then(|| summary.title.clone()),
        project: summary.project_root.clone(),
        message_count: summary.message_count,
        updated_at: summary
            .updated_at
            .map(|timestamp| timestamp.to_rfc3339())
            .unwrap_or_default(),
        goal: summary.goal.clone(),
    }
}

pub(crate) async fn resolve_session_prefix(
    client: &Client,
    prefix: &str,
) -> Result<atman_proto::SessionId> {
    if let Ok(id) = uuid::Uuid::parse_str(prefix) {
        return Ok(atman_proto::SessionId(id));
    }
    let matches = client
        .list_sessions(None, None, None)
        .await?
        .into_iter()
        .filter(|summary| summary.id.to_string().starts_with(prefix))
        .map(|summary| summary.id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session_id] => Ok(session_id.clone()),
        [] => bail!("no session found matching prefix `{prefix}`"),
        _ => bail!(
            "ambiguous session prefix `{prefix}` matches {} sessions",
            matches.len()
        ),
    }
}

pub(crate) async fn connect_local_daemon() -> Result<Client> {
    connect_local_daemon_as("atman-cli").await
}

async fn connect_local_daemon_as(client_name: &str) -> Result<Client> {
    let socket_path = atman_runtime::storage::data_dir()?
        .join("run")
        .join("atman.sock");
    if daemon_pid()?.is_none() {
        crate::spawn_daemon_process()?;
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let error = match Client::connect(
            UnixTransport::new(&socket_path),
            ClientIdentity::new(client_name, env!("CARGO_PKG_VERSION")),
        )
        .await
        {
            Ok(client) => return Ok(client),
            Err(error) => error,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::Error::from(error))
                .with_context(|| format!("connect to daemon socket {}", socket_path.display()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn daemon_pid() -> Result<Option<u32>> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    Ok(atman_daemon::pidfile::read_pid(&pid_path)?
        .filter(|pid| atman_daemon::pidfile::is_alive(*pid)))
}
