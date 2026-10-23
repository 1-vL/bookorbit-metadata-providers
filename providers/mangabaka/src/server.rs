use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;
use serde_json::json;

use crate::mangabaka::{Client, Series, UpstreamError};
use crate::mapping::{
    author_matches, map_series, names_exactly, shares_a_word, split_volume, BookMetadata,
};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A series id, then `v` and a volume for one volume of it: `1692`, `1692v3`, `1692v10.5`.
const MAX_ID_LENGTH: usize = 20;
static BOOK_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]{1,12})(?:v([0-9]{1,4}(?:\.[0-9]{1,2})?))?$").expect("valid id pattern")
});

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    mb: Arc<Client>,
}

pub fn routes(cfg: Arc<Config>, mb: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .with_state(AppState { cfg, mb })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "MangaBaka",
        "version": VERSION,
        "avatarUrl": "https://mangabaka.org/images/maomao/180x180.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "mangabaka",
        "mediaTypes": ["book", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "publishedYear", "language",
            "seriesName", "seriesIndex", "genres", "communityRating"
        ],
        "searchParams": ["limit"],
        "idMaxLength": MAX_ID_LENGTH,
        // No `bookUrlTemplate`: a volume's id is not a page. Every record carries its series' page.
        "coverShape": "portrait",
        // MangaBaka's own `--primary`.
        "color": "#e30047",
        // Searches are the scarce call upstream: 30 a minute.
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": state.cfg.search_interval.as_millis() as u64 },
    }))
    .into_response()
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let query = q.get("query").map(|v| v.trim()).unwrap_or_default();
    if query.is_empty() {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    if q.get("mediaType").map(String::as_str) == Some("audiobook") {
        return Json(json!({ "matches": [] })).into_response();
    }
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(DEFAULT_LIMIT, |n| n.clamp(1, MAX_LIMIT));
    let (title, volume) = split_volume(query);

    let found =
        match tokio::time::timeout(REQUEST_TIMEOUT, state.mb.search_series(&title, limit)).await {
            Ok(Ok(found)) => found,
            Ok(Err(err)) => return upstream_error("search", err),
            Err(_) => return upstream_error("search", UpstreamError::Failed("timed out".into())),
        };

    let author = q.get("author").map(|v| v.trim()).unwrap_or_default();
    let mut active: Vec<&Series> = found
        .iter()
        .filter(|s| s.state == "active" && shares_a_word(s, &title))
        .collect();
    // The asked-for author's series first, then one named exactly as asked, otherwise MangaBaka's
    // own order: "Spy x Family" is the series, not the "Spy x Family: Family Portrait" novel.
    active.sort_by_key(|s| (!author_matches(s, author), !names_exactly(s, &title)));
    let matches: Vec<BookMetadata> = active.into_iter().map(|s| map_series(s, &volume)).collect();
    Json(json!({ "matches": matches })).into_response()
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let Some(captures) = BOOK_ID.captures(&raw) else {
        return error(StatusCode::NOT_FOUND, "Not found");
    };
    let Ok(id) = captures[1].parse::<u64>() else {
        return error(StatusCode::NOT_FOUND, "Not found");
    };
    let volume = captures.get(2).map_or("", |m| m.as_str()).to_string();

    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let found = state.mb.series(id).await?;
        // A series merged into another answers with the one it now lives in, once.
        match (found.state.as_str(), found.merged_with) {
            ("merged", Some(target)) => state.mb.series(target).await,
            _ => Ok(found),
        }
    })
    .await
    .unwrap_or_else(|_| Err(UpstreamError::Failed("timed out".into())));

    match result {
        Ok(found) if found.state == "active" => Json(map_series(&found, &volume)).into_response(),
        Ok(_) | Err(UpstreamError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
        Err(err) => upstream_error("lookup", err),
    }
}

/// Status codes are part of the contract: a client backs off on 429 and treats 5xx as a failure,
/// never as "no results".
fn upstream_error(op: &str, err: UpstreamError) -> Response {
    match err {
        UpstreamError::RateLimited {
            retry_after_seconds,
        } => {
            let mut res = error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - mangabaka request failed");
            error(StatusCode::BAD_GATEWAY, "mangabaka unavailable")
        }
    }
}

/// The refusal to send when a token is configured and the request does not carry it.
fn deny_unauthorized(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let token = state.cfg.auth_token.as_ref()?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    (!constant_time_eq(given, token.as_bytes()))
        .then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU16, Ordering};

    use super::*;

    /// Answers like api.mangabaka.org from the captured fixtures.
    async fn fake_mangabaka(status: Arc<AtomicU16>) -> String {
        let app = Router::new()
            .route(
                "/v1/series/search",
                get({
                    let status = status.clone();
                    move |Query(q): Query<HashMap<String, String>>| async move {
                        fixture(
                            &status,
                            if q.get("q").map(String::as_str) == Some("Berserk") {
                                include_str!("../testdata/search-berserk.json")
                            } else {
                                include_str!("../testdata/search-empty.json")
                            },
                        )
                    }
                }),
            )
            .route(
                "/v1/series/{id}",
                get(move |Path(id): Path<String>| async move {
                    match id.as_str() {
                        "1692" => fixture(&status, include_str!("../testdata/series-1692.json")),
                        "999001" => fixture(
                            &status,
                            include_str!("../testdata/series-999001-merged.json"),
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            include_str!("../testdata/not-found.json"),
                        )
                            .into_response(),
                    }
                }),
            );
        serve(app).await
    }

    fn fixture(status: &AtomicU16, body: &'static str) -> Response {
        match status.load(Ordering::SeqCst) {
            0 => (StatusCode::OK, body).into_response(),
            code => (
                StatusCode::from_u16(code).unwrap(),
                [(header::RETRY_AFTER, "42")],
                "",
            )
                .into_response(),
        }
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider(auth_token: Option<&str>) -> (String, Arc<AtomicU16>) {
        let status = Arc::new(AtomicU16::new(0));
        let upstream = fake_mangabaka(status.clone()).await;
        let cfg = Config {
            auth_token: auth_token.map(str::to_string),
            search_interval: Duration::from_secs(2),
            lookup_interval: Duration::ZERO,
        };
        let client = Client::new(&upstream, "test", Duration::ZERO, Duration::ZERO);
        (serve(routes(Arc::new(cfg), Arc::new(client))).await, status)
    }

    async fn get_json(url: &str, auth: Option<&str>) -> (u16, serde_json::Value, HeaderMap) {
        let mut req = reqwest::Client::new().get(url);
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let body =
            serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    #[tokio::test]
    async fn info_declares_the_contract() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/provider/info"), None).await;
        assert_eq!(code, 200);
        assert_eq!(
            (
                body["key"].as_str(),
                body["lookup"].as_bool(),
                body["authenticationRequired"].as_bool()
            ),
            (Some("mangabaka"), Some(true), Some(false))
        );
        assert_eq!(body["rateLimit"]["minIntervalMs"], 2000);
        assert_eq!(
            (body["avatarUrl"].as_str(), body["color"].as_str()),
            (
                Some("https://mangabaka.org/images/maomao/180x180.png"),
                Some("#e30047")
            )
        );
    }

    #[tokio::test]
    async fn search_strips_the_volume_and_puts_it_on_the_series() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Berserk+Vol.+3&mediaType=book"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 5);
        assert_eq!(matches[0]["providerId"], "1692v3");
        assert_eq!(matches[0]["series"][0]["sequence"], "3");
    }

    #[tokio::test]
    async fn search_puts_the_asked_for_author_first() {
        let (base, _) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Berserk&author=Ichika+Isshiki"),
            None,
        )
        .await;
        assert_eq!(body["matches"][0]["providerId"], "60246");
    }

    #[tokio::test]
    async fn search_answers_empty_and_refuses_no_query() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/search?query=nothing"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Berserk&mediaType=audiobook"),
            None,
        )
        .await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        let (code, _, _) = get_json(&format!("{base}/search"), None).await;
        assert_eq!(code, 400);
    }

    #[tokio::test]
    async fn lookup_follows_a_merge_and_answers_404() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/book/999001"), None).await;
        assert_eq!((code, body["providerId"].as_str()), (200, Some("1692")));
        // A volume's id answers that volume, as the search did.
        let (code, body, _) = get_json(&format!("{base}/book/1692v3"), None).await;
        assert_eq!(
            (
                code,
                body["title"].as_str(),
                body["series"][0]["sequence"].as_str()
            ),
            (200, Some("Berserk, Vol. 3"), Some("3"))
        );
        for id in ["5", "abc", "..", &"9".repeat(13), "1692v", "1692v3v4"] {
            let (code, _, _) = get_json(&format!("{base}/book/{id}"), None).await;
            assert_eq!(code, 404, "lookup {id}");
        }
    }

    #[tokio::test]
    async fn upstream_throttle_and_failure() {
        let (base, status) = provider(None).await;
        status.store(429, Ordering::SeqCst);
        let (code, _, headers) = get_json(&format!("{base}/search?query=Berserk"), None).await;
        assert_eq!(
            (
                code,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        status.store(500, Ordering::SeqCst);
        let (code, _, _) = get_json(&format!("{base}/book/1692"), None).await;
        assert_eq!(code, 502);
    }

    #[tokio::test]
    async fn auth_token() {
        let (base, _) = provider(Some("Bearer s3cret")).await;
        assert_eq!(
            get_json(&format!("{base}/search?query=Berserk"), None)
                .await
                .0,
            401
        );
        assert_eq!(
            get_json(
                &format!("{base}/search?query=Berserk"),
                Some("Bearer s3cret")
            )
            .await
            .0,
            200
        );
        assert_eq!(
            get_json(&format!("{base}/provider/info"), None).await.0,
            200
        );
    }

    #[tokio::test]
    async fn pacer_turns_a_long_queue_into_a_429() {
        let pacer = crate::mangabaka::Pacer::new(Duration::from_secs(10));
        assert!(pacer.wait().await.is_ok());
        pacer.push_back(Duration::from_secs(20));
        assert!(matches!(
            pacer.wait().await,
            Err(UpstreamError::RateLimited { .. })
        ));
    }
}
