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
        show_startup: bool,
        transcript_bookmark: Option<atman_tui::app::TranscriptBookmark>,
    },
}

const RETAINED_SESSION_LIMIT: usize = 8;

#[derive(Default)]
struct RetainedSessions {
    entries: std::collections::HashMap<atman_proto::SessionId, SessionClient>,
    recency: std::collections::VecDeque<atman_proto::SessionId>,
}

impl RetainedSessions {
    fn get(&mut self, session_id: &atman_proto::SessionId) -> Option<SessionClient> {
        let session = self.entries.get(session_id).cloned()?;
        self.touch(session_id);
        Some(session)
    }

    fn insert(&mut self, session: SessionClient) {
        let session_id = session.session_id().clone();
        self.entries.insert(session_id.clone(), session);
        self.touch(&session_id);
        while self.recency.len() > RETAINED_SESSION_LIMIT {
            if let Some(evicted) = self.recency.pop_front() {
                self.entries.remove(&evicted);
            }
        }
    }

    fn touch(&mut self, session_id: &atman_proto::SessionId) {
        self.recency.retain(|known| known != session_id);
        self.recency.push_back(session_id.clone());
    }
}

pub(crate) async fn run(resume: Option<String>) -> Result<()> {
    crate::load_model_config_from_disk();
    let mut onboarding_recommended = atman_runtime::model_registry::is_first_run();
    let client = connect_local_daemon_as("atman-tui").await?;
    crate::load_model_config_from_disk();
    let project_root = std::env::current_dir()?.to_string_lossy().into_owned();
    let show_startup = resume.is_none();
    let first = match resume {
        Some(prefix) => {
            let session_id = resolve_session_prefix(&client, &prefix).await?;
            client.attach_session_windowed(session_id).await?
        }
        None => {
            client
                .create_session(Some(project_root.clone()), None)
                .await?
        }
    };

    let _terminal_guard = atman_tui::terminal_guard::TerminalGuard::install()?;
    let _sink_guard = atman_runtime::notify::ScopedSink::tui();
    let sessions = std::sync::Arc::new(tokio::sync::Mutex::new(RetainedSessions::default()));
    sessions.lock().await.insert(first.clone());
    let mut bookmarks = std::collections::HashMap::<
        atman_proto::SessionId,
        atman_tui::app::TranscriptBookmark,
    >::new();
    let mut current = NextSession::Attached {
        session: first,
        intro: None,
        show_startup,
        transcript_bookmark: None,
    };
    loop {
        let NextSession::Attached {
            session,
            intro,
            show_startup,
            transcript_bookmark,
        } = current;
        let session_id = session.session_id().clone();
        let (next, bookmark) = run_session(
            client.clone(),
            session,
            intro,
            show_startup,
            onboarding_recommended,
            transcript_bookmark.or_else(|| bookmarks.get(&session_id).copied()),
            sessions.clone(),
        )
        .await?;
        if let Some(bookmark) = bookmark {
            bookmarks.insert(session_id, bookmark);
        }
        let Some(next) = next else {
            return Ok(());
        };
        onboarding_recommended = false;
        current = next;
    }
}

async fn run_session(
    client: Client,
    session: SessionClient,
    intro: Option<atman_tui::app::StartupIntro>,
    show_startup: bool,
    onboarding_recommended: bool,
    transcript_bookmark: Option<atman_tui::app::TranscriptBookmark>,
    sessions: std::sync::Arc<tokio::sync::Mutex<RetainedSessions>>,
) -> Result<(
    Option<NextSession>,
    Option<atman_tui::app::TranscriptBookmark>,
)> {
    let startup_card = if show_startup {
        Some(atman_tui::app::OutputItem::StartupCard {
            version: env!("CARGO_PKG_VERSION").into(),
            recent: build_startup_recent(&client, session.session_id()).await,
        })
    } else {
        None
    };
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
    let control_sessions = sessions.clone();
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
                        Ok(session_id) => {
                            let cached = control_sessions.lock().await.get(&session_id);
                            match cached {
                                Some(session) => Ok(session),
                                None => match control_client
                                    .attach_session_windowed(session_id.clone())
                                    .await
                                {
                                    Ok(session) => {
                                        control_sessions.lock().await.insert(session.clone());
                                        Ok(session)
                                    }
                                    Err(error) => Err(anyhow::Error::from(error)),
                                },
                            }
                        }
                        Err(error) => Err(error),
                    };
                    match attached {
                        Ok(session) => {
                            let _ = next_tx.send(NextSession::Attached {
                                session,
                                intro: Some(intro),
                                show_startup: false,
                                transcript_bookmark: None,
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
                            control_sessions.lock().await.insert(session.clone());
                            let _ = next_tx.send(NextSession::Attached {
                                session,
                                intro: None,
                                show_startup: true,
                                transcript_bookmark: None,
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
                TuiControl::OnboardingInit => match control_client.initialize_config(None).await {
                    Ok(_) => {
                        crate::load_model_config_from_disk();
                    }
                    Err(error) => {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not initialize configuration: {error}"
                        )));
                    }
                },
                TuiControl::MutateProvider(request) => {
                    let result = provider_mutation_to_proto(request.action.clone())
                        .map_err(|error| error.to_string());
                    let result = match result {
                        Ok(mutation) => control_client
                            .mutate_provider(mutation)
                            .await
                            .map(provider_mutation_from_proto)
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error),
                    };
                    if result.is_ok() {
                        crate::load_model_config_from_disk();
                    }
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
                    let result = control_client
                        .upsert_model_config(atman_proto::UpsertModelConfigRequest {
                            request_id: None,
                            old_name,
                            name: name.clone(),
                            model,
                            provider,
                            context_budget,
                            reasoning: reasoning.to_string(),
                            max_tokens,
                            enabled,
                        })
                        .await;
                    match result {
                        Ok(_) => {
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
                    let result = control_client
                        .switch_default_model(model.clone())
                        .await
                        .map(|response| response.model)
                        .map_err(|error| error.to_string());
                    if result.is_ok() {
                        crate::load_model_config_from_disk();
                    }
                    let _ = command_tx.send(TuiCommand::ModelSwitchResult {
                        request_id,
                        model,
                        result,
                    });
                }
                TuiControl::TestProvider { name, entry } => {
                    let result = control_client
                        .probe_provider(atman_proto::ProbeProviderRequest {
                            name,
                            kind: entry.kind,
                            api_key: entry.api_key,
                            api_key_env: entry.api_key_env,
                            base_url: entry.base_url,
                            max_tokens: entry.max_tokens,
                            reasoning_format: entry
                                .reasoning_format
                                .map(|format| format.to_string()),
                            prompt_cache_key: entry.prompt_cache_key,
                            enabled: entry.enabled.unwrap_or(true),
                        })
                        .await
                        .map(|response| (response.message, response.ok))
                        .unwrap_or_else(|error| (error.to_string(), false));
                    let _ = command_tx.send(TuiCommand::ProviderTestResult(result));
                }
                TuiControl::McpTest { name } => {
                    let (message, ok) = control_client
                        .probe_mcp(name.clone())
                        .await
                        .map(|response| (response.message, response.ok))
                        .unwrap_or_else(|error| (error.to_string(), false));
                    let _ = command_tx.send(TuiCommand::McpTestResult { name, message, ok });
                }
                TuiControl::McpListResources { name } => {
                    let resources = match control_client.list_mcp_resources(name.clone()).await {
                        Ok(response) => response
                            .resources
                            .into_iter()
                            .map(|resource| atman_runtime::mcp::McpResource {
                                uri: resource.uri,
                                name: resource.name,
                                description: resource.description,
                                mime_type: resource.mime_type,
                            })
                            .collect(),
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                            Vec::new()
                        }
                    };
                    let _ = command_tx.send(TuiCommand::McpResourcesResult { name, resources });
                }
                TuiControl::McpListPrompts { name } => {
                    let prompts = match control_client.list_mcp_prompts(name.clone()).await {
                        Ok(response) => response
                            .prompts
                            .into_iter()
                            .map(|prompt| atman_runtime::mcp::McpPrompt {
                                name: prompt.name,
                                description: prompt.description,
                                arguments: prompt
                                    .arguments
                                    .into_iter()
                                    .map(|argument| atman_runtime::mcp::McpPromptArg {
                                        name: argument.name,
                                        description: argument.description,
                                        required: argument.required,
                                    })
                                    .collect(),
                            })
                            .collect(),
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                            Vec::new()
                        }
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
                TuiControl::LoadOlderHistory => {
                    if let Err(error) = control_session.load_older_history().await {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not load older session history: {error}"
                        )));
                    }
                }
                TuiControl::LoadNewerHistory => {
                    if let Err(error) = control_session.load_newer_history().await {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not load newer session history: {error}"
                        )));
                    }
                }
                TuiControl::SearchHistory {
                    query,
                    project_wide,
                } => {
                    let result = control_session
                        .search_history(query.clone(), project_wide, Some(50))
                        .await
                        .map(|response| {
                            response
                                .hits
                                .into_iter()
                                .map(|hit| atman_tui::history_search_modal::HistoryHit {
                                    session_id: hit.session_id,
                                    seq: hit.seq,
                                    ts: hit.ts,
                                    kind: hit.kind,
                                    snippet: hit.snippet,
                                })
                                .collect()
                        })
                        .map_err(|error| format!("could not search session history: {error}"));
                    let _ = command_tx.send(TuiCommand::HistorySearchResult { query, result });
                }
                TuiControl::JumpToHistory { session_id, seq } => {
                    let parsed = uuid::Uuid::parse_str(&session_id)
                        .map(atman_proto::SessionId)
                        .context("invalid history search session id");
                    match parsed {
                        Ok(session_id) if session_id == *control_session.session_id() => {
                            if let Err(error) = control_session.load_history_around(seq).await {
                                let _ = control_note_tx.send(TuiNote::Error(format!(
                                    "could not load session history: {error}"
                                )));
                            }
                        }
                        Ok(session_id) => {
                            let cached = control_sessions.lock().await.get(&session_id);
                            let attached = match cached {
                                Some(session) => Ok(session),
                                None => match control_client
                                    .attach_session_windowed(session_id.clone())
                                    .await
                                {
                                    Ok(session) => {
                                        control_sessions.lock().await.insert(session.clone());
                                        Ok(session)
                                    }
                                    Err(error) => Err(anyhow::Error::from(error)),
                                },
                            };
                            match attached {
                                Ok(session) => match session.load_history_around(seq).await {
                                    Ok(()) => {
                                        let _ = next_tx.send(NextSession::Attached {
                                            session,
                                            intro: None,
                                            show_startup: false,
                                            transcript_bookmark: Some(
                                                atman_tui::app::TranscriptBookmark::at_sequence(
                                                    seq,
                                                ),
                                            ),
                                        });
                                        if let Some(tx) = shutdown_tx.take() {
                                            let _ = tx.send(());
                                        }
                                        break;
                                    }
                                    Err(error) => {
                                        let _ = control_note_tx.send(TuiNote::Error(format!(
                                            "could not load session history: {error}"
                                        )));
                                    }
                                },
                                Err(error) => {
                                    let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                                }
                            }
                        }
                        Err(error) => {
                            let _ = control_note_tx.send(TuiNote::Error(error.to_string()));
                        }
                    }
                }
                TuiControl::LoadToolDetail { tool_use_id } => {
                    if let Err(error) = control_session.load_tool_detail(&tool_use_id).await {
                        let _ = control_note_tx.send(TuiNote::Error(format!(
                            "could not load tool output detail: {error}"
                        )));
                    }
                }
                _ => {
                    let _ = control_note_tx.send(TuiNote::Warn(
                        "this control is not available through the daemon yet".into(),
                    ));
                }
            }
        }
    });

    let mut handle = TuiHandle::from_daemon(&session);
    if let Some(startup_card) = startup_card {
        handle.initial_items.push(startup_card);
    }
    handle.control_tx = Some(control_tx);
    handle.cmd_rx = Some(command_rx);
    handle.note_rx = Some(note_rx);
    handle.shutdown_rx = Some(shutdown_rx);
    handle.flow_names = crate::discover_flow_names();
    handle.startup_intro = intro;
    handle.onboarding_recommended = onboarding_recommended;
    handle.initial_transcript_bookmark = transcript_bookmark;
    let (bookmark_tx, bookmark_rx) = tokio::sync::oneshot::channel();
    handle.transcript_bookmark_tx = Some(bookmark_tx);
    let result = atman_tui::run_tui(handle).await;
    let bookmark = bookmark_rx.await.ok();

    sync_task.abort();
    control_task.await.context("join daemon TUI control task")?;
    result?;
    Ok((next_rx.try_recv().ok(), bookmark))
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

fn provider_mutation_to_proto(
    mutation: atman_tui::ProviderMutation,
) -> Result<atman_proto::ProviderMutation> {
    Ok(match mutation {
        atman_tui::ProviderMutation::Login { kind, name } => atman_proto::ProviderMutation::Login {
            kind: match kind {
                atman_runtime::auth_store::ProviderKind::Codex => atman_proto::ProviderKind::Codex,
                atman_runtime::auth_store::ProviderKind::AnthropicOauth => {
                    atman_proto::ProviderKind::AnthropicOauth
                }
                atman_runtime::auth_store::ProviderKind::GitHubCopilot => {
                    atman_proto::ProviderKind::GitHubCopilot
                }
                atman_runtime::auth_store::ProviderKind::Custom => {
                    atman_proto::ProviderKind::Custom
                }
            },
            name,
        },
        atman_tui::ProviderMutation::SetEnabled {
            provider_id,
            enabled,
        } => atman_proto::ProviderMutation::SetEnabled {
            provider_id,
            enabled,
        },
        atman_tui::ProviderMutation::Remove { provider_id } => {
            atman_proto::ProviderMutation::Remove { provider_id }
        }
        atman_tui::ProviderMutation::Refresh { provider_id } => {
            atman_proto::ProviderMutation::Refresh { provider_id }
        }
        atman_tui::ProviderMutation::UpsertConfig {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            reasoning_format,
            enabled,
            create,
        } => atman_proto::ProviderMutation::UpsertConfig {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            reasoning_format,
            enabled,
            create,
        },
        _ => bail!("provider mutation is not supported by this client"),
    })
}

fn provider_mutation_from_proto(
    result: atman_proto::ProviderMutationResult,
) -> atman_tui::ProviderMutationSuccess {
    match result {
        atman_proto::ProviderMutationResult::Installed {
            provider_id,
            name,
            kind,
            delta,
        } => atman_tui::ProviderMutationSuccess::Installed {
            provider_id,
            name,
            kind: match kind {
                atman_proto::ProviderKind::Codex => atman_runtime::auth_store::ProviderKind::Codex,
                atman_proto::ProviderKind::AnthropicOauth => {
                    atman_runtime::auth_store::ProviderKind::AnthropicOauth
                }
                atman_proto::ProviderKind::GitHubCopilot => {
                    atman_runtime::auth_store::ProviderKind::GitHubCopilot
                }
                atman_proto::ProviderKind::Custom => {
                    atman_runtime::auth_store::ProviderKind::Custom
                }
            },
            delta: runtime_catalog_delta(delta),
        },
        atman_proto::ProviderMutationResult::StateChanged {
            provider_id,
            enabled,
            change,
            catalog,
        } => atman_tui::ProviderMutationSuccess::StateChanged {
            provider_id,
            enabled,
            change: atman_runtime::provider_lifecycle::ProviderStateChange {
                auth_changed: change.auth_changed,
                live_changed: change.live_changed,
                catalog_changed: change.catalog_changed,
            },
            catalog: catalog.map(runtime_catalog_delta),
        },
        atman_proto::ProviderMutationResult::Refreshed { provider_id, delta } => {
            atman_tui::ProviderMutationSuccess::Refreshed {
                provider_id,
                delta: runtime_catalog_delta(delta),
            }
        }
        atman_proto::ProviderMutationResult::ConfigSaved { name, created } => {
            atman_tui::ProviderMutationSuccess::ConfigSaved { name, created }
        }
    }
}

fn runtime_catalog_delta(
    delta: atman_proto::CatalogDelta,
) -> atman_runtime::model_registry::CatalogDelta {
    atman_runtime::model_registry::CatalogDelta {
        added: delta.added,
        updated: delta.updated,
        removed: delta.removed,
        total: delta.total,
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

async fn build_startup_recent(
    client: &Client,
    current: &atman_proto::SessionId,
) -> Vec<atman_tui::app::StartupSessionEntry> {
    let Ok(summaries) = client.list_sessions(None, None, Some(6)).await else {
        return Vec::new();
    };
    let now = chrono::Utc::now();
    summaries
        .into_iter()
        .filter(|summary| &summary.id != current)
        .take(5)
        .map(|summary| {
            let age_secs = summary
                .updated_at
                .and_then(|updated_at| (now - updated_at).to_std().ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let session_id = summary.id.to_string();
            let short_id = session_id.chars().take(8).collect();
            let project = summary.project_root.as_deref().and_then(|project_root| {
                std::path::Path::new(project_root)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
            atman_tui::app::StartupSessionEntry {
                session_id,
                short_id,
                goal: summary.goal,
                project,
                age_label: format_age(age_secs),
                event_count: u64::try_from(summary.event_count).unwrap_or(u64::MAX),
            }
        })
        .collect()
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
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
