use std::sync::Arc;

use atman_daemon::{DaemonState, unix::UnixServer};
use atman_proto::{JsonRpcRequest, JsonRpcResponse, methods};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn unix_socket_ping_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("atman.sock");
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let server = UnixServer::bind(&sock_path).await.unwrap();
    let shutdown = CancellationToken::new();
    let sh_clone = shutdown.clone();
    let server_task = tokio::spawn(async move { server.serve(state, sh_clone).await });

    // wait for socket ready
    for _ in 0..20 {
        if sock_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let stream = UnixStream::connect(&sock_path).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();

    let req = JsonRpcRequest::new(1, methods::PING, serde_json::json!({}));
    let mut buf = serde_json::to_vec(&req).unwrap();
    buf.push(b'\n');
    wr.write_all(&buf).await.unwrap();

    let line = lines
        .next_line()
        .await
        .unwrap()
        .expect("expected response line");
    let resp: JsonRpcResponse = serde_json::from_str(&line).unwrap();
    let result = resp.result.expect("expect ok");
    assert_eq!(result["pong"], serde_json::json!(true));

    drop(wr);
    shutdown.cancel();
    let _ = server_task.await;
}

#[tokio::test]
async fn unix_socket_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let sock_path = tmp.path().join("atman.sock");
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let server = UnixServer::bind(&sock_path).await.unwrap();
    let shutdown = CancellationToken::new();
    let sh_clone = shutdown.clone();
    let handle = tokio::spawn(async move { server.serve(state, sh_clone).await });

    let mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    shutdown.cancel();
    let _ = handle.await;
}
