mod douban;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

/// Where record links point, whatever `DOUBAN_SITE` the requests go to.
pub const PUBLIC_SITE: &str = "https://book.douban.com";

/// Douban challenges heavy traffic, so the provider never goes faster than this, whatever it is
/// configured to do.
const MIN_REQUEST_INTERVAL_MS: u64 = 500;
const MAX_IN_FLIGHT: usize = 2;

pub struct Config {
    pub auth_token: Option<String>,
    pub request_interval: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let site = env("DOUBAN_SITE", PUBLIC_SITE);
    // Not configurable: the provider always says what it is, and is never dressed up as a browser.
    let user_agent = format!("bookorbit-douban-provider/{VERSION}");
    let cookie = std::env::var("DOUBAN_COOKIE")
        .ok()
        .map(|cookie| cookie.trim().to_string())
        .filter(|cookie| !cookie.is_empty());
    let interval_ms = std::env::var("REQUEST_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(MIN_REQUEST_INTERVAL_MS)
        .max(MIN_REQUEST_INTERVAL_MS);
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        request_interval: Duration::from_millis(interval_ms),
    };
    let client = douban::Client::new(
        &site,
        &user_agent,
        cookie.as_deref(),
        MAX_IN_FLIGHT,
        cfg.request_interval,
    );
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let port_number = port.parse::<u16>().expect("PORT must be a port number");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port_number))
        .await
        .expect("could not bind the port");
    eprintln!(
        "[provider] [start] port={port} site={site} version={VERSION} intervalMs={interval_ms} cookie={} - listening",
        cookie.is_some()
    );
    axum::serve(listener, app).await.expect("server stopped");
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
