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

use crate::lubimyczytac::{build_book_url, build_search_url, extract_path, Client, UpstreamError};
use crate::mapping::{map_book, parse_book_page, search_results, BookMetadata};
use crate::{Config, VERSION};

/// The built-in source fetched at most five books a search, one page each.
const MAX_LIMIT: usize = 5;
const MAX_ID_LENGTH: usize = 512;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    lc: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: some matches could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, lc: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{*id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, lc })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "LubimyCzytac",
        "version": VERSION,
        "avatarUrl": "https://s.lubimyczytac.pl/img/favicon/2021/apple-touch-icon.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "lubimyczytac",
        "mediaTypes": ["book", "audiobook", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "description", "cover", "authors", "publisher", "isbn", "publishedYear",
            "language", "pageCount", "seriesName", "seriesIndex", "genres", "communityRating"
        ],
        // Not `isbn`: the site's search matches titles only, so an ISBN finds unrelated books.
        "searchParams": ["limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "bookUrlTemplate": "https://lubimyczytac.pl/ksiazka/{id}",
        "coverShape": "portrait",
        // The brand orange of lubimyczytac.pl's stylesheet (`--orange` and `.text-color-brand`).
        "color": "#ff5500",
        // A search already keeps both upstream slots busy, so BookOrbit sends one call at a time
        // and each gets its whole timeout.
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
        // The site's search matches titles, not ISBNs: an ISBN alone would bring back whatever
        // books share those digits, so it gets an honest empty answer instead.
        if !param("isbn").is_empty() {
            return Json(serde_json::json!({ "matches": [] })).into_response();
        }
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    let limit = param("limit")
        .parse::<usize>()
        .map_or(MAX_LIMIT, |n| n.clamp(1, MAX_LIMIT));
    let deadline = tokio::time::Instant::now() + state.cfg.deadline;

    match search_books(&state, phrase, param("author"), limit, deadline).await {
        Ok(answer) => Json(answer).into_response(),
        Err(err) => upstream_error("search", err),
    }
}

async fn search_books(
    state: &AppState,
    phrase: &str,
    author: &str,
    limit: usize,
    deadline: tokio::time::Instant,
) -> Result<SearchResponse, UpstreamError> {
    let html = tokio::time::timeout_at(deadline, state.lc.page(&build_search_url(phrase, author)))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("search page timed out".into())))
        .map_err(|err| match err {
            UpstreamError::NotFound => UpstreamError::Failed("search page answered 404".into()),
            err => err,
        })?;

    let links = search_results(&html, limit);
    let mut tasks = JoinSet::new();
    for (index, link) in links.iter().enumerate() {
        let lc = state.lc.clone();
        let (url, provider_id) = (link.url.clone(), link.provider_id.clone());
        tasks.spawn(async move { (index, fetch_book(&lc, &url, &provider_id).await) });
    }

    let mut found: Vec<Option<BookMetadata>> = (0..links.len()).map(|_| None).collect();
    let mut partial = false;
    let mut failure = None;
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(None) => break,
            Ok(Some(Ok((index, result)))) => match result {
                Ok(Some(book)) => found[index] = Some(book),
                // A page that is gone or does not parse is skipped, as the built-in source did.
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
                failure.get_or_insert(UpstreamError::Failed("book pages timed out".into()));
                break;
            }
        }
    }

    let matches: Vec<BookMetadata> = found.into_iter().flatten().collect();
    if matches.is_empty() && partial {
        return Err(
            failure.unwrap_or_else(|| UpstreamError::Failed("no book page could be read".into()))
        );
    }
    Ok(SearchResponse { matches, partial })
}

/// One book page as an answer. `None` when the page has no title, which the built-in source
/// skipped as unreadable.
async fn fetch_book(
    lc: &Client,
    url: &str,
    provider_id: &str,
) -> Result<Option<BookMetadata>, UpstreamError> {
    let html = lc.page(url).await?;
    Ok(map_book(
        &parse_book_page(&html, Some(url)),
        url,
        provider_id,
    ))
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let path = match extract_path(&raw) {
        Some(path) if raw.len() <= MAX_ID_LENGTH => path,
        _ => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    let url = build_book_url(&path);

    let result = tokio::time::timeout(state.cfg.deadline, fetch_book(&state.lc, &url, &path))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("book page timed out".into())));

    match result {
        Ok(Some(book)) => Json(book).into_response(),
        Ok(None) => {
            eprintln!("[provider.lookup] [fail] error=\"book page did not parse\" - lubimyczytac page unreadable");
            error(
                StatusCode::BAD_GATEWAY,
                "lubimyczytac.pl page could not be read",
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
                "lubimyczytac.pl is rate limiting this provider",
            );
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - lubimyczytac request failed");
            error(StatusCode::BAD_GATEWAY, "lubimyczytac.pl is unavailable")
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

    const SEARCH_HTML: &str = r#"
      <div class="book-card">
        <a class="book-card__title" href="/ksiazka/123456/ostatnie-zyczenie">Ostatnie życzenie</a>
      </div>
    "#;

    const SEARCH_TWO: &str = r#"
      <a class="book-card__title" href="/ksiazka/123456/ostatnie-zyczenie">Ostatnie życzenie</a>
      <a class="book-card__title" href="/ksiazka/789012/miecz-przeznaczenia">Miecz przeznaczenia</a>
    "#;

    const BOOK_HTML: &str = r#"
      <h1 class="book__title">Ostatnie życzenie</h1>
      <div class="book-cover"><img src="/upload/books/cover.jpg" /></div>
      <a href="/wydawnictwo/123/superNowa">superNOWA</a>
      <dl><dt>Język:</dt><dd>polski</dd></dl>
      <meta property="books:isbn" content="9788375780635" />
      <script type="application/ld+json">
      { "@type": "Book", "url": "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie", "numberOfPages": 330, "datePublished": "2014-05-12", "author": { "@type": "Person", "name": "Andrzej Sapkowski" } }
      </script>
    "#;

    const BOOK_HTML_2: &str = r#"
      <h1 class="book__title">Miecz przeznaczenia</h1>
      <script type="application/ld+json">
      { "@type": "Book", "url": "https://lubimyczytac.pl/ksiazka/789012/miecz-przeznaczenia", "numberOfPages": 280, "datePublished": "2015" }
      </script>
    "#;

    const EMPTY_PAGE: &str = "<html><body></body></html>";

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
                let logged = match req.uri().query() {
                    Some(query) => format!("{path}?{query}"),
                    None => path.clone(),
                };
                fake.requested.lock().unwrap().push(logged);
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
                Some("lubimyczytac"),
                Some("LubimyCzytac"),
                Some(true),
                Some(false)
            )
        );
        assert_eq!(
            info["bookUrlTemplate"],
            "https://lubimyczytac.pl/ksiazka/{id}"
        );
        assert_eq!(info["idMaxLength"], 512);
        assert_eq!(
            (info["avatarUrl"].as_str(), info["color"].as_str()),
            (
                Some("https://s.lubimyczytac.pl/img/favicon/2021/apple-touch-icon.png"),
                Some("#ff5500")
            )
        );
    }

    #[tokio::test]
    async fn search_by_title_then_fetches_book_details() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_HTML),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
        ]);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=Ostatnie%20%C5%BCyczenie&author=Andrzej%20Sapkowski&mediaType=book",
            None,
        )
        .await;
        assert_eq!((status, matches(&body).len()), (200, 1));
        assert!(body.get("partial").is_none());
        let book = &matches(&body)[0];
        assert_eq!(book["providerId"], "123456/ostatnie-zyczenie");
        assert_eq!(book["title"], "Ostatnie życzenie");
        assert_eq!(book["authors"], json!(["Andrzej Sapkowski"]));
        assert_eq!(book["publisher"], "superNOWA");
        assert_eq!(book["languageCode"], "pl");
        assert_eq!(book["pages"], 330);
        assert_eq!(book["publishedYear"], "2014");
        assert_eq!(book["isbn13"], "9788375780635");
        assert_eq!(
            book["url"],
            "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie"
        );
        assert_eq!(
            book["cover"],
            "https://lubimyczytac.pl/upload/books/cover.jpg"
        );
        assert_eq!(
            fake.requests()[0],
            "/szukaj/ksiazki?phrase=Ostatnie+%C5%BCyczenie&author=Andrzej+Sapkowski"
        );
    }

    #[tokio::test]
    async fn search_requires_a_query() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/search", None).await.0, 400);
        assert_eq!(get(&base, "/search?query=%20&author=x", None).await.0, 400);
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn search_by_isbn_without_a_title() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_HTML),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
        ]);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?isbn=9788375780635", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
        // With a title, the title is the phrase and the ISBN is not sent.
        get(&base, "/search?query=Lalka&isbn=9788375780635", None).await;
        assert!(fake
            .requests()
            .contains(&"/szukaj/ksiazki?phrase=Lalka".to_string()));
    }

    #[tokio::test]
    async fn search_keeps_the_site_order_across_several_books() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
            ("/ksiazka/789012/miecz-przeznaczenia", 200, BOOK_HTML_2),
        ]);
        fake.delays.lock().unwrap().insert(
            "/ksiazka/123456/ostatnie-zyczenie".into(),
            Duration::from_millis(150),
        );
        let base = provider(fake).await;
        let (_, body, _) = get(&base, "/search?query=Sapkowski", None).await;
        let titles: Vec<&str> = matches(&body)
            .iter()
            .map(|b| b["title"].as_str().unwrap())
            .collect();
        assert_eq!(titles, ["Ostatnie życzenie", "Miecz przeznaczenia"]);
        assert_eq!(matches(&body)[1]["publishedYear"], "2015");
    }

    #[tokio::test]
    async fn search_respects_the_limit() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
            ("/ksiazka/789012/miecz-przeznaczenia", 200, BOOK_HTML_2),
        ]);
        let base = provider(fake.clone()).await;
        let (_, body, _) = get(&base, "/search?query=Sapkowski&limit=1", None).await;
        assert_eq!(matches(&body).len(), 1);
        assert_eq!(fake.requests().len(), 2);
    }

    #[tokio::test]
    async fn real_search_fetches_five_books() {
        let fake = Fake::with(&[(
            "/szukaj/ksiazki",
            200,
            include_str!("../testdata/search-wiedzmin.html"),
        )]);
        for path in [
            "/ksiazka/138913/wiedzmin",
            "/ksiazka/4811730/wiedzmin-szpony-i-kly",
            "/ksiazka/5244386/wiedzmin-ostatnie-zyczenie",
            "/ksiazka/303348/wiedzmin-ostatnie-zyczenie",
            "/ksiazka/98232/wiedzmin-zbior-opowiadan",
        ] {
            fake.pages.lock().unwrap().insert(
                path.into(),
                (200, include_str!("../testdata/book-138913.html").into()),
            );
        }
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?query=Wied%C5%BAmin&limit=10", None).await;
        assert_eq!((status, matches(&body).len()), (200, 5));
        // Every page here is the same book, whose canonical id comes from its own JSON-LD.
        assert_eq!(matches(&body)[0]["providerId"], "138913/wiedzmin");
        assert_eq!(fake.requests().len(), 6);
    }

    #[tokio::test]
    async fn unreadable_book_pages_fail_the_search_instead_of_answering_nothing() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, EMPTY_PAGE),
            ("/ksiazka/789012/miecz-przeznaczenia", 200, EMPTY_PAGE),
        ]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=Wied%C5%BAmin", None).await.0, 502);
    }

    #[tokio::test]
    async fn a_skipped_book_marks_the_answer_partial() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
            ("/ksiazka/789012/miecz-przeznaczenia", 503, ""),
        ]);
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?query=Wied%C5%BAmin", None).await;
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
    }

    #[tokio::test]
    async fn search_page_failure_is_a_bad_gateway() {
        let fake = Fake::with(&[("/szukaj/ksiazki", 503, "")]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=Wied%C5%BAmin", None).await.0, 502);
        let base = provider(Fake::with(&[])).await;
        assert_eq!(get(&base, "/search?query=Wied%C5%BAmin", None).await.0, 502);
    }

    #[tokio::test]
    async fn a_slow_book_is_left_out_when_the_deadline_passes() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
            ("/ksiazka/789012/miecz-przeznaczenia", 200, BOOK_HTML_2),
        ]);
        fake.delays.lock().unwrap().insert(
            "/ksiazka/789012/miecz-przeznaczenia".into(),
            Duration::from_secs(3),
        );
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(700)).await;
        let started = Instant::now();
        let (status, body, _) = get(&base, "/search?query=Sapkowski", None).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
        assert_eq!(matches(&body)[0]["title"], "Ostatnie życzenie");
    }

    #[tokio::test]
    async fn lookup_by_stored_ids() {
        let fake = Fake::with(&[
            ("/ksiazka/123456/-", 200, BOOK_HTML),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
        ]);
        let base = provider(fake.clone()).await;
        // A bare numeric id, as older BookOrbit versions stored, answers with the canonical id.
        let (status, book, _) = get(&base, "/book/123456", None).await;
        assert_eq!(
            (status, book["providerId"].as_str(), book["title"].as_str()),
            (
                200,
                Some("123456/ostatnie-zyczenie"),
                Some("Ostatnie życzenie")
            )
        );
        assert_eq!(fake.requests(), ["/ksiazka/123456/-"]);
        for id in [
            "123456%2Fostatnie-zyczenie",
            "123456/ostatnie-zyczenie",
            "https%3A%2F%2Flubimyczytac.pl%2Fksiazka%2F123456%2Fostatnie-zyczenie",
        ] {
            let (status, book, _) = get(&base, &format!("/book/{id}"), None).await;
            assert_eq!(
                (status, book["providerId"].as_str()),
                (200, Some("123456/ostatnie-zyczenie")),
                "{id}"
            );
        }
        assert_eq!(
            fake.requests()[1..],
            ["/ksiazka/123456/ostatnie-zyczenie"; 3]
        );
    }

    #[tokio::test]
    async fn lookup_answers_404_for_unknown_and_invalid_ids() {
        let fake = Fake::with(&[]);
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/book/999999", None).await.0, 404);
        let asked = fake.requests().len();
        let long = format!("1/{}", "a".repeat(MAX_ID_LENGTH));
        for id in [
            "not-a-valid-id",
            "..",
            "conformance-unknown-id",
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
        let fake = Fake::with(&[("/ksiazka/1/-", 200, EMPTY_PAGE), ("/ksiazka/2/-", 503, "")]);
        let base = provider(fake).await;
        assert_eq!(get(&base, "/book/1", None).await.0, 502);
        assert_eq!(get(&base, "/book/2", None).await.0, 502);
    }

    #[tokio::test]
    async fn network_failure_is_a_bad_gateway() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let base = provider_at(&closed, None, Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/book/123456", None).await.0, 502);
        assert_eq!(get(&base, "/search?query=x", None).await.0, 502);
    }

    #[tokio::test]
    async fn upstream_rate_limit_is_passed_on_and_held() {
        let fake = Fake::with(&[("/ksiazka/123456/-", 429, "")]);
        let base = provider(fake.clone()).await;
        let (status, _, headers) = get(&base, "/book/123456", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        // The pause holds: the next call is refused without asking lubimyczytac.pl again.
        let (status, _, headers) = get(&base, "/search?query=x", None).await;
        let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
        assert_eq!(status, 429);
        assert!((40..=42).contains(&retry_after), "{retry_after}");
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_rate_limit_during_details_keeps_what_was_found() {
        let fake = Fake::with(&[
            ("/szukaj/ksiazki", 200, SEARCH_TWO),
            ("/ksiazka/123456/ostatnie-zyczenie", 200, BOOK_HTML),
            ("/ksiazka/789012/miecz-przeznaczenia", 429, ""),
        ]);
        // The 429 arrives after the first book was sent, so the pause cannot refuse it.
        fake.delays.lock().unwrap().insert(
            "/ksiazka/789012/miecz-przeznaczenia".into(),
            Duration::from_millis(200),
        );
        let base = provider(fake).await;
        let (status, body, _) = get(&base, "/search?query=Sapkowski", None).await;
        assert_eq!(
            (status, matches(&body).len(), body["partial"].as_bool()),
            (200, 1, Some(true))
        );
    }

    #[tokio::test]
    async fn auth_token() {
        let fake = Fake::with(&[("/ksiazka/123456/-", 200, BOOK_HTML)]);
        let upstream = fake_site(fake).await;
        let base = provider_at(&upstream, Some("Bearer s3cret"), Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/book/123456", None).await.0, 401);
        assert_eq!(
            get(&base, "/search?query=x", Some("Bearer nope")).await.0,
            401
        );
        assert_eq!(
            get(&base, "/book/123456", Some("Bearer s3cret")).await.0,
            200
        );
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(
            (status, info["authenticationRequired"].as_bool()),
            (200, Some(true))
        );
    }
}
