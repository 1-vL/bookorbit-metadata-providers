mod mangabaka;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
    /// Gaps between upstream requests. MangaBaka allows 30 searches and 180 other requests a
    /// minute per IP, so the defaults stay just inside both.
    pub search_interval: Duration,
    pub lookup_interval: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let api = env("MANGABAKA_API", "https://api.mangabaka.org");
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-mangabaka-provider/{VERSION}"),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        search_interval: env_millis("SEARCH_INTERVAL_MS", 2000),
        lookup_interval: env_millis("LOOKUP_INTERVAL_MS", 340),
    };
    let client =
        mangabaka::Client::new(&api, &user_agent, cfg.search_interval, cfg.lookup_interval);
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
