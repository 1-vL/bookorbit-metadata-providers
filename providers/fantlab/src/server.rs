use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use crate::fantlab::{Client, FantlabError, Work, WorkHit};
use crate::mapping::{list_contains_isbn, map_edition, map_work, normalize_isbn, BookMetadata};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(25);

/// Work types that are never a book on someone's shelf. A deny list rather than an allow list, so
/// a type fantlab adds later is offered instead of silently disappearing. `other` is where fantlab
/// files appendices, film-production papers and press statements.
const SKIPPED_WORK_TYPES: &[&str] = &[
    "review",
    "article",
    "essay",
    "interview",
    "cycle",
    "scenario",
    "poem",
    "excerpt",
    "magazine",
    "microstory",
    "other",
];

static BOOK_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(e?)(\d{1,10})$").expect("valid pattern"));

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    fl: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: some matches could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, fl: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, fl })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "FantLab",
        "version": VERSION,
        "avatarUrl": format!("{}/img/fl_apple.png", state.cfg.site_base),
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "fantlab",
        "mediaTypes": ["book"],
        "lookup": true,
        "idMaxLength": 11,
        "searchParams": ["query", "author", "isbn", "language", "limit", "mediaType"],
        // Publisher, ISBN and pages come from an edition, so only an ISBN search fills them.
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "isbn", "publishedYear",
            "language", "pageCount", "communityRating", "seriesName", "seriesIndex", "genres"
        ],
        "rateLimit": { "maxConcurrency": 2, "minIntervalMs": 250 },
        // The blue fantlab.ru's own stylesheet uses most.
        "color": "#2c98cd",
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
    let language = primary_language(param("language"));

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
            let book = search_by_isbn(&state, &isbn).await?;
            if book.is_some() || query.is_empty() {
                return Ok(SearchResponse {
                    matches: book.into_iter().collect(),
                    partial: false,
                });
            }
        }
        search_by_text(&state, query, author, &language, limit).await
    })
    .await
    .unwrap_or_else(|_| Err(FantlabError::Unavailable("timed out".into())));

    match answer {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error(err),
    }
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let Some(captures) = BOOK_ID.captures(&raw) else {
        return error(StatusCode::NOT_FOUND, "Not found");
    };
    let id: i64 = captures[2].parse().unwrap_or_default();
    let is_edition = &captures[1] == "e";
    let language = primary_language(q.get("language").map(String::as_str).unwrap_or_default());

    let result = tokio::time::timeout(SEARCH_TIMEOUT, async {
        if is_edition {
            lookup_edition(&state, id, "").await
        } else {
            let work = state.fl.work(id).await?;
            let book = map_work(&work, &state.cfg.site_base, &language);
            Ok(number_in_cycle(&state, &work, book).await)
        }
    })
    .await
    .unwrap_or_else(|_| Err(FantlabError::Unavailable("timed out".into())));

    match result {
        Ok(book) if !book.title.is_empty() => Json(book).into_response(),
        Ok(_) | Err(FantlabError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
        Err(err) => upstream_error(err),
    }
}

async fn search_by_isbn(
    state: &AppState,
    isbn: &str,
) -> Result<Option<BookMetadata>, FantlabError> {
    for hit in state.fl.search_editions(isbn).await? {
        let listed: Vec<&str> = hit.isbn.split(',').collect();
        if list_contains_isbn(&listed, isbn) {
            match lookup_edition(state, hit.edition_id, isbn).await {
                Err(FantlabError::NotFound) => continue,
                result => return result.map(Some),
            }
        }
    }
    Ok(None)
}

async fn lookup_edition(
    state: &AppState,
    id: i64,
    preferred_isbn: &str,
) -> Result<BookMetadata, FantlabError> {
    let edition = state.fl.edition(id).await?;
    let work = match edition.work_ids.as_slice() {
        [only] => match state.fl.work(*only).await {
            Ok(work) => Some(work),
            Err(FantlabError::NotFound) => None,
            Err(err) => return Err(err),
        },
        _ => None,
    };
    let book = map_edition(
        &edition,
        work.as_ref(),
        &state.cfg.site_base,
        preferred_isbn,
    );
    Ok(match work {
        Some(work) => number_in_cycle(state, &work, book).await,
        None => book,
    })
}

/// Numbers the book in the nearest cycle whose main line it is on, which then names the series.
/// Failing to read a cycle costs the number only.
async fn number_in_cycle(state: &AppState, work: &Work, mut book: BookMetadata) -> BookMetadata {
    let Some(series) = book.series.first_mut() else {
        return book;
    };
    for cycle in &work.first_chain {
        if let Ok(Some(position)) = state.fl.cycle_position(cycle.id, work.id).await {
            series.series = cycle.name.clone();
            series.sequence = position.to_string();
            break;
        }
    }
    book
}

async fn search_by_text(
    state: &AppState,
    query: &str,
    author: &str,
    language: &str,
    limit: usize,
) -> Result<SearchResponse, FantlabError> {
    let title = searchable_title(query);
    let mut hits = state
        .fl
        .search_works(format!("{title} {author}").trim())
        .await?;
    if hits.is_empty() && !author.is_empty() {
        // The author may be spelled in another script than fantlab stores; the title alone still
        // finds the work, and the client ranks the candidates itself.
        hits = state.fl.search_works(title).await?;
    }

    let ids = pick_works(&hits, author, limit);
    let mut tasks = JoinSet::new();
    for (index, id) in ids.iter().copied().enumerate() {
        let state = state.clone();
        let language = language.to_string();
        tasks.spawn(async move {
            let result = match state.fl.work(id).await {
                Ok(work) => {
                    let book = map_work(&work, &state.cfg.site_base, &language);
                    Ok(number_in_cycle(&state, &work, book).await)
                }
                Err(err) => Err(err),
            };
            (index, result)
        });
    }
    let mut results: Vec<Option<Result<BookMetadata, FantlabError>>> =
        (0..ids.len()).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        let (index, result) =
            joined.map_err(|error| FantlabError::Unavailable(error.to_string()))?;
        results[index] = Some(result);
    }

    let mut matches = Vec::new();
    let mut failure = None;
    for result in results.into_iter().flatten() {
        match result {
            Ok(book) if !book.title.is_empty() => matches.push(book),
            Ok(_) | Err(FantlabError::NotFound) => {}
            Err(err) => {
                failure.get_or_insert(err);
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

/// Keeps fantlab's relevance order, but lifts works by the requested author to the front, since
/// fantlab ranks a famous review of a novel above a little-known novel's own record.
fn pick_works(hits: &[WorkHit], author: &str, limit: usize) -> Vec<i64> {
    let needle = last_word(author).to_lowercase();
    let (mut by_author, mut rest) = (Vec::new(), Vec::new());
    let mut seen = HashSet::new();
    for hit in hits {
        if hit.work_id == 0
            || SKIPPED_WORK_TYPES.contains(&hit.type_slug.as_str())
            || !seen.insert(hit.work_id)
        {
            continue;
        }
        let names = format!("{} {}", hit.author_names, hit.author_ru_names).to_lowercase();
        if !needle.is_empty() && names.contains(&needle) {
            by_author.push(hit.work_id);
        } else {
            rest.push(hit.work_id);
        }
    }
    by_author.extend(rest);
    by_author.truncate(limit);
    by_author
}

/// Drops a subtitle or a bracketed series note: fantlab's full-text search needs every word to
/// match, so any extra word in a long title returns nothing.
fn searchable_title(title: &str) -> &str {
    match title.find([':', '(', '[']) {
        Some(index) if index > 0 => title[..index].trim(),
        _ => title.trim(),
    }
}

fn last_word(s: &str) -> &str {
    s.split_whitespace()
        .last()
        .map(|word| word.trim_matches(['.', ',']))
        .unwrap_or_default()
}

fn primary_language(tag: &str) -> String {
    let tag = tag.trim().to_lowercase();
    let tag = match tag.find(['-', '_']) {
        Some(index) if index > 0 => &tag[..index],
        _ => tag.as_str(),
    };
    if tag.len() == 2 {
        tag.to_string()
    } else {
        String::new()
    }
}

fn parse_limit(raw: &str) -> usize {
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => (n as usize).min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

fn upstream_error(err: FantlabError) -> Response {
    match err {
        FantlabError::RateLimited { retry_after } => {
            let mut res = error(
                StatusCode::TOO_MANY_REQUESTS,
                "fantlab.ru is rate limiting this provider",
            );
            if let Ok(value) = HeaderValue::from_str(&retry_after) {
                res.headers_mut().insert(header::RETRY_AFTER, value);
            }
            res
        }
        FantlabError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        FantlabError::Unavailable(message) => {
            eprintln!("[upstream] [fail] error={message:?} - fantlab request failed");
            error(StatusCode::BAD_GATEWAY, "fantlab.ru is unavailable")
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
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Mutex;

    use super::*;

    const SITE: &str = "https://fantlab.ru";

    /// Serves the captured fixtures, records every path it was asked for, and can be told to
    /// fail: `fail_status` for every request, `fail_works_except` for every work but one.
    #[derive(Default)]
    struct Fake {
        requested: Mutex<Vec<String>>,
        fail_status: AtomicU16,
        fail_works_except: Mutex<Option<&'static str>>,
    }

    impl Fake {
        fn asked(&self, path: &str) -> bool {
            self.requested.lock().unwrap().iter().any(|p| p == path)
        }
    }

    async fn fake_fantlab(fake: Arc<Fake>) -> String {
        let app = Router::new().fallback(move |req: Request| {
            let fake = fake.clone();
            async move {
                let path = req.uri().path().to_string();
                fake.requested.lock().unwrap().push(path.clone());
                match fake.fail_status.load(Ordering::SeqCst) {
                    0 => {}
                    429 => {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(header::RETRY_AFTER, "30")],
                            "",
                        )
                            .into_response()
                    }
                    code => return StatusCode::from_u16(code).unwrap().into_response(),
                }
                if let Some(kept) = *fake.fail_works_except.lock().unwrap() {
                    if path.starts_with("/work/") && path != kept {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
                let body = match path.as_str() {
                    "/search-works" => include_str!("../testdata/search-works-solaris.json"),
                    "/search-editions" => {
                        include_str!("../testdata/search-editions-9785170254767.json")
                    }
                    "/work/3104/extended" => include_str!("../testdata/work-3104.json"),
                    "/work/1255/extended" => include_str!("../testdata/work-1255.json"),
                    "/work/1279/extended" => include_str!("../testdata/cycle-1279.json"),
                    "/edition/723/extended" => include_str!("../testdata/edition-723.json"),
                    _ => return (StatusCode::NOT_FOUND, r#"{"not found":null}"#).into_response(),
                };
                (StatusCode::OK, body).into_response()
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

    async fn provider(fake: Arc<Fake>, token: Option<&str>) -> String {
        let upstream = fake_fantlab(fake).await;
        let cfg = Config {
            site_base: SITE.into(),
            auth_token: token.map(str::to_string),
        };
        let client = Client::new(&upstream, "test", 2, Duration::ZERO);
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

    #[tokio::test]
    async fn search_returns_books_and_skips_reviews_and_cycles() {
        let fake = Arc::new(Fake::default());
        let base = provider(fake.clone(), None).await;
        let (status, body, _) = get(&base, "/search?query=%D0%A1%D0%BE%D0%BB%D1%8F%D1%80%D0%B8%D1%81&author=%D0%A1%D1%82%D0%B0%D0%BD%D0%B8%D1%81%D0%BB%D0%B0%D0%B2%20%D0%9B%D0%B5%D0%BC", None).await;
        let body = search_body(&body);
        assert_eq!((status, body.matches.len(), body.partial), (200, 1, false));
        assert_eq!(
            (
                body.matches[0].title.as_str(),
                body.matches[0].provider_id.as_str()
            ),
            ("Солярис", "3104")
        );
        for skipped in ["/work/922274/extended", "/work/2443651/extended"] {
            assert!(
                !fake.asked(skipped),
                "fetched {skipped}, which is a review or a cycle"
            );
        }
    }

    #[tokio::test]
    async fn search_marks_answer_partial_when_a_detail_fails() {
        let fake = Arc::new(Fake::default());
        *fake.fail_works_except.lock().unwrap() = Some("/work/3104/extended");
        let base = provider(fake, None).await;
        let body = search_body(
            &get(
                &base,
                "/search?query=%D0%A1%D0%BE%D0%BB%D1%8F%D1%80%D0%B8%D1%81",
                None,
            )
            .await
            .1,
        );
        assert_eq!((body.matches.len(), body.partial), (1, true));
    }

    #[tokio::test]
    async fn search_by_isbn_returns_the_edition() {
        let base = provider(Arc::new(Fake::default()), None).await;
        let body = search_body(&get(&base, "/search?isbn=978-5-17-025476-7", None).await.1);
        assert_eq!(body.matches.len(), 1);
        assert_eq!(
            (
                body.matches[0].provider_id.as_str(),
                body.matches[0].isbn13.as_str()
            ),
            ("e723", "9785170254767")
        );
    }

    #[tokio::test]
    async fn search_answers_audiobooks_with_nothing() {
        let fake = Arc::new(Fake::default());
        let base = provider(fake.clone(), None).await;
        let (status, body, _) = get(&base, "/search?query=x&mediaType=audiobook", None).await;
        assert_eq!((status, search_body(&body).matches.len()), (200, 0));
        assert!(fake.requested.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let base = provider(Arc::new(Fake::default()), None).await;
        assert_eq!(get(&base, "/search", None).await.0, 400);
    }

    #[tokio::test]
    async fn lookup() {
        let base = provider(Arc::new(Fake::default()), None).await;
        let (status, body, _) = get(&base, "/book/3104", None).await;
        let book: BookMetadata = serde_json::from_str(&body).unwrap();
        assert_eq!((status, book.title.as_str()), (200, "Солярис"));
        let (status, body, _) = get(&base, "/book/e723", None).await;
        let book: BookMetadata = serde_json::from_str(&body).unwrap();
        assert_eq!((status, book.publisher.as_str()), (200, "АСТ"));
        // Night Watch opens the Watch cycle; its side stories and comic do not count.
        for path in ["/book/1255", "/book/e723"] {
            let book: BookMetadata = serde_json::from_str(&get(&base, path, None).await.1).unwrap();
            assert_eq!(
                (
                    book.series[0].series.as_str(),
                    book.series[0].sequence.as_str()
                ),
                ("Дозоры", "1"),
                "{path}"
            );
        }
        for missing in ["/book/999", "/book/abc", "/book/e", "/book/12345678901"] {
            assert_eq!(get(&base, missing, None).await.0, 404, "{missing}");
        }
    }

    #[tokio::test]
    async fn upstream_rate_limit_is_passed_on() {
        let fake = Arc::new(Fake::default());
        fake.fail_status.store(429, Ordering::SeqCst);
        let base = provider(fake, None).await;
        let (status, _, headers) = get(&base, "/search?query=x", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("30"))
        );
    }

    #[tokio::test]
    async fn upstream_failure_is_a_bad_gateway() {
        let fake = Arc::new(Fake::default());
        fake.fail_status.store(500, Ordering::SeqCst);
        let base = provider(fake, None).await;
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
        assert_eq!(get(&base, "/book/3104", None).await.0, 502);
    }

    #[tokio::test]
    async fn auth_token() {
        let base = provider(Arc::new(Fake::default()), Some("Bearer s3cret")).await;
        assert_eq!(get(&base, "/book/3104", None).await.0, 401);
        assert_eq!(get(&base, "/book/3104", Some("Bearer s3cret")).await.0, 200);
        let (status, body, _) = get(&base, "/provider/info", None).await;
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (200, Some(true))
        );
        assert_eq!(
            (info["avatarUrl"].as_str(), info["color"].as_str()),
            (Some("https://fantlab.ru/img/fl_apple.png"), Some("#2c98cd"))
        );
    }
}
