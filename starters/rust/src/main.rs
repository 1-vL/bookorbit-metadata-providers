//! A minimal BookOrbit metadata provider. Change `info()` below and `source.rs`; everything else
//! serves the contract.

mod source;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query as QueryParams, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use source::{ExampleSource, Query, Source, SourceError};

const VERSION: &str = "0.1.0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// `GET /provider/info`. Only `name` is required; every other field has a default in BookOrbit.
fn info(auth_required: bool) -> Value {
    json!({
        "name": "Example Provider",
        "version": VERSION,
        "authenticationRequired": auth_required,
        // Permanent: BookOrbit files every id and rating it stores under this key.
        "key": "example-provider",
        "mediaTypes": ["book"],
        "lookup": true,
        "searchParams": ["isbn", "limit"],
        "rateLimit": { "maxConcurrency": 2, "minIntervalMs": 0 },
    })
}

struct App<S> {
    source: S,
    auth_token: Option<String>,
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    let auth_token = std::env::var("AUTH_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("could not bind the port");
    eprintln!("listening on :{port}");
    axum::serve(listener, routes(ExampleSource::new(), auth_token))
        .await
        .expect("server stopped");
}

fn routes<S: Source>(source: S, auth_token: Option<String>) -> Router {
    Router::new()
        .route("/provider/info", get(handle_info::<S>))
        .route("/search", get(handle_search::<S>))
        .route("/book/{id}", get(handle_lookup::<S>))
        .with_state(Arc::new(App { source, auth_token }))
}

async fn handle_info<S: Source>(State(app): State<Arc<App<S>>>) -> Json<Value> {
    Json(info(app.auth_token.is_some()))
}

async fn handle_search<S: Source>(
    State(app): State<Arc<App<S>>>,
    headers: HeaderMap,
    QueryParams(params): QueryParams<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(app.auth_token.as_deref(), &headers) {
        return denied;
    }
    let param = |key: &str| {
        params
            .get(key)
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    };
    let query = Query {
        title: param("query"),
        author: param("author"),
        isbn: param("isbn"),
        media_type: param("mediaType"),
        limit: param("limit")
            .parse::<usize>()
            .map_or(5, |limit| limit.clamp(1, 50)),
    };
    if query.title.is_empty() && query.isbn.is_empty() {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, app.source.search(&query)).await {
        Ok(Ok(matches)) => Json(json!({ "matches": matches })).into_response(),
        Ok(Err(err)) => source_error(err),
        Err(_) => source_error(SourceError::Failed("timed out".into())),
    }
}

async fn handle_lookup<S: Source>(
    State(app): State<Arc<App<S>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(app.auth_token.as_deref(), &headers) {
        return denied;
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, app.source.lookup(&id)).await {
        Ok(Ok(Some(book))) => Json(book).into_response(),
        Ok(Ok(None)) => error(StatusCode::NOT_FOUND, "Not found"),
        Ok(Err(err)) => source_error(err),
        Err(_) => source_error(SourceError::Failed("timed out".into())),
    }
}

/// Status codes are part of the contract: BookOrbit backs off on 429 and treats 5xx as a failure it
/// may answer from its cache, never as "no results".
fn source_error(err: SourceError) -> Response {
    match err {
        SourceError::RateLimited {
            retry_after_seconds,
        } => {
            let mut res = error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        SourceError::Failed(message) => {
            eprintln!("source failed: {message}");
            error(StatusCode::BAD_GATEWAY, "source unavailable")
        }
    }
}

/// The refusal to send when `AUTH_TOKEN` is set and the request does not carry it exactly.
fn deny_unauthorized(token: Option<&str>, headers: &HeaderMap) -> Option<Response> {
    let token = token?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    let matches = given.len() == token.len()
        && given
            .iter()
            .zip(token.as_bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    (!matches).then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::source::BookMetadata;

    struct FailingSource(fn() -> SourceError);

    impl Source for FailingSource {
        async fn search(&self, _: &Query) -> Result<Vec<BookMetadata>, SourceError> {
            Err((self.0)())
        }
        async fn lookup(&self, _: &str) -> Result<Option<BookMetadata>, SourceError> {
            Err((self.0)())
        }
    }

    async fn get(app: Router, uri: &str, auth: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::get(uri);
        if let Some(auth) = auth {
            req = req.header(header::AUTHORIZATION, auth);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn search_answers_with_a_matches_list() {
        let (status, _, body) = get(
            routes(ExampleSource::new(), None),
            "/search?query=earthsea",
            None,
        )
        .await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["matches"][0]["providerId"], "ex-1");
        assert_eq!(body["matches"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn search_with_nothing_found_is_an_empty_list_not_null() {
        let (_, _, body) = get(
            routes(ExampleSource::new(), None),
            "/search?query=nothing-like-this",
            None,
        )
        .await;
        assert_eq!(body, r#"{"matches":[]}"#);
    }

    #[tokio::test]
    async fn search_without_a_query_is_a_bad_request() {
        assert_eq!(
            get(routes(ExampleSource::new(), None), "/search", None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn lookup_answers_404_for_an_unknown_id() {
        assert_eq!(
            get(routes(ExampleSource::new(), None), "/book/missing", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(routes(ExampleSource::new(), None), "/book/ex-1", None)
                .await
                .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn source_errors_map_to_the_status_codes_the_contract_defines() {
        let limited = || SourceError::RateLimited {
            retry_after_seconds: 30,
        };
        let (status, headers, _) = get(
            routes(FailingSource(limited), None),
            "/search?query=x",
            None,
        )
        .await;
        assert_eq!(
            (
                status,
                headers
                    .get(header::RETRY_AFTER)
                    .map(|v| v.to_str().unwrap())
            ),
            (StatusCode::TOO_MANY_REQUESTS, Some("30"))
        );

        let failed = || SourceError::Failed("upstream down".into());
        assert_eq!(
            get(routes(FailingSource(failed), None), "/search?query=x", None)
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn auth_token_guards_search_and_lookup_but_not_info() {
        let app = || routes(ExampleSource::new(), Some("Bearer s3cret".into()));
        assert_eq!(
            get(app(), "/search?query=x", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app(), "/book/ex-1", Some("Bearer s3cret")).await.0,
            StatusCode::OK
        );
        let (status, _, body) = get(app(), "/provider/info", None).await;
        let info: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (StatusCode::OK, Some(true))
        );
    }
}
