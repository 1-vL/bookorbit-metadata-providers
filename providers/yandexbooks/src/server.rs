use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use crate::mapping::{map_title, rank, search_hits, BookMetadata};
use crate::yandex::{search_url, Client, Title, UpstreamError};
use crate::{Config, VERSION};

/// Each match costs a page of its own, and Yandex takes two to three seconds to render one. Three,
/// two at a time after the search page, finish in about seven seconds, inside the deadline.
const MAX_LIMIT: usize = 3;
/// `comicbook-` and a site id of up to 16 characters.
const MAX_ID_LENGTH: usize = 32;
/// The longest pasted link a lookup will read.
const MAX_LOOKUP_LENGTH: usize = 512;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    yandex: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: some matches could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, yandex: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{*id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, yandex })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "Yandex Books",
        "version": VERSION,
        "avatarUrl": "https://books.yandex.ru/apple-touch-icon.png?v=3",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "yandexbooks",
        "mediaTypes": ["book", "audiobook", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "description", "cover", "audioCover", "authors", "narrators", "publisher",
            "publishedYear", "language", "pageCount", "duration", "seriesName", "seriesIndex",
            "genres"
        ],
        // Not `isbn`: Yandex Books shows no ISBNs and its search does not match them.
        "searchParams": ["limit"],
        "idMaxLength": MAX_ID_LENGTH,
        // `--brand-brand3` of the site's stylesheet, the red end of its logo's gradient.
        "color": "#ff5d5d",
        // A search already keeps the one upstream slot busy, so BookOrbit sends one call at a
        // time and each gets its whole timeout.
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": 500 },
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
    let param = |key: &str| q.get(key).map(|v| v.trim()).unwrap_or_default();
    let phrase = param("query");
    if phrase.is_empty() {
        // The site carries no ISBNs: an ISBN alone gets an honest empty answer.
        if !param("isbn").is_empty() {
            return Json(json!({ "matches": [] })).into_response();
        }
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    let limit = param("limit")
        .parse::<usize>()
        .map_or(MAX_LIMIT, |n| n.clamp(1, MAX_LIMIT));
    let deadline = tokio::time::Instant::now() + state.cfg.deadline;

    match search_books(
        &state,
        phrase,
        param("author"),
        param("mediaType"),
        limit,
        deadline,
    )
    .await
    {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error("search", err),
    }
}

async fn search_books(
    state: &AppState,
    phrase: &str,
    author: &str,
    media_type: &str,
    limit: usize,
    deadline: tokio::time::Instant,
) -> Result<SearchResponse, UpstreamError> {
    let html = tokio::time::timeout_at(deadline, state.yandex.page(&search_url(phrase)))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("search page timed out".into())))
        .map_err(|err| match err {
            UpstreamError::NotFound => UpstreamError::Failed("search page answered 404".into()),
            err => err,
        })?;

    let hits: Vec<Title> = rank(search_hits(&html), phrase, author, media_type)
        .into_iter()
        .take(limit)
        .map(|hit| hit.title)
        .collect();
    let mut tasks = JoinSet::new();
    for (index, title) in hits.iter().enumerate() {
        let (yandex, title) = (state.yandex.clone(), title.clone());
        tasks.spawn(async move { (index, fetch_title(&yandex, &title).await) });
    }

    let mut found: Vec<Option<BookMetadata>> = (0..hits.len()).map(|_| None).collect();
    let mut partial = false;
    let mut failure = None;
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(None) => break,
            Ok(Some(Ok((index, result)))) => match result {
                Ok(Some(book)) => found[index] = Some(book),
                // A page that is gone or carries no record is skipped.
                Ok(None) | Err(UpstreamError::NotFound) => partial = true,
                Err(err) => {
                    partial = true;
                    failure.get_or_insert(err);
                }
            },
            Ok(Some(Err(joined))) => {
                partial = true;
                failure.get_or_insert(UpstreamError::Failed(joined.to_string()));
            }
            Err(_) => {
                tasks.abort_all();
                partial = true;
                failure.get_or_insert(UpstreamError::Failed("title pages timed out".into()));
                break;
            }
        }
    }

    let matches: Vec<BookMetadata> = found.into_iter().flatten().collect();
    if matches.is_empty() && partial {
        return Err(
            failure.unwrap_or_else(|| UpstreamError::Failed("no title page could be read".into()))
        );
    }
    Ok(SearchResponse { matches, partial })
}

/// One title's page as an answer. `None` when the page carries no record of it.
async fn fetch_title(
    yandex: &Client,
    title: &Title,
) -> Result<Option<BookMetadata>, UpstreamError> {
    let html = yandex.page(&title.url()).await?;
    Ok(map_title(&html, title))
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let title = match Title::parse(&raw) {
        Some(title) if raw.len() <= MAX_LOOKUP_LENGTH => title,
        _ => return error(StatusCode::NOT_FOUND, "Not found"),
    };

    let result = tokio::time::timeout(state.cfg.deadline, fetch_title(&state.yandex, &title))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("title page timed out".into())));

    match result {
        Ok(Some(book)) => Json(book).into_response(),
        Ok(None) => {
            eprintln!("[provider.lookup] [fail] error=\"title page carried no record\" - yandex page unreadable");
            error(
                StatusCode::BAD_GATEWAY,
                "books.yandex.ru page could not be read",
            )
        }
        Err(UpstreamError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
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
            let mut res = error(
                StatusCode::TOO_MANY_REQUESTS,
                "books.yandex.ru is rate limiting this provider",
            );
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - yandex request failed");
            error(StatusCode::BAD_GATEWAY, "books.yandex.ru is unavailable")
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

async fn log_requests(req: Request, next: Next) -> Response {
    let (method, path, started) = (
        req.method().clone(),
        req.uri().path().to_string(),
        Instant::now(),
    );
    let res = next.run(req).await;
    eprintln!(
        "[request] [end] method={method} path={path} status={} durationMs={}",
        res.status().as_u16(),
        started.elapsed().as_millis()
    );
    res
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::yandex::SITE;

    const SEARCH_MASTER: &str = include_str!("../testdata/search-master-i-margarita.html");
    const SEARCH_DOZOR: &str = include_str!("../testdata/search-nochnoy-dozor.html");
    const SEARCH_EMPTY: &str = include_str!("../testdata/search-empty.html");
    const BOOK_MASTER: &str = include_str!("../testdata/book-MWxAop5b.html");
    const BOOK_DOZOR: &str = include_str!("../testdata/book-FVvJyn5e.html");
    const AUDIO_MASTER: &str = include_str!("../testdata/audiobook-VTVT8GzC.html");
    const COMIC_MASTER: &str = include_str!("../testdata/comicbook-yjrd9dHo.html");
    const EMPTY_PAGE: &str = "<html><body></body></html>";

    fn search_path(phrase: &str) -> String {
        search_url(phrase).strip_prefix(SITE).unwrap().to_string()
    }

    /// One canned answer per path, a delay per path, and a log of every request it saw.
    #[derive(Default)]
    struct Fake {
        pages: Mutex<HashMap<String, (u16, String)>>,
        delays: Mutex<HashMap<String, Duration>>,
        requested: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(pages: &[(&str, u16, &str)]) -> Arc<Self> {
            let fake = Fake::default();
            for (path, status, body) in pages {
                fake.pages
                    .lock()
                    .unwrap()
                    .insert(path.to_string(), (*status, body.to_string()));
            }
            Arc::new(fake)
        }

        fn requests(&self) -> Vec<String> {
            self.requested.lock().unwrap().clone()
        }
    }

    async fn fake_site(fake: Arc<Fake>) -> String {
        let app = Router::new().fallback(move |req: Request| {
            let fake = fake.clone();
            async move {
                let path = req.uri().path().to_string();
                fake.requested.lock().unwrap().push(path.clone());
                let delay = fake.delays.lock().unwrap().get(&path).copied();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let page = fake.pages.lock().unwrap().get(&path).cloned();
                match page {
                    Some((429, _)) => (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "42")],
                        "",
                    )
                        .into_response(),
                    Some((status, body)) => {
                        (StatusCode::from_u16(status).unwrap(), body).into_response()
                    }
                    None => (StatusCode::NOT_FOUND, "<h1>404</h1>").into_response(),
                }
            }
        });
        serve(app).await
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider_at(upstream: &str, token: Option<&str>, deadline: Duration) -> String {
        let cfg = Config {
            auth_token: token.map(str::to_string),
            deadline,
        };
        let client = Client::new(upstream, "test", 2, Duration::ZERO);
        serve(routes(Arc::new(cfg), Arc::new(client))).await
    }

    async fn provider(fake: Arc<Fake>) -> String {
        provider_at(&fake_site(fake).await, None, Duration::from_secs(5)).await
    }

    async fn get(
        base: &str,
        path: &str,
        auth: Option<&str>,
    ) -> (u16, serde_json::Value, HeaderMap) {
        let mut req = reqwest::Client::new().get(format!("{base}{path}"));
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body =
            serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    fn matches(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["matches"].as_array().expect("a matches list")
    }

    /// The Мастер и Маргарита search, whose first books all answer with the one fixture page.
    fn master_site() -> Arc<Fake> {
        let fake = Fake::with(&[(&search_path("Мастер и Маргарита"), 200, SEARCH_MASTER)]);
        for path in [
            "/books/MWxAop5b",
            "/books/TKUkSwtS",
            "/books/ds3Ah2rB",
            "/books/nvijlmLb",
            "/comicbooks/yjrd9dHo",
        ] {
            let page = if path.starts_with("/comic") {
                COMIC_MASTER
            } else {
                BOOK_MASTER
            };
            fake.pages
                .lock()
                .unwrap()
                .insert(path.into(), (200, page.into()));
        }
        fake
    }

    #[tokio::test]
    async fn info_declares_the_contract() {
        let base = provider(Fake::with(&[])).await;
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(status, 200);
        assert_eq!(
            (
                info["key"].as_str(),
                info["name"].as_str(),
                info["lookup"].as_bool(),
                info["authenticationRequired"].as_bool()
            ),
            (
                Some("yandexbooks"),
                Some("Yandex Books"),
                Some(true),
                Some(false)
            )
        );
        assert_eq!(info["idMaxLength"], 32);
        assert_eq!(info["searchParams"], json!(["limit"]));
        assert!(info.get("bookUrlTemplate").is_none());
        assert_eq!(info["color"], "#ff5d5d");
    }

    #[tokio::test]
    async fn search_reads_the_results_then_each_title_page() {
        let fake = master_site();
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&author=%D0%9C%D0%B8%D1%85%D0%B0%D0%B8%D0%BB%20%D0%91%D1%83%D0%BB%D0%B3%D0%B0%D0%BA%D0%BE%D0%B2&mediaType=book",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 3));
        assert!(body.get("partial").is_none());
        let book = &matches(&body)[0];
        assert_eq!(book["providerId"], "book-MWxAop5b");
        assert_eq!(book["mediaType"], "book");
        assert_eq!(book["title"], "Мастер и Маргарита");
        assert_eq!(book["authors"], json!(["Михаил Булгаков"]));
        assert_eq!(book["pages"], 249);
        assert_eq!(book["url"], "https://books.yandex.ru/books/MWxAop5b");
        // The search page first, then the first three books by the author with the exact title,
        // two at a time, so in either order.
        let mut requests = fake.requests();
        assert_eq!(requests.remove(0), search_path("Мастер и Маргарита"));
        requests.sort();
        assert_eq!(
            requests,
            ["/books/MWxAop5b", "/books/TKUkSwtS", "/books/ds3Ah2rB"]
        );
    }

    #[tokio::test]
    async fn a_comic_is_found_by_its_own_title() {
        let fake = master_site();
        fake.pages.lock().unwrap().insert(
            search_path("Мастер и Маргарита. Графический роман"),
            (200, SEARCH_MASTER.into()),
        );
        let base = provider(fake).await;
        let (_, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0.%20%D0%93%D1%80%D0%B0%D1%84%D0%B8%D1%87%D0%B5%D1%81%D0%BA%D0%B8%D0%B9%20%D1%80%D0%BE%D0%BC%D0%B0%D0%BD&limit=1",
            None,
        )
        .await;
        let comic = &matches(&body)[0];
        assert_eq!(
            (comic["providerId"].as_str(), comic["mediaType"].as_str()),
            (Some("comicbook-yjrd9dHo"), Some("comic"))
        );
        assert_eq!(comic["pages"], 146);
    }

    #[tokio::test]
    async fn an_audiobook_search_reads_audiobook_pages() {
        let fake = Fake::with(&[
            (&search_path("Мастер и Маргарита"), 200, SEARCH_MASTER),
            ("/audiobooks/VTVT8GzC", 200, AUDIO_MASTER),
        ]);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&mediaType=audiobook&limit=1",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 1));
        let audio = &matches(&body)[0];
        assert_eq!(audio["providerId"], "audiobook-VTVT8GzC");
        assert_eq!(audio["mediaType"], "audiobook");
        assert_eq!(audio["duration"], 64914);
        assert_eq!(audio["narrators"][0], "Максим Суханов");
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn search_respects_the_limit() {
        let fake = master_site();
        let base = provider(fake.clone()).await;
        let (_, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&limit=2",
            None,
        )
        .await;
        assert_eq!(matches(&body).len(), 2);
        assert_eq!(fake.requests().len(), 3);
        // A limit above three still fetches three pages.
        let (_, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&author=%D0%91%D1%83%D0%BB%D0%B3%D0%B0%D0%BA%D0%BE%D0%B2&limit=50",
            None,
        )
        .await;
        assert_eq!(matches(&body).len(), 3);
        assert_eq!(fake.requests().len(), 7);
    }

    #[tokio::test]
    async fn a_series_book_is_numbered() {
        let fake = Fake::with(&[
            (&search_path("Ночной дозор"), 200, SEARCH_DOZOR),
            ("/books/FVvJyn5e", 200, BOOK_DOZOR),
        ]);
        let base = provider(fake).await;
        let (_, body, _) = get(
            &base,
            "/search?query=%D0%9D%D0%BE%D1%87%D0%BD%D0%BE%D0%B9%20%D0%B4%D0%BE%D0%B7%D0%BE%D1%80&author=%D0%A1%D0%B5%D1%80%D0%B3%D0%B5%D0%B9%20%D0%9B%D1%83%D0%BA%D1%8C%D1%8F%D0%BD%D0%B5%D0%BD%D0%BA%D0%BE&limit=1",
            None,
        )
        .await;
        assert_eq!(
            matches(&body)[0]["series"],
            json!([{ "series": "Дозоры", "sequence": "1" }])
        );
        assert_eq!(matches(&body)[0]["publishedYear"], "2022");
    }

    #[tokio::test]
    async fn no_results_is_an_empty_answer() {
        let fake = Fake::with(&[(&search_path("qwxz"), 200, SEARCH_EMPTY)]);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?query=qwxz", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/search", None).await.0, 400);
        assert_eq!(get(&base, "/search?query=%20&author=x", None).await.0, 400);
        let (status, body, _) = get(&base, "/search?isbn=9785170123456", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn unreadable_title_pages_fail_the_search_instead_of_answering_nothing() {
        let fake = Fake::with(&[
            (&search_path("Мастер и Маргарита"), 200, SEARCH_MASTER),
            ("/books/MWxAop5b", 200, EMPTY_PAGE),
        ]);
        let base = provider(fake).await;
        let (status, _, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&limit=1",
            None,
        )
        .await;
        assert_eq!(status, 502);
    }

    #[tokio::test]
    async fn a_skipped_title_marks_the_answer_partial() {
        let fake = master_site();
        fake.pages
            .lock()
            .unwrap()
            .insert("/books/TKUkSwtS".into(), (503, String::new()));
        let base = provider(fake).await;
        let (status, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&limit=2",
            None,
        )
        .await;
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
    }

    #[tokio::test]
    async fn search_page_failure_is_a_bad_gateway() {
        let fake = Fake::with(&[(&search_path("x"), 503, "")]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
        let base = provider(Fake::with(&[])).await;
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn a_slow_title_is_left_out_when_the_deadline_passes() {
        let fake = master_site();
        fake.delays
            .lock()
            .unwrap()
            .insert("/books/TKUkSwtS".into(), Duration::from_secs(3));
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(700)).await;
        let started = Instant::now();
        let (status, body, _) = get(
            &base,
            "/search?query=%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0&limit=2",
            None,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
        assert_eq!(matches(&body)[0]["providerId"], "book-MWxAop5b");
    }

    #[tokio::test]
    async fn lookup_by_id_and_by_pasted_link() {
        let fake = Fake::with(&[
            ("/books/MWxAop5b", 200, BOOK_MASTER),
            ("/audiobooks/VTVT8GzC", 200, AUDIO_MASTER),
        ]);
        let base = provider(fake.clone()).await;
        for (id, want) in [
            ("book-MWxAop5b", "book-MWxAop5b"),
            ("audiobook-VTVT8GzC", "audiobook-VTVT8GzC"),
            (
                "https%3A%2F%2Fbooks.yandex.ru%2Fbooks%2FMWxAop5b",
                "book-MWxAop5b",
            ),
        ] {
            let (status, book, _) = get(&base, &format!("/book/{id}"), None).await;
            assert_eq!(
                (status, book["providerId"].as_str()),
                (200, Some(want)),
                "{id}"
            );
        }
        assert_eq!(
            fake.requests(),
            ["/books/MWxAop5b", "/audiobooks/VTVT8GzC", "/books/MWxAop5b"]
        );
    }

    #[tokio::test]
    async fn lookup_answers_404_for_unknown_and_invalid_ids() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/book/book-zzzzzzzz", None).await.0, 404);
        let asked = fake.requests().len();
        let long = format!(
            "https%3A%2F%2Fbooks.yandex.ru%2Fbooks%2FMWxAop5b%3F{}",
            "a".repeat(600)
        );
        for id in [
            "MWxAop5b",
            "..",
            "conformance-unknown-id",
            "book-../x",
            long.as_str(),
        ] {
            assert_eq!(
                get(&base, &format!("/book/{id}"), None).await.0,
                404,
                "{id}"
            );
        }
        assert_eq!(fake.requests().len(), asked);
    }

    #[tokio::test]
    async fn lookup_of_an_unreadable_page_is_a_bad_gateway() {
        let fake = Fake::with(&[
            ("/books/aaaaaaaa", 200, EMPTY_PAGE),
            ("/books/bbbbbbbb", 503, ""),
        ]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/book/book-aaaaaaaa", None).await.0, 502);
        assert_eq!(get(&base, "/book/book-bbbbbbbb", None).await.0, 502);
    }

    #[tokio::test]
    async fn network_failure_is_a_bad_gateway() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let base = provider_at(&closed, None, Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/book/book-MWxAop5b", None).await.0, 502);
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn upstream_rate_limit_is_passed_on_and_held() {
        let fake = Fake::with(&[("/books/MWxAop5b", 429, "")]);
        let base = provider(fake.clone()).await;
        let (status, _, headers) = get(&base, "/book/book-MWxAop5b", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        // The pause holds: the next call is refused without asking Yandex again.
        let (status, _, headers) = get(&base, "/search?query=x", None).await;
        let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert_eq!(status, 429);
        assert!((40..=42).contains(&retry_after), "{retry_after}");
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_captcha_is_a_rate_limit_and_is_not_answered() {
        let captcha = r#"<html><body><form action="/checkcaptcha"><div class="SmartCaptcha-Anchor"></div></form></body></html>"#;
        let fake = Fake::with(&[("/books/MWxAop5b", 200, captcha)]);
        let base = provider(fake.clone()).await;
        let (status, _, headers) = get(&base, "/book/book-MWxAop5b", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("300"))
        );
        assert_eq!(get(&base, "/search?query=x", None).await.0, 429);
        assert_eq!(fake.requests(), ["/books/MWxAop5b"]);
    }

    #[tokio::test]
    async fn auth_token() {
        let fake = Fake::with(&[("/books/MWxAop5b", 200, BOOK_MASTER)]);
        let upstream = fake_site(fake).await;
        let base = provider_at(&upstream, Some("Bearer s3cret"), Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/book/book-MWxAop5b", None).await.0, 401);
        assert_eq!(
            get(&base, "/search?query=x", Some("Bearer nope")).await.0,
            401
        );
        assert_eq!(
            get(&base, "/book/book-MWxAop5b", Some("Bearer s3cret"))
                .await
                .0,
            200
        );
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (200, Some(true))
        );
    }
}
