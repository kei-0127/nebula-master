use nebula_switchboard::server::Server;

#[tokio::main]
async fn main() {
    nebula_log::init();
    if let Ok(server) = Server::new().await {
        let _ = Server::run(server).await;
    }
}
