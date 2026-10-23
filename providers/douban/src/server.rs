use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use crate::douban::{Client, DoubanError, Suggestion};
use crate::mapping::{author_matches, map_subject, normalize_isbn, BookMetadata};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);
const MAX_ID_LENGTH: usize = 10;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    db: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: some matches could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, db: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, db })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "Douban",
        "version": VERSION,
        // Douban's app icon from its own global navigation. The img*.doubanio.com copies refuse
        // any request without a Douban referer; this host serves it to anyone.
        "avatarUrl": "https://asset.doubanio.com/cuphead/nav/2.0.0/shire/assets/nav_doubanapp_6@2x.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "douban",
        "mediaTypes": ["book"],
        "lookup": true,
        // No `cover`, whose images Douban serves only to its own pages, and no `seriesName`:
        // Douban's 丛书 is a publisher's imprint line, not story order.
        "supportedFields": [
            "title", "subtitle", "description", "authors", "publisher", "isbn", "publishedYear",
            "language", "pageCount", "communityRating"
        ],
        "searchParams": ["isbn", "limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "bookUrlTemplate": "https://book.douban.com/subject/{id}/",
        "coverShape": "portrait",
        // The green Douban's global navigation stylesheet (cuphead/nav/2.0.0/shire/bundle.css)
        // gives its app link, the same green as the icon.
        "color": "#13bc39",
        // One search is a suggestion request and a page per match, so one search at a time.
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": state.cfg.request_interval.as_millis() as u64 },
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
    let (query, author) = (param("query"), param("author"));
    let isbn = normalize_isbn(param("isbn"));

    if param("mediaType") == "audiobook" {
        return Json(SearchResponse {
            matches: Vec::new(),
            partial: false,
        })
        .into_response();
    }
    if query.is_empty() && isbn.is_empty() {
        return error(StatusCode::BAD_REQUEST, "query is required");
    }

    let limit = parse_limit(param("limit"));
    let answer = tokio::time::timeout(SEARCH_TIMEOUT, async {
        if !isbn.is_empty() {
            let found = state.db.subject_by_isbn(&isbn).await?;
            if found.is_some() || query.is_empty() {
                return Ok(SearchResponse {
                    matches: found.iter().map(map_subject).collect(),
                    partial: false,
                });
            }
        }
        search_by_text(&state, query, author, limit).await
    })
    .await
    .unwrap_or_else(|_| Err(DoubanError::Failed("timed out".into())));

    match answer {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error(err),
    }
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    if raw.is_empty() || raw.len() > MAX_ID_LENGTH || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return error(StatusCode::NOT_FOUND, "Not found");
    }

    let result = tokio::time::timeout(SEARCH_TIMEOUT, state.db.subject(&raw))
        .await
        .unwrap_or_else(|_| Err(DoubanError::Failed("timed out".into())));
    match result {
        Ok(subject) => Json(map_subject(&subject)).into_response(),
        Err(err) => upstream_error(err),
    }
}

async fn search_by_text(
    state: &AppState,
    query: &str,
    author: &str,
    limit: usize,
) -> Result<SearchResponse, DoubanError> {
    let suggestions = state.db.suggest(query).await?;
    let ids = pick_subjects(&suggestions, author, limit);

    let mut tasks = JoinSet::new();
    for (index, id) in ids.iter().cloned().enumerate() {
        let db = state.db.clone();
        tasks.spawn(async move { (index, db.subject(&id).await) });
    }
    let mut results: Vec<Option<Result<BookMetadata, DoubanError>>> =
        (0..ids.len()).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        let (index, result) = joined.map_err(|error| DoubanError::Failed(error.to_string()))?;
        results[index] = Some(result.map(|subject| map_subject(&subject)));
    }

    let mut matches = Vec::new();
    let mut failure: Option<DoubanError> = None;
    for result in results.into_iter().flatten() {
        match result {
            Ok(book) => matches.push(book),
            Err(DoubanError::NotFound) => {}
            // A challenge outranks a plain failure, so the caller backs off.
            Err(err) => {
                let outranks = match &failure {
                    None => true,
                    Some(DoubanError::Failed(_)) => !matches!(err, DoubanError::Failed(_)),
                    Some(_) => false,
                };
                if outranks {
                    failure = Some(err);
                }
            }
        }
    }
    match failure {
        Some(err) if matches.is_empty() => Err(err),
        failure => Ok(SearchResponse {
            matches,
            partial: failure.is_some(),
        }),
    }
}

/// Keeps Douban's order, which puts the best-known edition first, but lifts books by the
/// requested author to the front.
fn pick_subjects(suggestions: &[Suggestion], author: &str, limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut books: Vec<&Suggestion> = suggestions
        .iter()
        .filter(|s| {
            s.kind == "b"
                && !s.id.is_empty()
                && s.id.len() <= MAX_ID_LENGTH
                && s.id.bytes().all(|b| b.is_ascii_digit())
                && seen.insert(s.id.clone())
        })
        .collect();
    books.sort_by_key(|s| !author_matches(s, author));
    books
        .into_iter()
        .take(limit)
        .map(|s| s.id.clone())
        .collect()
}

fn parse_limit(raw: &str) -> usize {
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => (n as usize).min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

/// Status codes are part of the contract: a client backs off on 429 and treats 5xx as a failure,
/// never as "no results".
fn upstream_error(err: DoubanError) -> Response {
    match err {
        DoubanError::RateLimited {
            retry_after_seconds,
        } => retry_later(retry_after_seconds, "douban is rate limiting this provider"),
        DoubanError::Blocked {
            retry_after_seconds,
        } => retry_later(
            retry_after_seconds,
            "douban is challenging this provider's requests",
        ),
        DoubanError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        DoubanError::Failed(message) => {
            eprintln!("[upstream] [fail] error={message:?} - douban request failed");
            error(StatusCode::BAD_GATEWAY, "douban is unavailable")
        }
    }
}

fn retry_later(seconds: u64, message: &str) -> Response {
    let mut res = error(StatusCode::TOO_MANY_REQUESTS, message);
    res.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    res
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

    use super::*;

    /// How the fake Douban answers every request: normally, or the way Douban refuses one.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Normal,
        Status(u16),
        SecRedirect,
        ChallengePage,
    }

    /// Serves the captured fixtures, records every path it was asked for with its cookie, and
    /// can be told to answer like a Douban that is refusing the provider.
    struct Fake {
        requested: Mutex<Vec<(String, String)>>,
        mode: Mutex<Mode>,
        failing_subject: Mutex<Option<&'static str>>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requested: Mutex::new(Vec::new()),
                mode: Mutex::new(Mode::Normal),
                failing_subject: Mutex::new(None),
            })
        }

        fn set(&self, mode: Mode) {
            *self.mode.lock().unwrap() = mode;
        }

        fn asked(&self, path: &str) -> bool {
            self.requested
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _)| p == path)
        }

        fn count(&self) -> usize {
            self.requested.lock().unwrap().len()
        }
    }

    const ISBN_REDIRECT: &str = include_str!("../testdata/isbn-9787544253994.txt");

    fn subject_fixture(id: &str) -> Option<&'static str> {
        Some(match id {
            "2567698" => include_str!("../testdata/subject-2567698.html"),
            "3066477" => include_str!("../testdata/subject-3066477.html"),
            "5363767" => include_str!("../testdata/subject-5363767.html"),
            "6082808" => include_str!("../testdata/subject-6082808.html"),
            "25985021" => include_str!("../testdata/subject-25985021.html"),
            "25841131" => include_str!("../testdata/subject-25841131.html"),
            _ => return None,
        })
    }

    /// The captured redirect: a status line, a location header, then the body.
    fn isbn_redirect() -> Response {
        let (head, body) = ISBN_REDIRECT.split_once("\n\n").unwrap();
        let location = head
            .lines()
            .find_map(|line| line.strip_prefix("location: "))
            .unwrap();
        (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, location.to_string())],
            body.to_string(),
        )
            .into_response()
    }

    fn not_found() -> Response {
        (
            StatusCode::NOT_FOUND,
            include_str!("../testdata/subject-not-found.html"),
        )
            .into_response()
    }

    async fn fake_douban(fake: Arc<Fake>) -> String {
        let app = Router::new().fallback(move |req: Request| {
            let fake = fake.clone();
            async move {
                let path = req.uri().path().to_string();
                let cookie = req
                    .headers()
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                fake.requested.lock().unwrap().push((path.clone(), cookie));
                let mode = *fake.mode.lock().unwrap();
                match mode {
                    Mode::Normal => {}
                    Mode::Status(429) => {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, "42")],
                            "",
                        )
                            .into_response()
                    }
                    Mode::Status(code) => {
                        return StatusCode::from_u16(code).unwrap().into_response()
                    }
                    Mode::SecRedirect => {
                        return (
                            StatusCode::FOUND,
                            [(header::LOCATION, "https://sec.douban.com/c?r=https%3A%2F%2Fbook.douban.com%2F")],
                            "",
                        )
                            .into_response()
                    }
                    Mode::ChallengePage => {
                        return (
                            StatusCode::OK,
                            "<html><head><title>禁止访问</title></head><body>检测到有异常请求从你的 IP 发出</body></html>",
                        )
                            .into_response()
                    }
                }
                if let Some(id) = path
                    .strip_prefix("/subject/")
                    .map(|rest| rest.trim_end_matches('/'))
                {
                    if *fake.failing_subject.lock().unwrap() == Some(id) {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    return match subject_fixture(id) {
                        Some(page) => (StatusCode::OK, page).into_response(),
                        None => not_found(),
                    };
                }
                match path.as_str() {
                    "/j/subject_suggest" => {
                        let q: Query<HashMap<String, String>> =
                            Query::try_from_uri(req.uri()).unwrap();
                        let body = match q.get("q").map(String::as_str) {
                            Some("三体") => include_str!("../testdata/suggest-santi.json"),
                            Some("人类简史") => include_str!("../testdata/suggest-renlei.json"),
                            _ => "[]",
                        };
                        (StatusCode::OK, body).into_response()
                    }
                    "/isbn/9787544253994/" => isbn_redirect(),
                    _ => not_found(),
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

    async fn provider(fake: Arc<Fake>, token: Option<&str>, cookie: Option<&str>) -> String {
        let upstream = fake_douban(fake).await;
        let cfg = Config {
            auth_token: token.map(str::to_string),
            request_interval: Duration::from_millis(500),
        };
        let client = Client::new(&upstream, "test", cookie, 2, Duration::ZERO);
        serve(routes(Arc::new(cfg), Arc::new(client))).await
    }

    async fn get(base: &str, path: &str, auth: Option<&str>) -> (u16, String, HeaderMap) {
        let mut req = reqwest::Client::new().get(format!("{base}{path}"));
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        (status, res.text().await.unwrap(), headers)
    }

    fn search_body(body: &str) -> SearchResponse {
        serde_json::from_str(body).expect("search answer parses")
    }

    const SANTI: &str = "/search?query=%E4%B8%89%E4%BD%93&mediaType=book";

    #[tokio::test]
    async fn info_declares_the_contract() {
        let base = provider(Fake::new(), None, None).await;
        let (status, body, _) = get(&base, "/provider/info", None).await;
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            (
                info["key"].as_str(),
                info["lookup"].as_bool(),
                info["authenticationRequired"].as_bool()
            ),
            (Some("douban"), Some(true), Some(false))
        );
        assert_eq!(info["mediaTypes"], json!(["book"]));
        assert_eq!(info["searchParams"], json!(["isbn", "limit"]));
        let fields = info["supportedFields"].as_array().unwrap();
        assert!(!fields.contains(&json!("seriesName")) && !fields.contains(&json!("cover")));
        assert_eq!(
            info["bookUrlTemplate"],
            "https://book.douban.com/subject/{id}/"
        );
        assert_eq!(
            info["rateLimit"],
            json!({ "maxConcurrency": 1, "minIntervalMs": 500 })
        );
        assert_eq!(
            (info["avatarUrl"].as_str(), info["color"].as_str()),
            (
                Some("https://asset.doubanio.com/cuphead/nav/2.0.0/shire/assets/nav_doubanapp_6@2x.png"),
                Some("#13bc39")
            )
        );
    }

    #[tokio::test]
    async fn search_returns_each_suggestion_as_a_full_record() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        let (status, body, _) = get(&base, SANTI, None).await;
        let body = search_body(&body);
        assert_eq!((status, body.partial), (200, false));
        let ids: Vec<&str> = body
            .matches
            .iter()
            .map(|m| m.provider_id.as_str())
            .collect();
        assert_eq!(ids, ["2567698", "3066477", "5363767"]);
        let second = &body.matches[1];
        assert_eq!(
            (second.title.as_str(), second.subtitle.as_str()),
            ("三体Ⅱ", "黑暗森林")
        );
        assert_eq!(
            (
                second.isbn13.as_str(),
                second.pages,
                second.authors.as_slice()
            ),
            ("9787536693968", 470, ["刘慈欣".to_string()].as_slice())
        );
        assert!(fake.asked("/j/subject_suggest"));
    }

    #[tokio::test]
    async fn search_honours_the_limit() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        let body = search_body(&get(&base, &format!("{SANTI}&limit=2"), None).await.1);
        assert_eq!(body.matches.len(), 2);
        assert!(!fake.asked("/subject/5363767/"), "fetched past the limit");
    }

    #[tokio::test]
    async fn search_by_isbn_follows_douban_to_the_subject() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        let body = search_body(&get(&base, "/search?isbn=978-7-5442-5399-4", None).await.1);
        assert_eq!(body.matches.len(), 1);
        let book = &body.matches[0];
        assert_eq!(
            (
                book.provider_id.as_str(),
                book.title.as_str(),
                book.isbn13.as_str()
            ),
            ("6082808", "百年孤独", "9787544253994")
        );
        assert_eq!(book.authors, ["加西亚·马尔克斯"]);
        assert!(!fake.asked("/j/subject_suggest"));
    }

    #[tokio::test]
    async fn an_unknown_isbn_falls_back_to_the_title() {
        let base = provider(Fake::new(), None, None).await;
        let (status, body, _) = get(&base, "/search?isbn=9781948371629", None).await;
        assert_eq!((status, search_body(&body).matches.len()), (200, 0));
        let body = search_body(
            &get(&base, &format!("{SANTI}&isbn=9781948371629"), None)
                .await
                .1,
        );
        assert_eq!(body.matches.len(), 3);
    }

    #[tokio::test]
    async fn search_answers_audiobooks_with_nothing_and_needs_a_query() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        let (status, body, _) = get(&base, "/search?query=x&mediaType=audiobook", None).await;
        assert_eq!((status, search_body(&body).matches.len()), (200, 0));
        assert_eq!(fake.count(), 0);
        assert_eq!(get(&base, "/search", None).await.0, 400);
        assert_eq!(get(&base, "/search?isbn=12345", None).await.0, 400);
        let (status, body, _) = get(&base, "/search?query=nothing", None).await;
        assert_eq!((status, search_body(&body).matches.len()), (200, 0));
    }

    #[tokio::test]
    async fn lookup() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        let (status, body, _) = get(&base, "/book/25985021", None).await;
        let book: BookMetadata = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (status, book.provider_id.as_str(), book.title.as_str()),
            (200, "25985021", "人类简史")
        );
        assert_eq!(get(&base, "/book/999999999", None).await.0, 404);
        let before = fake.count();
        for missing in [
            "/book/abc",
            "/book/12345678901",
            "/book/conformance-unknown-id",
        ] {
            assert_eq!(get(&base, missing, None).await.0, 404, "{missing}");
        }
        assert_eq!(
            fake.count(),
            before,
            "asked douban about an id it never issues"
        );
    }

    #[tokio::test]
    async fn a_challenge_answers_429_and_pauses_the_provider() {
        for mode in [
            Mode::SecRedirect,
            Mode::ChallengePage,
            Mode::Status(403),
            Mode::Status(418),
        ] {
            let fake = Fake::new();
            let base = provider(fake.clone(), None, None).await;
            fake.set(mode);
            let (status, _, headers) = get(&base, SANTI, None).await;
            let retry_after: u64 = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            assert_eq!(status, 429);
            assert!(retry_after > 500, "retry after {retry_after}");

            // Douban is fine again, but the provider keeps its distance until the pause ends.
            fake.set(Mode::Normal);
            let asked = fake.count();
            assert_eq!(get(&base, "/book/2567698", None).await.0, 429);
            assert_eq!(fake.count(), asked, "asked douban during the pause");
        }
    }

    #[tokio::test]
    async fn a_challenge_on_a_lookup_or_an_isbn_is_a_429_too() {
        for path in ["/book/2567698", "/search?isbn=9787544253994"] {
            let fake = Fake::new();
            let base = provider(fake.clone(), None, None).await;
            fake.set(Mode::SecRedirect);
            assert_eq!(get(&base, path, None).await.0, 429, "{path}");
            assert_eq!(fake.count(), 1, "{path} followed the challenge");
        }
    }

    #[tokio::test]
    async fn upstream_rate_limit_is_passed_on() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        fake.set(Mode::Status(429));
        let (status, _, headers) = get(&base, SANTI, None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
    }

    #[tokio::test]
    async fn upstream_failure_is_a_bad_gateway() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, None).await;
        fake.set(Mode::Status(500));
        assert_eq!(get(&base, SANTI, None).await.0, 502);
        assert_eq!(get(&base, "/book/2567698", None).await.0, 502);
    }

    #[tokio::test]
    async fn search_marks_answer_partial_when_a_page_fails() {
        let fake = Fake::new();
        *fake.failing_subject.lock().unwrap() = Some("3066477");
        let base = provider(fake, None, None).await;
        let body = search_body(&get(&base, SANTI, None).await.1);
        let ids: Vec<&str> = body
            .matches
            .iter()
            .map(|m| m.provider_id.as_str())
            .collect();
        assert_eq!((ids, body.partial), (vec!["2567698", "5363767"], true));
    }

    #[tokio::test]
    async fn sends_the_configured_cookie() {
        let fake = Fake::new();
        let base = provider(fake.clone(), None, Some("bid=abc; dbcl2=\"1:x\"")).await;
        assert_eq!(get(&base, "/book/2567698", None).await.0, 200);
        let requested = fake.requested.lock().unwrap();
        assert_eq!(requested[0].1, "bid=abc; dbcl2=\"1:x\"");
    }

    #[tokio::test]
    async fn auth_token() {
        let base = provider(Fake::new(), Some("Bearer s3cret"), None).await;
        assert_eq!(get(&base, "/book/2567698", None).await.0, 401);
        assert_eq!(get(&base, SANTI, None).await.0, 401);
        assert_eq!(
            get(&base, "/book/2567698", Some("Bearer s3cret")).await.0,
            200
        );
        let (status, body, _) = get(&base, "/provider/info", None).await;
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (200, Some(true))
        );
    }

    #[tokio::test]
    async fn a_long_queue_turns_into_a_429() {
        let client = Client::new(
            "http://127.0.0.1:9",
            "test",
            None,
            2,
            Duration::from_secs(1),
        );
        client.push_back(Duration::from_secs(20));
        assert!(matches!(
            client.suggest("x").await,
            Err(DoubanError::RateLimited { .. })
        ));
    }
}
