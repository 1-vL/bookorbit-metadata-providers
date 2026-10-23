mod romanceio;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query as QueryParams, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use romanceio::{Query, RomanceIo, Source, SourceError};

pub const VERSION: &str = "0.1.0";
const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

struct App<S> {
    source: Arc<S>,
    auth_token: Option<String>,
}

impl<S> Clone for App<S> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            auth_token: self.auth_token.clone(),
        }
    }
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let site = env("ROMANCEIO_SITE", romanceio::SITE);
    let user_agent = env(
        "USER_AGENT",
        &format!(
            "Mozilla/5.0 (compatible; BookOrbit-RomanceIo-Provider/{VERSION}; +https://github.com/bookorbit/bookorbit-metadata-providers)"
        ),
    );
    let auth_token = std::env::var("AUTH_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let source = RomanceIo::new(&site, &user_agent, 1, Duration::from_millis(1000));
    let app = routes(source, auth_token);

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

fn routes<S: Source>(source: S, auth_token: Option<String>) -> Router {
    Router::new()
        .route("/provider/info", get(info::<S>))
        .route("/search", get(search::<S>))
        .route("/book/{id}", get(lookup::<S>))
        .layer(middleware::from_fn(log_requests))
        .with_state(App {
            source: Arc::new(source),
            auth_token,
        })
}

async fn info<S: Source>(State(app): State<App<S>>) -> Response {
    Json(json!({
        "name": "Romance.io",
        "version": VERSION,
        "avatarUrl": "https://www.romance.io/icons/android-icon-192x192.png",
        "authenticationRequired": app.auth_token.is_some(),
        "key": "romanceio",
        "mediaTypes": ["book"],
        "lookup": true,
        "searchParams": ["query", "author", "limit", "mediaType"],
        "supportedFields": [
            "title", "description", "cover", "authors", "publishedYear", "language",
            "pageCount", "seriesName", "seriesIndex", "genres", "communityRating"
        ],
        "bookUrlTemplate": "https://www.romance.io/books/{id}",
        "idMaxLength": 24,
        "coverShape": "portrait",
        "color": "#dd0489",
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": 1000 },
    }))
    .into_response()
}

async fn search<S: Source>(
    State(app): State<App<S>>,
    headers: HeaderMap,
    QueryParams(params): QueryParams<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(app.auth_token.as_deref(), &headers) {
        return denied;
    }
    let param = |key: &str| {
        params
            .get(key)
            .map(|value| value.trim())
            .unwrap_or_default()
    };
    let query = Query {
        title: param("query").to_string(),
        author: param("author").to_string(),
        media_type: param("mediaType").to_string(),
        limit: parse_limit(param("limit")),
    };
    if query.title.is_empty() {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }

    match tokio::time::timeout(REQUEST_TIMEOUT, app.source.search(&query)).await {
        Ok(Ok(answer)) => Json(answer).into_response(),
        Ok(Err(err)) => source_error("search", err),
        Err(_) => source_error("search", SourceError::Failed("request timed out".into())),
    }
}

async fn lookup<S: Source>(
    State(app): State<App<S>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(app.auth_token.as_deref(), &headers) {
        return denied;
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, app.source.lookup(&id)).await {
        Ok(Ok(Some(book))) => Json(book).into_response(),
        Ok(Ok(None)) | Ok(Err(SourceError::NotFound)) => error(StatusCode::NOT_FOUND, "Not found"),
        Ok(Err(err)) => source_error("lookup", err),
        Err(_) => source_error("lookup", SourceError::Failed("request timed out".into())),
    }
}

fn parse_limit(raw: &str) -> usize {
    raw.parse::<usize>()
        .map_or(DEFAULT_LIMIT, |limit| limit.clamp(1, MAX_LIMIT))
}

fn source_error(op: &str, err: SourceError) -> Response {
    match err {
        SourceError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        SourceError::RateLimited {
            retry_after_seconds,
        } => {
            let mut response = error(
                StatusCode::TOO_MANY_REQUESTS,
                "romance.io is rate limiting this provider",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            response
        }
        SourceError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - romance.io request failed");
            error(StatusCode::BAD_GATEWAY, "romance.io is unavailable")
        }
    }
}

fn deny_unauthorized(token: Option<&str>, headers: &HeaderMap) -> Option<Response> {
    let token = token?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    (!constant_time_eq(given, token.as_bytes()))
        .then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    eprintln!(
        "[request] [end] method={method} path={path} status={} durationMs={} - request served",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    use super::*;
    use crate::romanceio::{BookMetadata, SearchAnswer};

    #[derive(Clone)]
    struct FakeSource;

    impl Source for FakeSource {
        async fn search(&self, query: &Query) -> Result<SearchAnswer, SourceError> {
            Ok(SearchAnswer {
                matches: vec![BookMetadata {
                    media_type: "book".into(),
                    provider_id: "5484ecd47a5936fb0405756c".into(),
                    title: query.title.clone(),
                    ..BookMetadata::default()
                }],
                partial: false,
            })
        }

        async fn lookup(&self, id: &str) -> Result<Option<BookMetadata>, SourceError> {
            Ok((id == "5484ecd47a5936fb0405756c").then(|| BookMetadata {
                media_type: "book".into(),
                provider_id: id.into(),
                title: "Pride and Prejudice".into(),
                ..BookMetadata::default()
            }))
        }
    }

    async fn get(app: Router, uri: &str, auth: Option<&str>) -> (StatusCode, String) {
        let mut request = HttpRequest::get(uri);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn info_declares_the_provider_contract() {
        let (status, body) = get(routes(FakeSource, None), "/provider/info", None).await;
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["key"], "romanceio");
        assert_eq!(info["lookup"], true);
        assert_eq!(info["idMaxLength"], 24);
        assert_eq!(info["color"], "#dd0489");
        assert!(info["supportedFields"]
            .as_array()
            .unwrap()
            .contains(&json!("genres")));
    }

    #[tokio::test]
    async fn search_and_lookup_serve_contract_shapes() {
        let app = || routes(FakeSource, None);
        let (status, body) = get(app(), "/search?query=Pride%20and%20Prejudice", None).await;
        let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            answer["matches"][0]["providerId"],
            "5484ecd47a5936fb0405756c"
        );

        assert_eq!(
            get(app(), "/book/5484ecd47a5936fb0405756c", None).await.0,
            StatusCode::OK
        );
        assert_eq!(
            get(app(), "/book/000000000000000000000000", None).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn query_validation_and_auth_are_enforced() {
        assert_eq!(
            get(routes(FakeSource, None), "/search", None).await.0,
            StatusCode::BAD_REQUEST
        );
        let app = || routes(FakeSource, Some("Bearer secret".into()));
        assert_eq!(
            get(app(), "/search?query=x", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app(), "/search?query=x", Some("Bearer secret")).await.0,
            StatusCode::OK
        );
        assert_eq!(get(app(), "/provider/info", None).await.0, StatusCode::OK);
    }
}
