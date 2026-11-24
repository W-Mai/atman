use std::sync::Arc;

use anyhow::{Context, Result};
#[cfg(unix)]
use atman_daemon::unix::UnixServer;
use atman_daemon::{
    DaemonState,
    config::default_config_path,
    host_directory::{HostPublication, HostRecord, HostRegistration, HostRole},
    http::{HttpState, router},
    pidfile,
    remote::RemoteAccess,
};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--push-relay")) {
        let path = std::env::args_os()
            .nth(2)
            .context("usage: atman-daemon --push-relay CONFIG")?;
        anyhow::ensure!(
            std::env::args_os().nth(3).is_none(),
            "usage: atman-daemon --push-relay CONFIG"
        );
        return atman_daemon::remote::push_relay::serve(std::path::Path::new(&path)).await;
    }
    if std::env::args().nth(1).as_deref() == Some("--preview-serve") {
        let port = std::env::args()
            .nth(2)
            .and_then(|value| value.parse().ok())
            .unwrap_or(65097);
        return atman_daemon::preview_server::serve(port).await;
    }
    atman_runtime::notify::install(Arc::new(atman_runtime::notify::CliSink));
    let data_dir = default_data_dir()?;
    let pid_path = pidfile::default_pid_path()?;
    let _pid_lease = pidfile::PidLease::acquire(&pid_path)?;
    let config_path = default_config_path()?;
    let config_dir = atman_daemon::bootstrap::default_config_dir()?;
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config_dir)
        .with_daemon_config_path(&config_path);
    migrate_legacy_layout_for_daemon(&hub, &data_dir)?;
    hub.migrate_and_reload_models()?;
    let launcher = std::sync::Arc::new(atman_daemon::run::RunLauncher::new(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        atman_daemon::bootstrap::default_config_dir().ok(),
        atman_runtime::platform::user_home_dir(),
    )?);

    let daemon_generation = uuid::Uuid::now_v7().to_string();
    let state = Arc::new(DaemonState::new_with_generation(
        data_dir.clone(),
        daemon_generation,
    ));
    let reconciled =
        atman_daemon::run::reconcile_workspaces(&launcher.project_root, state.daemon_generation())?;
    if !reconciled.is_empty() {
        eprintln!(
            "[atman-daemon] marked {} stale workspace lease(s) orphaned",
            reconciled.len()
        );
    }
    launcher.start_provider_catalog_refreshes(&state).await?;
    state.set_launcher(launcher);

    let config = hub.load_or_init_daemon_config()?;
    let remote = RemoteAccess::prepare(state.clone(), &config_path).await?;
    state.configure_host_routing(config.auth_token.clone())?;
    state.prepare_peer_transport().await?;
    println!(
        "[atman-daemon] config loaded from {} (token 32-byte, keep it secret)",
        config_path.display()
    );

    #[cfg(unix)]
    let unix_server = {
        let sock_path = default_socket_path()?;
        let server = UnixServer::bind(&sock_path).await?;
        println!(
            "[atman-daemon] unix socket listening at {}",
            server.path().display()
        );
        server
    };

    let http_state = Arc::new(HttpState {
        daemon: state.clone(),
        auth_token: config.auth_token.clone(),
    });
    let port = std::env::var("ATMAN_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(65099u16);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    state.register_peer_endpoint(listener.local_addr()?)?;
    println!("[atman-daemon] http listening on http://{addr}");

    let host = state.runtime_host();
    let record = HostRecord {
        generation: uuid::Uuid::parse_str(state.daemon_generation())?,
        process_id: std::process::id(),
        role: HostRole::Daemon,
        presentation: atman_proto::hosts::HostPresentation::Headless,
        endpoint: listener.local_addr()?,
        sessions: host.sessions(),
    };
    let registration_data = data_dir.clone();
    let registration = Arc::new(
        tokio::task::spawn_blocking(move || HostRegistration::register(&registration_data, record))
            .await??,
    );
    let publication = HostPublication::start(registration.clone(), host);
    let shutdown = state.shutdown_signal();
    let http_shutdown = shutdown.clone();
    let mut app = router(http_state);
    if let Some(remote) = &remote {
        app = app.merge(remote.operator_routes(config.auth_token));
    }
    let http_task = tokio::spawn(atman_daemon::http::serve(listener, app, http_shutdown));
    if let Err(error) = state.ensure_profile_port().await {
        state.request_shutdown();
        let _ = http_task.await;
        publication.shutdown().await?;
        state.shutdown().await?;
        return Err(error.into());
    }
    let remote_task = remote.map(|remote| {
        println!(
            "[atman-daemon] device HTTPS listening for {}",
            remote.origin()
        );
        tokio::spawn(remote.serve(shutdown.clone()))
    });
    let maintenance = atman_daemon::host_maintenance::HostMaintenance::start(&state);
    #[cfg(unix)]
    let unix_task = {
        let sh = shutdown.clone();
        let st = state.clone();
        Some(tokio::spawn(async move { unix_server.serve(st, sh).await }))
    };
    #[cfg(not(unix))]
    let unix_task: Option<tokio::task::JoinHandle<Result<()>>> = None;

    let ctrlc_shutdown = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        ctrlc_shutdown.cancel();
    });

    let cleanup = {
        let state = state.clone();
        let signal = shutdown.clone();
        tokio::spawn(async move {
            signal.cancelled().await;
            state.shutdown().await
        })
    };

    let http_result = http_task.await;
    state.request_shutdown();
    maintenance.shutdown().await?;
    let cleanup_result = cleanup.await;
    publication.shutdown().await?;
    tokio::task::spawn_blocking(move || drop(registration)).await?;
    http_result??;
    cleanup_result??;
    if let Some(unix_task) = unix_task {
        unix_task.await??;
    }
    if let Some(remote_task) = remote_task {
        remote_task.await??;
    }
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

fn default_data_dir() -> Result<std::path::PathBuf> {
    atman_runtime::storage::data_dir()
}

#[cfg(unix)]
fn default_socket_path() -> Result<std::path::PathBuf> {
    Ok(default_data_dir()?.join("run").join("atman.sock"))
}
