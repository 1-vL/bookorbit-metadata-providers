mod fantlab;
mod mapping;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub site_base: String,
    pub auth_token: Option<String>,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let api = env("FANTLAB_API", "https://api.fantlab.ru");
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-fantlab-provider/{VERSION}"),
    );
    let cfg = Config {
        site_base: env("FANTLAB_SITE", "https://fantlab.ru"),
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
    };
    // fantlab.ru is a community site: two requests in flight and 250ms between starts keeps a
    // bulk refresh from looking like a scrape, whatever the client asks for.
    let client = fantlab::Client::new(&api, &user_agent, 2, Duration::from_millis(250));
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let port_number = port.parse::<u16>().expect("PORT must be a port number");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port_number))
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
