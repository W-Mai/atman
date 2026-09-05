use atman_client::{Client, ClientIdentity, UnixTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::args()
        .nth(1)
        .ok_or_else(|| std::io::Error::other("usage: attach <path-to-atman.sock>"))?;
    let client = Client::connect(
        UnixTransport::new(socket_path),
        ClientIdentity::new("rust-example", env!("CARGO_PKG_VERSION")),
    )
    .await?;

    let created = client
        .create_session(
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            Some("SDK example".into()),
        )
        .await?;
    let session_id = created.session_id().clone();
    let session = client.attach_session(session_id).await?;
    let mut states = session.subscribe();

    let synchronizer = session.clone();
    let synchronization = tokio::spawn(async move { synchronizer.synchronize().await });
    session
        .send_message("Summarize the current workspace.", None, Vec::new())
        .await?;
    states.changed().await?;

    let state = states.borrow_and_update();
    println!(
        "session={} cursor={} messages={}",
        session.session_id(),
        state.cursor().0,
        state.projection().transcript.len()
    );
    synchronization.abort();
    Ok(())
}
