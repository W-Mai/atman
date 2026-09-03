use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use atman_proto::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

use crate::{DaemonState, dispatch};

pub struct UnixServer {
    listener: UnixListener,
    path: PathBuf,
}

impl UnixServer {
    pub async fn bind(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        if path.exists() {
            tokio::fs::remove_file(&path).await.ok();
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind unix socket {}", path.display()))?;
        set_socket_perms(&path)?;
        Ok(Self { listener, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn serve(self, state: Arc<DaemonState>, shutdown: CancellationToken) -> Result<()> {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result {
                        atman_runtime::notify!(warn, location = Log, stack = merge_count("daemon.unix_conn_join_error", 60_000), "unix connection task failed: {error}");
                    }
                }
                accepted = self.listener.accept() => {
                    let (stream, _addr) = accepted?;
                    let state = state.clone();
                    let connection_shutdown = shutdown.clone();
                    connections.spawn(async move {
                        if let Err(e) = handle_conn(stream, state, connection_shutdown).await {
                            atman_runtime::notify!(warn, location = Log, stack = merge_count("daemon.unix_conn_error", 60_000), "unix conn error: {e}");
                        }
                    });
                }
            }
        }
        while connections.join_next().await.is_some() {}
        let _ = tokio::fs::remove_file(&self.path).await;
        Ok(())
    }
}

async fn handle_conn(
    stream: UnixStream,
    state: Arc<DaemonState>,
    shutdown: CancellationToken,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd).lines();
    loop {
        let line = tokio::select! {
            _ = shutdown.cancelled() => break,
            line = reader.next_line() => line?,
        };
        let Some(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => dispatch(state.clone(), req).await,
            Err(e) => JsonRpcResponse::err(None, JsonRpcError::parse_error(e.to_string())),
        };
        let mut buf = serde_json::to_vec(&resp)?;
        buf.push(b'\n');
        wr.write_all(&buf).await?;
    }
    Ok(())
}

fn set_socket_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
