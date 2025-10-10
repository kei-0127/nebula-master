use anyhow::Result;
use nebula_proxy::server::ProxyServer;

#[tokio::main]
async fn main() -> Result<()> {
    nebula_log::init();
    let mut server = ProxyServer::new();
    server.run().await?;
    Ok(())
}
