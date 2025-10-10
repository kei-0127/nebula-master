use nebula_media::server::Server;

#[tokio::main]
pub async fn main() {
    nebula_log::init();
    match Server::new().await {
        Ok(mut server) => server.run().await,
        Err(e) => println!("{}", e),
    };
}
