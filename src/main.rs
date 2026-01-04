use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
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
    let file_appender = tracing_appender::rolling::daily("logs", "server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_file(true)
                .with_line_number(true),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();

    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("Panic: {:?}", info);
        default_panic(info);
    }));
    tracing::info!("Logs are saved in 'logs/server.log'");
    match server::run_server().await {
        Ok(_) => tracing::info!("Server stopped OK"),
        Err(e) => tracing::error!("Server crashed: {}", e),
    }
}
