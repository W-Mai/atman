use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
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
    identity: (u64, u64),
    _lease: atman_runtime::platform::ExclusiveFileLease,
}

impl UnixServer {
    pub async fn bind(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let lease_path = path.clone();
        let lease = tokio::task::spawn_blocking(move || -> Result<_> {
            if let Some(parent) = lease_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut name = lease_path.as_os_str().to_owned();
            name.push(".lock");
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(PathBuf::from(name))?;
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "socket lease is not a regular file"
            );
            Ok(atman_runtime::platform::ExclusiveFileLease::try_acquire(
                file,
            )?)
        })
        .await??;
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "socket path is not a socket"
                );
                // An older server may not participate in the lease protocol.
                match UnixStream::connect(&path).await {
                    Ok(_) => anyhow::bail!("unix socket already listening at {}", path.display()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
                let current = tokio::fs::symlink_metadata(&path).await?;
                anyhow::ensure!(
                    current.dev() == metadata.dev() && current.ino() == metadata.ino(),
                    "socket path changed during startup"
                );
                tokio::fs::remove_file(&path).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind unix socket {}", path.display()))?;
        set_socket_perms(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            identity: (metadata.dev(), metadata.ino()),
            _lease: lease,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn serve(self, state: Arc<DaemonState>, shutdown: CancellationToken) -> Result<()> {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    let (stream, _addr) = accepted?;
                    let state = state.clone();
                    let connection_shutdown = shutdown.clone();
                    tokio::spawn(async move {
                        let result = tokio::select! {
                            biased;
                            _ = connection_shutdown.cancelled() => Ok(()),
                            result = handle_conn(stream, state) => result,
                        };
                        if let Err(e) = result {
                            atman_runtime::notify!(warn, location = Log, stack = merge_count("daemon.unix_conn_error", 60_000), "unix conn error: {e}");
                        }
                    });
                }
            }
        }
        Ok(())
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn handle_conn(stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd).lines();
    while let Some(line) = reader.next_line().await? {
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
