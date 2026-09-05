use std::path::{Path, PathBuf};

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
    let client = connect_local_daemon().await?;
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
        let Some(next) = run_session(client.clone(), session, intro).await? else {
            return Ok(());
        };
        current = next;
    }
}

async fn run_session(
    client: Client,
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
                TuiControl::OnboardingInit => {
                    if let Ok(config_dir) = atman_runtime::storage::config_dir() {
                        let _ = crate::init::init_config_dir_with_mode(&config_dir, None);
                        crate::load_model_config_from_disk();
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

async fn resolve_session_prefix(client: &Client, prefix: &str) -> Result<atman_proto::SessionId> {
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

async fn connect_local_daemon() -> Result<Client> {
    let socket_path = atman_runtime::storage::data_dir()?
        .join("run")
        .join("atman.sock");
    if daemon_pid()?.is_none() {
        spawn_daemon()?;
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let error = match Client::connect(
            UnixTransport::new(&socket_path),
            ClientIdentity::new("atman-tui", env!("CARGO_PKG_VERSION")),
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

fn spawn_daemon() -> Result<()> {
    let binary = daemon_binary();
    std::process::Command::new(&binary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;
    Ok(())
}

fn daemon_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("atman-daemon")))
        .filter(|path| path.exists())
        .unwrap_or_else(|| Path::new("atman-daemon").to_path_buf())
}
