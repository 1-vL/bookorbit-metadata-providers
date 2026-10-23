mod lubimyczytac;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
    /// How long a search or lookup may take. A search that runs out of time answers with the books
    /// it has, marked partial, instead of letting BookOrbit's own timeout discard them all.
    pub deadline: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let site = env("LUBIMYCZYTAC_SITE", lubimyczytac::SITE);
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-lubimyczytac-provider/{VERSION}"),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        deadline: Duration::from_millis(
            std::env::var("DEADLINE_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(8000),
        ),
    };
    // lubimyczytac.pl is a community site: two pages in flight and 500 ms between starts, whatever
    // the clients ask for. The built-in source spaced its book pages 600 ms apart, one at a time.
    // One page at a time, a second apart: the site answered 429 to two at a time 500 ms apart.
    let client = lubimyczytac::Client::new(&site, &user_agent, 1, Duration::from_millis(1000));
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let port_number = port.parse::<u16>().expect("PORT must be a port number");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port_number))
        .await
        .expect("could not bind the port");
    eprintln!("[provider] [start] port={port} site={site} version={VERSION} - listening");
    axum::serve(listener, app).await.expect("server stopped");
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
