mod jnovelclub;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
    /// Gap between requests sent to J-Novel Club. It publishes no limit, so the default stays well
    /// under what its own site sends while someone browses.
    pub request_interval: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let api = env("JNC_API", "https://labs.j-novel.club/app/v2");
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-jnovelclub-provider/{VERSION}"),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        request_interval: env_millis("REQUEST_INTERVAL_MS", 500),
    };
    let client = Arc::new(jnovelclub::Client::new(
        &api,
        &user_agent,
        cfg.request_interval,
    ));
    let catalogue = Arc::new(jnovelclub::Catalogue::new(
        client.clone(),
        env_minutes("SERIES_REFRESH_MINUTES", 360),
        env_minutes("VOLUME_CACHE_MINUTES", 60),
    ));
    // Load the series list now, so the first search does not wait for it.
    tokio::spawn({
        let catalogue = catalogue.clone();
        async move {
            let _ = catalogue.series().await;
        }
    });
    let app = server::routes(Arc::new(cfg), catalogue);

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

fn env_minutes(key: &str, fallback: u64) -> Duration {
    Duration::from_secs(
        60 * std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|minutes| *minutes > 0)
            .unwrap_or(fallback),
    )
}
