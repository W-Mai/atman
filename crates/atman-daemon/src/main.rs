use std::sync::Arc;

use anyhow::{Context, Result};
use atman_daemon::{
    DaemonState,
    config::{DaemonConfig, default_config_path},
    http::{HttpState, router},
    pidfile,
    unix::UnixServer,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let sessions_dir = default_sessions_dir()?;
    let state = Arc::new(DaemonState::new(sessions_dir));

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

    let config_path = default_config_path()?;
    let config = DaemonConfig::load_or_init(&config_path)?;
    println!(
        "[atman-daemon] config loaded from {} (token 32-byte, keep it secret)",
        config_path.display()
    );

    let sock_path = default_socket_path()?;
    let unix_server = UnixServer::bind(&sock_path).await?;
    println!(
        "[atman-daemon] unix socket listening at {}",
        unix_server.path().display()
    );

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
    println!("[atman-daemon] http listening on http://{addr}");

    let shutdown = CancellationToken::new();
    let unix_task = {
        let sh = shutdown.clone();
        let st = state.clone();
        tokio::spawn(async move { unix_server.serve(st, sh).await })
    };

    let ctrlc_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        ctrlc_shutdown.cancel();
    });

    let serve = axum::serve(listener, router(http_state))
        .with_graceful_shutdown(async move { shutdown.cancelled().await });
    serve.await?;
    let _ = unix_task.await;
    pidfile::remove_pid(&pid_path);
    Ok(())
}

fn default_sessions_dir() -> Result<std::path::PathBuf> {
    let base = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir")?
        .data_dir()
        .to_path_buf();
    Ok(base.join("sessions"))
}

fn default_socket_path() -> Result<std::path::PathBuf> {
    let base = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir")?
        .data_dir()
        .to_path_buf();
    Ok(base.join("run").join("atman.sock"))
}
