use std::sync::Arc;

use anyhow::{Context, Result};
use atman_daemon::{DaemonState, http::router};

#[tokio::main]
async fn main() -> Result<()> {
    let sessions_dir = default_sessions_dir()?;
    let state = Arc::new(DaemonState::new(sessions_dir));
    let port = std::env::var("ATMAN_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(65099u16);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("[atman-daemon] listening on http://{addr}");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

fn default_sessions_dir() -> Result<std::path::PathBuf> {
    let base = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir")?
        .data_dir()
        .to_path_buf();
    Ok(base.join("sessions"))
}
