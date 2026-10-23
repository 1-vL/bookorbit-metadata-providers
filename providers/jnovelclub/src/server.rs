use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use crate::jnovelclub::{Catalogue, UpstreamError};
use crate::mapping::{
    author_matches, map_volume, pick_volume, rank_series, split_volume, BookMetadata,
};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 20;
/// Series whose volumes one search reads: a novel, its manga and a spin-off is the usual set.
const MAX_SERIES_PER_SEARCH: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ID_LENGTH: usize = 32;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    jnc: Arc<Catalogue>,
}

pub fn routes(cfg: Arc<Config>, jnc: Arc<Catalogue>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .with_state(AppState { cfg, jnc })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "J-Novel Club",
        "version": VERSION,
        "avatarUrl": "https://j-novel.club/apple-touch-icon.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "jnovelclub",
        "mediaTypes": ["book", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "publishedYear", "language",
            "seriesName", "seriesIndex", "genres"
        ],
        "searchParams": ["limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "coverShape": "portrait",
        // J-Novel Club's own theme-color, in its site.webmanifest and theme-color meta tag.
        "color": "#0057a8",
        // A search can read up to three volume lists upstream.
        "rateLimit": {
            "maxConcurrency": 1,
            "minIntervalMs": 2 * state.jnc.request_interval().as_millis() as u64,
        },
    }))
    .into_response()
}

#[derive(Serialize)]
struct SearchAnswer {
    matches: Vec<BookMetadata>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    partial: bool,
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let param = |key: &str| q.get(key).map(|v| v.trim()).unwrap_or_default();
    let query = param("query");
    if query.is_empty() && param("isbn").is_empty() {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    // J-Novel Club lists no ISBNs and no audiobooks.
    if query.is_empty() || param("mediaType") == "audiobook" {
        return Json(json!({ "matches": [] })).into_response();
    }
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .map_or(DEFAULT_LIMIT, |n| n.clamp(1, MAX_LIMIT));

    let found = tokio::time::timeout(REQUEST_TIMEOUT, find(&state, query, param("author"), limit))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("timed out".into())));
    match found {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error("search", err),
    }
}

struct Found {
    book: BookMetadata,
    by_author: bool,
    exact: bool,
    series_score: u32,
    volume_score: u32,
    rank: usize,
}

async fn find(
    state: &AppState,
    query: &str,
    author: &str,
    limit: usize,
) -> Result<SearchAnswer, UpstreamError> {
    let (title, volume) = split_volume(query);
    let catalogue = state.jnc.series().await?;
    let mut found = Vec::new();
    let mut failure = None;
    for (rank, candidate) in rank_series(&catalogue, &title, query)
        .into_iter()
        .take(MAX_SERIES_PER_SEARCH)
        .enumerate()
    {
        let volumes = match state.jnc.volumes(&candidate.series.id).await {
            Ok(volumes) => volumes,
            Err(UpstreamError::NotFound) => continue,
            Err(err) => {
                failure = Some(err);
                break;
            }
        };
        if let Some(pick) = pick_volume(candidate.series, &volumes, &title, query, &volume) {
            let v = &volumes[pick.index];
            found.push(Found {
                book: map_volume(v, Some(candidate.series)),
                by_author: author_matches(v, author),
                exact: pick.exact,
                series_score: candidate.score,
                volume_score: pick.score,
                rank,
            });
        }
    }
    if found.is_empty() {
        if let Some(err) = failure {
            return Err(err);
        }
    }
    // The asked-for author's books first, then a volume titled exactly as asked, then by series.
    found.sort_by_key(|f| {
        (
            !f.by_author,
            !f.exact,
            Reverse(f.series_score),
            Reverse(f.volume_score),
            f.rank,
        )
    });
    found.truncate(limit);
    Ok(SearchAnswer {
        matches: found.into_iter().map(|f| f.book).collect(),
        partial: failure.is_some(),
    })
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let valid = id.len() <= MAX_ID_LENGTH
        && id.strip_prefix("VOL-").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        });
    if !valid {
        return error(StatusCode::NOT_FOUND, "Not found");
    }

    let result = tokio::time::timeout(REQUEST_TIMEOUT, state.jnc.volume(&id))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("timed out".into())));
    match result {
        Ok((volume, series)) => Json(map_volume(&volume, series.as_ref())).into_response(),
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
            eprintln!("[provider.{op}] [fail] error={message:?} - j-novel club request failed");
            error(StatusCode::BAD_GATEWAY, "j-novel club unavailable")
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
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

    use super::*;
    use crate::jnovelclub::Client;

    #[derive(Default)]
    struct Upstream {
        /// An error status every route answers with instead, when not zero.
        status: AtomicU16,
        series_pages: AtomicUsize,
        requests: AtomicUsize,
    }

    /// Answers like labs.j-novel.club from the captured fixtures.
    async fn fake_jnc(upstream: Arc<Upstream>) -> String {
        let app = Router::new()
            .route(
                "/app/v2/series",
                get({
                    let upstream = upstream.clone();
                    move |Query(q): Query<HashMap<String, String>>| async move {
                        upstream.series_pages.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(q.get("format").map(String::as_str), Some("json"));
                        let page = match q.get("skip").map(String::as_str) {
                            Some("0") => include_str!("../testdata/series-page-1.json"),
                            Some("6") => include_str!("../testdata/series-page-2.json"),
                            _ => r#"{"series":[],"pagination":{"lastPage":true}}"#,
                        };
                        fixture(&upstream, page)
                    }
                }),
            )
            .route(
                "/app/v2/series/{id}/volumes",
                get({
                    let upstream = upstream.clone();
                    move |Path(id): Path<String>| async move {
                        fixture(
                            &upstream,
                            match id.as_str() {
                                "SER-Q7K7IYA" => {
                                    include_str!("../testdata/volumes-SER-Q7K7IYA.json")
                                }
                                "SER-LKJPCCI" => {
                                    include_str!("../testdata/volumes-SER-LKJPCCI.json")
                                }
                                "SER-DGZBWMY" => {
                                    include_str!("../testdata/volumes-SER-DGZBWMY.json")
                                }
                                "SER-N6KH4FA" => {
                                    include_str!("../testdata/volumes-SER-N6KH4FA.json")
                                }
                                "SER-CRHCSBA" => {
                                    include_str!("../testdata/volumes-SER-CRHCSBA.json")
                                }
                                _ => include_str!("../testdata/volumes-empty.json"),
                            },
                        )
                    }
                }),
            )
            .route(
                "/app/v2/volumes/{id}",
                get({
                    let upstream = upstream.clone();
                    move |Path(id): Path<String>| async move {
                        match id.as_str() {
                            "VOL-SD5FU4Q" => fixture(
                                &upstream,
                                include_str!("../testdata/volume-VOL-SD5FU4Q.json"),
                            ),
                            _ => not_found(&upstream),
                        }
                    }
                }),
            )
            .route(
                "/app/v2/volumes/{id}/serie",
                get(move |Path(id): Path<String>| async move {
                    match id.as_str() {
                        "VOL-SD5FU4Q" => fixture(
                            &upstream,
                            include_str!("../testdata/serie-VOL-SD5FU4Q.json"),
                        ),
                        _ => not_found(&upstream),
                    }
                }),
            );
        serve(app).await
    }

    fn fixture(upstream: &Upstream, body: &'static str) -> Response {
        upstream.requests.fetch_add(1, Ordering::SeqCst);
        match upstream.status.load(Ordering::SeqCst) {
            0 => (StatusCode::OK, body).into_response(),
            code => (
                StatusCode::from_u16(code).unwrap(),
                [(header::RETRY_AFTER, "42")],
                "",
            )
                .into_response(),
        }
    }

    fn not_found(upstream: &Upstream) -> Response {
        upstream.requests.fetch_add(1, Ordering::SeqCst);
        StatusCode::NOT_FOUND.into_response()
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider(auth_token: Option<&str>) -> (String, Arc<Upstream>) {
        let upstream = Arc::new(Upstream::default());
        let base = fake_jnc(upstream.clone()).await;
        let cfg = Config {
            auth_token: auth_token.map(str::to_string),
            request_interval: Duration::ZERO,
        };
        let client = Arc::new(Client::new(
            &format!("{base}/app/v2"),
            "test",
            Duration::ZERO,
        ));
        let catalogue = Arc::new(Catalogue::new(
            client,
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        ));
        (serve(routes(Arc::new(cfg), catalogue)).await, upstream)
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

    fn ids(body: &serde_json::Value) -> Vec<&str> {
        body["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["providerId"].as_str().unwrap())
            .collect()
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
            (Some("jnovelclub"), Some(true), Some(false))
        );
        assert_eq!(body["mediaTypes"], json!(["book", "comic"]));
        assert_eq!(
            (body["avatarUrl"].as_str(), body["color"].as_str()),
            (
                Some("https://j-novel.club/apple-touch-icon.png"),
                Some("#0057a8")
            )
        );
        assert_eq!(body["rateLimit"]["maxConcurrency"], 1);
    }

    #[tokio::test]
    async fn search_finds_the_volume_a_title_names() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(
            &format!(
                "{base}/search?query=Ascendance+of+a+Bookworm%3A+Part+3+Volume+2&mediaType=book"
            ),
            None,
        )
        .await;
        assert_eq!(code, 200);
        // The novel's volume, then the manga's volume of the same name.
        assert_eq!(ids(&body), ["VOL-SD5FU4Q", "VOL-KRQFYFA"]);
        let book = &body["matches"][0];
        assert_eq!(book["title"], "Ascendance of a Bookworm: Part 3 Volume 2");
        assert_eq!(
            book["series"],
            json!([{ "series": "Ascendance of a Bookworm", "sequence": "9" }])
        );
        assert_eq!(book["authors"], json!(["Miya Kazuki"]));
        assert_eq!(
            (book["publisher"].as_str(), book["languageCode"].as_str()),
            (Some("J-Novel Club"), Some("en"))
        );
    }

    #[tokio::test]
    async fn search_takes_the_volume_off_the_query() {
        let (base, _) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Ascendance+of+a+Bookworm+Vol.+3"),
            None,
        )
        .await;
        // The novel's third volume, the manga's, and the third fanbook.
        assert_eq!(ids(&body), ["VOL-BAWJ2DI", "VOL-RZDB2VY", "VOL-XJYQS6I"]);
        assert_eq!(body["matches"][0]["series"][0]["sequence"], "3");
    }

    #[tokio::test]
    async fn search_without_a_volume_offers_first_volumes() {
        let (base, _) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Ascendance+of+a+Bookworm&limit=2"),
            None,
        )
        .await;
        assert_eq!(ids(&body), ["VOL-ILVLS2Y", "VOL-VURSWBA"]);
        let (_, body, _) = get_json(&format!("{base}/search?query=Crybaby+Love+Song"), None).await;
        assert_eq!(body["matches"][0]["title"], "Crybaby Love Song");
    }

    #[tokio::test]
    async fn search_puts_the_asked_for_author_first() {
        let (base, _) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Ascendance+of+a+Bookworm+Vol.+3&author=Suzuka"),
            None,
        )
        .await;
        assert_eq!(ids(&body)[0], "VOL-RZDB2VY");
    }

    #[tokio::test]
    async fn search_matches_a_volume_subtitle() {
        let (base, _) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Grimgar+of+Fantasy+and+Ash+Everything+is+Precious"),
            None,
        )
        .await;
        assert_eq!(ids(&body), ["VOL-QQKUUAQ"]);
    }

    #[tokio::test]
    async fn search_answers_empty_and_refuses_no_query() {
        let (base, _) = provider(None).await;
        for path in [
            "search?query=Berserk",
            "search?query=Ascendance+of+a+Bookworm&mediaType=audiobook",
            "search?isbn=9781718356009",
        ] {
            let (code, body, _) = get_json(&format!("{base}/{path}"), None).await;
            assert_eq!(
                (code, body["matches"].as_array().map(Vec::len)),
                (200, Some(0)),
                "{path}"
            );
        }
        let (code, _, _) = get_json(&format!("{base}/search"), None).await;
        assert_eq!(code, 400);
    }

    #[tokio::test]
    async fn series_list_is_fetched_once_and_paged() {
        let (base, upstream) = provider(None).await;
        for query in ["Crybaby+Love+Song", "Grimgar+of+Fantasy+and+Ash"] {
            let (code, body, _) = get_json(&format!("{base}/search?query={query}"), None).await;
            assert_eq!((code, ids(&body).len()), (200, 1), "{query}");
        }
        assert_eq!(upstream.series_pages.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lookup_answers_a_volume_and_404() {
        let (base, upstream) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/book/VOL-SD5FU4Q"), None).await;
        assert_eq!(
            (code, body["providerId"].as_str()),
            (200, Some("VOL-SD5FU4Q"))
        );
        assert_eq!(body["series"][0]["sequence"], "9");
        assert_eq!(
            body["url"],
            "https://j-novel.club/series/ascendance-of-a-bookworm#volume-9"
        );
        let asked = upstream.requests.load(Ordering::SeqCst);
        for id in [
            "VOL-ZZZZZZZ",
            "conformance-unknown-id",
            "vol-sd5fu4q",
            "VOL-",
            "SER-Q7K7IYA",
            &format!("VOL-{}", "A".repeat(40)),
        ] {
            let (code, _, _) = get_json(&format!("{base}/book/{id}"), None).await;
            assert_eq!(code, 404, "lookup {id}");
        }
        // Only the well-formed unknown id reached J-Novel Club.
        assert_eq!(upstream.requests.load(Ordering::SeqCst), asked + 1);
    }

    #[tokio::test]
    async fn lookup_after_a_search_is_answered_from_memory() {
        let (base, upstream) = provider(None).await;
        let (_, body, _) = get_json(
            &format!("{base}/search?query=Ascendance+of+a+Bookworm+Vol.+3"),
            None,
        )
        .await;
        let asked = upstream.requests.load(Ordering::SeqCst);
        let id = ids(&body)[0].to_string();
        let (code, book, _) = get_json(&format!("{base}/book/{id}"), None).await;
        assert_eq!(
            (code, book["providerId"].as_str()),
            (200, Some(id.as_str()))
        );
        assert_eq!(upstream.requests.load(Ordering::SeqCst), asked);
    }

    #[tokio::test]
    async fn upstream_throttle_and_failure() {
        let (base, upstream) = provider(None).await;
        upstream.status.store(429, Ordering::SeqCst);
        let (code, _, headers) =
            get_json(&format!("{base}/search?query=Crybaby+Love+Song"), None).await;
        assert_eq!(
            (
                code,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        upstream.status.store(500, Ordering::SeqCst);
        let (code, _, _) = get_json(&format!("{base}/book/VOL-SD5FU4Q"), None).await;
        assert_eq!(code, 502);
        let (code, _, _) = get_json(&format!("{base}/search?query=Crybaby+Love+Song"), None).await;
        assert_eq!(code, 502);
        // Once J-Novel Club answers again, so does the provider.
        upstream.status.store(0, Ordering::SeqCst);
        let (code, _, _) = get_json(&format!("{base}/search?query=Crybaby+Love+Song"), None).await;
        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn auth_token() {
        let (base, _) = provider(Some("Bearer s3cret")).await;
        let search = format!("{base}/search?query=Crybaby+Love+Song");
        assert_eq!(get_json(&search, None).await.0, 401);
        assert_eq!(get_json(&search, Some("Bearer s3cret")).await.0, 200);
        assert_eq!(
            get_json(&format!("{base}/book/VOL-SD5FU4Q"), Some("Bearer nope"))
                .await
                .0,
            401
        );
        assert_eq!(
            get_json(&format!("{base}/provider/info"), None).await.0,
            200
        );
    }

    #[tokio::test]
    async fn pacer_turns_a_long_queue_into_a_429() {
        let pacer = crate::jnovelclub::Pacer::new(Duration::from_secs(10));
        assert!(pacer.wait().await.is_ok());
        pacer.push_back(Duration::from_secs(20));
        assert!(matches!(
            pacer.wait().await,
            Err(UpstreamError::RateLimited { .. })
        ));
    }
}
