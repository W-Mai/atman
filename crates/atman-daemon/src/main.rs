use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    atman_daemon::server::serve().await
}
