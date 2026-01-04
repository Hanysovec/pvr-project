mod admin;
mod api;
mod quicksim;
mod server;
mod simulate;
mod topgear;
mod users;
mod utils;

#[tokio::main]
async fn main() {
    if let Err(e) = server::run_server().await {
        eprintln!("Server failed: {}", e);
    }
}
