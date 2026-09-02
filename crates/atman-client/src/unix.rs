use std::path::{Path, PathBuf};

use atman_proto::{JsonRpcRequest, JsonRpcResponse};
use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::{RpcTransport, TransportError};

#[derive(Debug, Clone)]
pub struct UnixTransport {
    socket_path: PathBuf,
}

impl UnixTransport {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl RpcTransport for UnixTransport {
    fn send(
        &self,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
        Box::pin(async move {
            let stream = UnixStream::connect(&self.socket_path).await?;
            exchange(stream, request).await
        })
    }
}

async fn exchange<S>(
    mut stream: S,
    request: JsonRpcRequest,
) -> Result<JsonRpcResponse, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut payload = serde_json::to_vec(&request)?;
    payload.push(b'\n');
    stream.write_all(&payload).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    if BufReader::new(stream).read_line(&mut response).await? == 0 {
        return Err(TransportError::Closed);
    }
    Ok(serde_json::from_str(&response)?)
}

#[cfg(test)]
mod tests {
    use atman_proto::{EmptyParams, JsonRpcResponse, methods};

    use super::*;

    #[tokio::test]
    async fn request_and_response_use_one_ndjson_frame() {
        let (client, server) = tokio::io::duplex(4_096);
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut line = String::new();
            BufReader::new(read).read_line(&mut line).await.unwrap();
            let request: JsonRpcRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.method, methods::PING);
            let response = JsonRpcResponse::ok(
                request.id,
                serde_json::json!({"pong": true, "version": "test"}),
            );
            write
                .write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes())
                .await
                .unwrap();
        });
        let request =
            JsonRpcRequest::for_method::<atman_proto::rpc::Ping>(1, &EmptyParams {}).unwrap();
        let response = exchange(client, request).await.unwrap();
        assert!(response.error.is_none());
        server.await.unwrap();
    }
}
