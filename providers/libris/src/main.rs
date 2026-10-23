mod libris;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
    /// Gap between requests sent to Libris. KB publishes no limit but blocks clients that strain
    /// the service, so the provider stays at one request a second whatever its clients ask for.
    pub interval: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let api = env("LIBRIS_API", "https://libris.kb.se");
    // KB asks API clients to name themselves and give a way to reach them.
    let user_agent = env(
        "USER_AGENT",
        &format!(
            "bookorbit-libris-provider/{VERSION} (+https://github.com/bookorbit/bookorbit-metadata-providers)"
        ),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        interval: env_millis("REQUEST_INTERVAL_MS", 1000),
    };
    let client = libris::Client::new(&api, &user_agent, cfg.interval);
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let listener = tokio::net::TcpListener::bind((
        "0.0.0.0",
        port.parse::<u16>().expect("PORT must be a port number"),
    ))
    .await
    .expect("could not bind the port");
    eprintln!("[provider] [start] port={port} api={api} version={VERSION} - listening");
    axum::serve(listener, app).await.expect("server stopped");
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn env_millis(key: &str, fallback: u64) -> Duration {
    Duration::from_millis(
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback),
    )
}
