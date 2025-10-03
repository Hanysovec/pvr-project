mod server;
mod simulate;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run_server().await {
        eprintln!("Server failed: {}", e);
    }
}
