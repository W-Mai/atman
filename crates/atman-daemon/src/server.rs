use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::{
    DaemonState,
    config::default_config_path,
    http::{HttpState, router},
    pidfile,
    unix::UnixServer,
};

const SESSION_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const SESSION_IDLE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const DAEMON_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub async fn serve() -> Result<()> {
    let data_dir = atman_runtime::storage::data_dir()?;
    let config_path = default_config_path()?;
    let config_dir = crate::bootstrap::default_config_dir()?;
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config_dir)
        .with_daemon_config_path(&config_path);
    migrate_legacy_layout_for_daemon(&hub, &data_dir)?;
    hub.migrate_and_reload_models()?;
    let launcher = Arc::new(crate::run::RunLauncher::new(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        crate::bootstrap::default_config_dir().ok(),
        std::env::var("HOME").ok().map(std::path::PathBuf::from),
    )?);

    let pid_path = pidfile::default_pid_path()?;
    if let Some(existing) = pidfile::read_pid(&pid_path)?
        && pidfile::is_alive(existing)
    {
        anyhow::bail!(
            "another atman-daemon already running (pid={existing}, {})",
            pid_path.display()
        );
    }
    pidfile::write_pid(&pid_path, std::process::id())?;

    let daemon_generation = uuid::Uuid::now_v7().to_string();
    let state = Arc::new(DaemonState::new_with_generation(
        data_dir.clone(),
        daemon_generation,
    ));
    let reconciled =
        crate::run::reconcile_workspaces(&launcher.project_root, state.daemon_generation())?;
    if !reconciled.is_empty() {
        eprintln!(
            "[atman-daemon] marked {} stale workspace lease(s) orphaned",
            reconciled.len()
        );
    }
    launcher.start_provider_catalog_refreshes(&state).await?;
    state.set_launcher(launcher);

    let config = hub.load_or_init_daemon_config()?;
    println!(
        "[atman-daemon] config loaded from {} (token 32-byte, keep it secret)",
        config_path.display()
    );

    let socket_path = data_dir.join("run").join("atman.sock");
    let unix_server = UnixServer::bind(&socket_path).await?;
    println!(
        "[atman-daemon] unix socket listening at {}",
        unix_server.path().display()
    );

    let http_state = Arc::new(HttpState {
        daemon: state.clone(),
        auth_token: config.auth_token,
    });
    let port = std::env::var("ATMAN_DAEMON_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(65099u16);
    let address = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("[atman-daemon] http listening on http://{address}");

    let shutdown = CancellationToken::new();
    let idle_eviction_task = tokio::spawn(state.clone().run_idle_eviction(
        SESSION_IDLE_TIMEOUT,
        SESSION_IDLE_SWEEP_INTERVAL,
        shutdown.clone(),
    ));
    let unix_task = {
        let shutdown = shutdown.clone();
        let state = state.clone();
        tokio::spawn(async move { unix_server.serve(state, shutdown).await })
    };

    let signal_shutdown = shutdown.clone();
    let state_for_shutdown = state.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
        state_for_shutdown.begin_shutdown();
        signal_shutdown.cancel();
    });

    axum::serve(listener, router(http_state))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    let _ = unix_task.await;
    let _ = idle_eviction_task.await;
    let drain = state.shutdown(DAEMON_DRAIN_TIMEOUT).await;
    if drain.forced != 0 || drain.remaining != 0 {
        eprintln!(
            "[atman-daemon] shutdown drained {} session(s), forced {}, remaining {}",
            drain.graceful, drain.forced, drain.remaining
        );
    }
    pidfile::remove_pid(&pid_path);
    Ok(())
}

fn migrate_legacy_layout_for_daemon(
    hub: &atman_runtime::config_hub::ConfigHub,
    data_dir: &std::path::Path,
) -> Result<()> {
    let report = hub.migrate_legacy_layout(data_dir)?;
    let Some(report) = report else {
        return Ok(());
    };
    for item in report.artifacts {
        let fatal = item.sensitive
            && matches!(
                item.kind,
                atman_runtime::config_migration::ArtifactOutcomeKind::RejectedFileType
                    | atman_runtime::config_migration::ArtifactOutcomeKind::FailedBeforePublish
            );
        if fatal {
            anyhow::bail!(
                "legacy sensitive config {} could not be migrated: {:?}{}",
                item.path,
                item.kind,
                item.error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            );
        }
        if matches!(
            item.kind,
            atman_runtime::config_migration::ArtifactOutcomeKind::CommittedSourceRetained
                | atman_runtime::config_migration::ArtifactOutcomeKind::RejectedFileType
                | atman_runtime::config_migration::ArtifactOutcomeKind::FailedBeforePublish
        ) {
            eprintln!(
                "[atman-daemon] legacy config {}: {:?}",
                item.path, item.kind
            );
        }
    }
    Ok(())
}
