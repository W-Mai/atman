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
        let mut pending_suggestions = std::collections::HashMap::<String, (String, String)>::new();
        while let Some(control) = control_rx.recv().await {
            match control {
                TuiControl::MetaCommand(command) => {
                    if handle_meta_command(
                        &control_session,
                        &command_tx,
                        &control_note_tx,
                        &command,
                        &mut pending_suggestions,
                    )
                    .await
                    {
                        if let Some(tx) = shutdown_tx.take() {
                            let _ = tx.send(());
                        }
                        break;
                    }
                }
                TuiControl::Domain(atman_tui::TuiDomainCommand::FormSubmit {
                    form_id,
                    submission,
                }) if form_id.starts_with("suggest_flow:") => {
                    let proposal = pending_suggestions.remove(&form_id);
                    let accepted = matches!(
                        &submission,
                        atman_runtime::form::FormSubmission::Submitted { answers }
                            if matches!(
                                answers.first(),
                                Some(atman_runtime::form::FormAnswer::Confirmed { value: true })
                            )
                    );
                    if accepted {
                        if let Some((flow_name, source)) = proposal {
                            match control_session
                                .install_suggested_flow(flow_name, source)
                                .await
                            {
                                Ok(response) => note(
                                    &control_note_tx,
                                    TuiNote::Info(format!(
                                        "installed suggested flow `{}`",
                                        response.flow_name
                                    )),
                                ),
                                Err(error) => note(
                                    &control_note_tx,
                                    TuiNote::Error(format!(
                                        "could not install suggested flow: {error}"
                                    )),
                                ),
                            }
                        } else {
                            note(
                                &control_note_tx,
                                TuiNote::Warn("suggestion proposal is no longer available".into()),
                            );
                        }
                    } else {
                        note(
                            &control_note_tx,
                            TuiNote::Info("suggested flow discarded".into()),
                        );
                    }
                    let _ = command_tx.send(TuiCommand::CloseSuggestionForm(form_id));
                }
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

async fn handle_meta_command(
    session: &SessionClient,
    command_tx: &mpsc::UnboundedSender<TuiCommand>,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
    pending_suggestions: &mut std::collections::HashMap<String, (String, String)>,
) -> bool {
    let Some(meta) = atman_runtime::meta_commands::match_command(command) else {
        note(
            note_tx,
            TuiNote::Error(format!("unknown `:{command}` — try `:help`")),
        );
        return false;
    };
    match meta.name {
        "exit" => return true,
        "help" => {
            for line in atman_runtime::meta_commands::help_lines() {
                note(note_tx, TuiNote::Info(line.into()));
            }
        }
        "session" => note(
            note_tx,
            TuiNote::Info(format!("session_id: {}", session.session_id())),
        ),
        "sessions" => {
            let _ = command_tx.send(TuiCommand::OpenSessionSwitcher);
        }
        "mode" => {
            let _ = command_tx.send(TuiCommand::OpenTrustModePicker);
        }
        "mode-theme" => {
            let _ = command_tx.send(TuiCommand::OpenThemePicker);
        }
        "model" => {
            let _ = command_tx.send(TuiCommand::OpenModelPicker);
        }
        "sidebar" => handle_sidebar(command_tx, note_tx, command),
        "rename" => handle_rename(session, command_tx, note_tx, command).await,
        "compact" => match session.compact().await {
            Ok(_) => note(note_tx, TuiNote::Info("compaction requested".into())),
            Err(error) => note(
                note_tx,
                TuiNote::Error(format!("could not request compaction: {error}")),
            ),
        },
        "attach" => handle_attach(command_tx, note_tx, command),
        "copy" => handle_copy(session, note_tx, command),
        "goal" => handle_goal(session, note_tx, command).await,
        "todo" => handle_todo(session, note_tx, command).await,
        "cost" => {
            let state = session.current();
            let usage = &state.projection().usage;
            note(
                note_tx,
                TuiNote::Info(format!(
                    "total llm_calls: {} · input: {} · output: {}",
                    usage.llm_calls, usage.input_tokens, usage.output_tokens
                )),
            );
        }
        "suggest" => match session.suggest_flow().await {
            Ok(response) => match response.result {
                atman_proto::SuggestFlowStatus::NoSuggestion => note(
                    note_tx,
                    TuiNote::Info("no reusable pattern found in recent turns".into()),
                ),
                atman_proto::SuggestFlowStatus::Invalid { reason } => note(
                    note_tx,
                    TuiNote::Warn(format!("suggestion was rejected: {reason}")),
                ),
                atman_proto::SuggestFlowStatus::Proposal {
                    flow_name,
                    source,
                    has_shell,
                } => {
                    note(
                        note_tx,
                        TuiNote::Info(format!("suggested flow `{flow_name}`:\n{source}")),
                    );
                    if has_shell {
                        note(
                            note_tx,
                            TuiNote::Warn(
                                "the suggested flow executes shell tools; review it before accepting"
                                    .into(),
                            ),
                        );
                    }
                    let form_id = format!("suggest_flow:{}", uuid::Uuid::now_v7());
                    pending_suggestions.insert(form_id.clone(), (flow_name.clone(), source));
                    let kind = atman_runtime::form::FormKind::Confirm {
                        prompt: format!("install suggested flow `{flow_name}`?"),
                    };
                    let _ = command_tx.send(TuiCommand::OpenSuggestionForm(
                        atman_runtime::form::PendingForm {
                            form_id,
                            run_id: atman_runtime::event::FlowRunId::now(),
                            tool_use_id: "suggest_flow".into(),
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
            },
            Err(error) => note(
                note_tx,
                TuiNote::Error(format!("could not generate flow suggestion: {error}")),
            ),
        },
        _ => {}
    }
    false
}

fn note(note_tx: &mpsc::UnboundedSender<TuiNote>, note: TuiNote) {
    let _ = note_tx.send(note);
}

fn handle_sidebar(
    command_tx: &mpsc::UnboundedSender<TuiCommand>,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
) {
    let arg = command.strip_prefix("sidebar").unwrap_or("").trim();
    let mode = match arg {
        "on" | "open" => atman_tui::sidebar::SidebarMode::Open,
        "off" | "close" | "closed" => atman_tui::sidebar::SidebarMode::Closed,
        _ => {
            note(note_tx, TuiNote::Warn(":sidebar on | off".into()));
            return;
        }
    };
    let _ = command_tx.send(TuiCommand::SetSidebar(mode));
}

async fn handle_rename(
    session: &SessionClient,
    command_tx: &mpsc::UnboundedSender<TuiCommand>,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
) {
    let arg = command.strip_prefix("rename").unwrap_or("").trim();
    if arg.is_empty() {
        note(
            note_tx,
            TuiNote::Info(format!(
                "session title: {}",
                session.current().projection().metadata.title
            )),
        );
        return;
    }
    let result = if arg == "clear" {
        session.clear_title().await
    } else {
        session.rename(arg).await
    };
    match result {
        Ok(response) => {
            let _ = command_tx.send(TuiCommand::SessionNameUpdated(response.session.title));
        }
        Err(error) => note(
            note_tx,
            TuiNote::Error(format!("could not rename session: {error}")),
        ),
    }
}

fn handle_attach(
    command_tx: &mpsc::UnboundedSender<TuiCommand>,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
) {
    let arg = command.strip_prefix("attach").unwrap_or("").trim();
    match arg {
        "" => note(
            note_tx,
            TuiNote::Warn(":attach <path> | :attach clear | :attach list".into()),
        ),
        "clear" => {
            let _ = command_tx.send(TuiCommand::ClearDraftAttachments);
        }
        "list" => {
            let _ = command_tx.send(TuiCommand::ListDraftAttachments);
        }
        path => match atman_runtime::attachment_store::AttachmentStore::at("").import_path(path) {
            Ok(source) => {
                let _ = command_tx.send(TuiCommand::AddDraftAttachment(source));
            }
            Err(error) => note(note_tx, TuiNote::Error(format!(":attach: {error}"))),
        },
    }
}

fn handle_copy(session: &SessionClient, note_tx: &mpsc::UnboundedSender<TuiNote>, command: &str) {
    use atman_proto::{MessagePart, MessageRole, TranscriptItem};

    let target = command.strip_prefix("copy").unwrap_or("").trim();
    let target = if target.is_empty() {
        "last-message"
    } else {
        target
    };
    let assistant = match target {
        "last-message" | "last" => true,
        "last-tool" => false,
        _ => {
            note(
                note_tx,
                TuiNote::Warn(format!(":copy: unknown target `{target}`")),
            );
            return;
        }
    };
    let payload = session
        .current()
        .projection()
        .transcript
        .iter()
        .rev()
        .find_map(|item| {
            let TranscriptItem::Message { message, .. } = item else {
                return None;
            };
            if assistant && message.role != MessageRole::Assistant {
                return None;
            }
            message.parts.iter().rev().find_map(|part| match part {
                MessagePart::Text { text } if assistant => Some(text.clone()),
                MessagePart::ToolResult { content, .. } if !assistant => Some(content.clone()),
                _ => None,
            })
        });
    let Some(payload) = payload else {
        note(
            note_tx,
            TuiNote::Info(format!(":copy: nothing to copy for {target}")),
        );
        return;
    };
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let _ = std::io::stderr().write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes());
    let _ = std::io::stderr().flush();
    note(
        note_tx,
        TuiNote::Info(format!(
            ":copy: pushed {} chars to clipboard",
            payload.chars().count()
        )),
    );
}

async fn handle_goal(
    session: &SessionClient,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
) {
    let arg = command.strip_prefix("goal").unwrap_or("").trim();
    if arg.is_empty() {
        let message = session
            .current()
            .projection()
            .goal
            .as_deref()
            .map(|goal| format!("goal: {goal}"))
            .unwrap_or_else(|| "no session goal set".into());
        note(note_tx, TuiNote::Info(message));
        return;
    }
    let goal = (arg != "clear").then(|| arg.to_owned());
    match session.set_goal(goal).await {
        Ok(_) => note(note_tx, TuiNote::Info("goal updated".into())),
        Err(error) => note(
            note_tx,
            TuiNote::Error(format!("could not update goal: {error}")),
        ),
    }
}

async fn handle_todo(
    session: &SessionClient,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
    command: &str,
) {
    let arg = command.strip_prefix("todo").unwrap_or("").trim();
    if matches!(arg, "" | "list") {
        let state = session.current();
        let todos = &state.projection().todos;
        if todos.is_empty() {
            note(note_tx, TuiNote::Info("no todos yet".into()));
        } else {
            for (index, todo) in todos.iter().enumerate() {
                let state = match todo.state {
                    atman_proto::TodoState::Pending => "pending",
                    atman_proto::TodoState::InProgress => "in progress",
                    atman_proto::TodoState::Done => "done",
                    atman_proto::TodoState::Cancelled => "cancelled",
                };
                note(
                    note_tx,
                    TuiNote::Info(format!("{index:>2} · {state} · {}", todo.where_)),
                );
            }
        }
        return;
    }
    let mutation = if arg == "clear" {
        atman_proto::TodoMutation::Clear
    } else if let Some(id) = arg.strip_prefix("done ") {
        let Some(id) = resolve_todo_id(session, id.trim(), note_tx) else {
            return;
        };
        atman_proto::TodoMutation::SetState {
            id,
            state: atman_proto::TodoState::Done,
        }
    } else if let Some(id) = arg.strip_prefix("cancel ") {
        let Some(id) = resolve_todo_id(session, id.trim(), note_tx) else {
            return;
        };
        atman_proto::TodoMutation::SetState {
            id,
            state: atman_proto::TodoState::Cancelled,
        }
    } else {
        note(
            note_tx,
            TuiNote::Warn(":todo list | done <id> | cancel <id> | clear".into()),
        );
        return;
    };
    match session.update_todos(mutation).await {
        Ok(_) => note(note_tx, TuiNote::Info("todo updated".into())),
        Err(error) => note(
            note_tx,
            TuiNote::Error(format!("could not update todo: {error}")),
        ),
    }
}

fn resolve_todo_id(
    session: &SessionClient,
    value: &str,
    note_tx: &mpsc::UnboundedSender<TuiNote>,
) -> Option<String> {
    if uuid::Uuid::parse_str(value).is_ok() {
        return Some(value.to_owned());
    }
    let Ok(index) = value.parse::<usize>() else {
        note(note_tx, TuiNote::Warn(format!("invalid todo id `{value}`")));
        return None;
    };
    match session.current().projection().todos.get(index) {
        Some(todo) => Some(todo.id.clone()),
        None => {
            note(
                note_tx,
                TuiNote::Warn(format!("todo index {index} out of range")),
            );
            None
        }
    }
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
