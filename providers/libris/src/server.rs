use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::libris::{Client, Instance, Media, UpstreamError};
use crate::mapping::{
    has_title, is_record_id, isbn_forms, lists_isbn, map_instance, media_of, normalize_isbn, rank,
    search_attempts, BookMetadata, MAX_ID_LENGTH,
};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    libris: Arc<Client>,
}

pub fn routes(cfg: Arc<Config>, libris: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, libris })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "Libris",
        "version": VERSION,
        "avatarUrl": "https://libris.kb.se/apple-touch-icon.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "libris",
        "mediaTypes": ["book", "audiobook"],
        "lookup": true,
        // Not `cover`: only the odd e-book lookup carries one, so field rules should not count on it.
        "supportedFields": [
            "title", "subtitle", "description", "authors", "narrators", "publisher",
            "isbn", "publishedYear", "language", "pageCount", "seriesName", "seriesIndex",
            "genres", "duration"
        ],
        "searchParams": ["isbn", "limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "bookUrlTemplate": "https://libris.kb.se/{id}",
        // Libris's own `--color-primary`: `--theme-primary-500` on `:root`, oklch(72.74% .156 126.68).
        "color": "#8db83e",
        "rateLimit": { "maxConcurrency": 1, "minIntervalMs": state.cfg.interval.as_millis() as u64 },
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
    if query.is_empty() && isbn.is_empty() {
        return error(StatusCode::BAD_REQUEST, "query or isbn is required");
    }
    let media = match param("mediaType") {
        "audiobook" => Media::Audiobook,
        _ => Media::Book,
    };
    let limit = parse_limit(param("limit"));

    let answer = tokio::time::timeout(REQUEST_TIMEOUT, async {
        if !isbn.is_empty() {
            let mut found = search_by_isbn(&state, &isbn, media).await?;
            if !found.is_empty() || query.is_empty() {
                found.truncate(limit);
                return Ok(found);
            }
        }
        search_by_text(&state, query, author, media, limit).await
    })
    .await
    .unwrap_or_else(|_| Err(UpstreamError::Failed("timed out".into())));

    match answer {
        Ok(matches) => Json(json!({ "matches": matches })).into_response(),
        Err(err) => upstream_error("search", err),
    }
}

/// The editions that list the ISBN, in the asked-for medium. A free-text match on the number
/// alone could be a record that only mentions it.
async fn search_by_isbn(
    state: &AppState,
    isbn: &str,
    media: Media,
) -> Result<Vec<BookMetadata>, UpstreamError> {
    let found = state.libris.search_isbn(&isbn_forms(isbn)).await?;
    let listing: Vec<&Instance> = found.iter().filter(|i| lists_isbn(i, isbn)).collect();
    Ok(to_matches(&listing, media, isbn))
}

async fn search_by_text(
    state: &AppState,
    query: &str,
    author: &str,
    media: Media,
    limit: usize,
) -> Result<Vec<BookMetadata>, UpstreamError> {
    // Records without the asked-for title are kept in case no later search finds it either.
    let mut fallback = Vec::new();
    for terms in search_attempts(query, author) {
        let found = state.libris.search(&terms, media, limit).await?;
        let mut matches = to_matches(&found.iter().collect::<Vec<_>>(), media, "");
        rank(&mut matches, query, author);
        matches.truncate(limit);
        if matches.first().is_some_and(|book| has_title(book, query)) {
            return Ok(matches);
        }
        if fallback.is_empty() {
            fallback = matches;
        }
    }
    Ok(fallback)
}

fn to_matches(found: &[&Instance], media: Media, isbn: &str) -> Vec<BookMetadata> {
    let mut seen = HashSet::new();
    found
        .iter()
        .filter(|instance| media_of(instance) == Some(media))
        .map(|instance| map_instance(instance, media, isbn))
        .filter(|book| {
            !book.provider_id.is_empty()
                && !book.title.is_empty()
                && seen.insert(book.provider_id.clone())
        })
        .collect()
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    if !is_record_id(&id) {
        return error(StatusCode::NOT_FOUND, "Not found");
    }

    let result = tokio::time::timeout(REQUEST_TIMEOUT, state.libris.instance(&id))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("timed out".into())));

    match result {
        Ok(found) => {
            // An id issued earlier stays answerable even if Libris recategorises the record.
            let book = map_instance(&found, media_of(&found).unwrap_or(Media::Book), "");
            if book.provider_id.is_empty() || book.title.is_empty() {
                return error(StatusCode::NOT_FOUND, "Not found");
            }
            Json(book).into_response()
        }
        Err(UpstreamError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
        Err(err) => upstream_error("lookup", err),
    }
}

fn parse_limit(raw: &str) -> usize {
    match raw.parse::<i64>() {
        Ok(n) if n >= 1 => (n as usize).min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
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
            eprintln!("[provider.{op}] [fail] error={message:?} - libris request failed");
            error(StatusCode::BAD_GATEWAY, "libris unavailable")
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

/// Logs the path and status, never the query: the query is someone's reading.
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

    /// Answers like libris.kb.se from the captured fixtures, records every search it was asked,
    /// and can be told to fail every request with `status`.
    #[derive(Default)]
    struct Fake {
        status: AtomicU16,
        searches: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl Fake {
        fn asked(&self, key: &str) -> Vec<String> {
            self.searches
                .lock()
                .unwrap()
                .iter()
                .flat_map(|params| params.iter().filter(|(k, _)| k == key))
                .map(|(_, v)| v.clone())
                .collect()
        }
    }

    async fn fake_libris(fake: Arc<Fake>) -> String {
        let finder = fake.clone();
        let app = Router::new()
            .route(
                "/find.jsonld",
                get(move |Query(params): Query<Vec<(String, String)>>| {
                    let fake = finder.clone();
                    async move {
                        fake.searches.lock().unwrap().push(params.clone());
                        let value = |key: &str| {
                            params
                                .iter()
                                .filter(|(k, _)| k == key)
                                .map(|(_, v)| v.as_str())
                                .collect::<Vec<_>>()
                        };
                        let q = value("q").concat();
                        let filters = value("instanceOf.category.@id");
                        let body = if q == "9789129657982 | 9129657989" {
                            include_str!("../testdata/search-isbn-9789129657982.json")
                        } else if q == "pippi långstrump extra astrid lindgren" {
                            // Editions of another book, as when a subtitle word matches only
                            // other records.
                            include_str!("../testdata/search-isbn-9789129657982.json")
                        } else if q.starts_with("pippi långstrump")
                            && filters.contains(&"https://id.kb.se/marc/Fiction")
                        {
                            include_str!("../testdata/search-pippi-audiobooks.json")
                        } else if (q == "pippi långstrump astrid lindgren"
                            || q == "pippi långstrump")
                            && filters == ["https://id.kb.se/term/rda/Text"]
                        {
                            include_str!("../testdata/search-pippi-books.json")
                        } else {
                            include_str!("../testdata/search-empty.json")
                        };
                        answer(&fake, StatusCode::OK, body)
                    }
                }),
            )
            .route(
                "/{id}/data.json",
                get(move |Path(id): Path<String>| async move {
                    match id.as_str() {
                        "v61jx8fbsd06xtgv" => answer(
                            &fake,
                            StatusCode::OK,
                            include_str!("../testdata/record-v61jx8fbsd06xtgv.json"),
                        ),
                        "7jf33fwr5rfk3b87" => answer(
                            &fake,
                            StatusCode::OK,
                            include_str!("../testdata/record-7jf33fwr5rfk3b87.json"),
                        ),
                        _ => answer(
                            &fake,
                            StatusCode::NOT_FOUND,
                            include_str!("../testdata/record-not-found.json"),
                        ),
                    }
                }),
            );
        serve(app).await
    }

    fn answer(fake: &Fake, status: StatusCode, body: &'static str) -> Response {
        match fake.status.load(Ordering::SeqCst) {
            0 => (status, body).into_response(),
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

    async fn provider(auth_token: Option<&str>) -> (String, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        let upstream = fake_libris(fake.clone()).await;
        let cfg = Config {
            auth_token: auth_token.map(str::to_string),
            interval: Duration::from_secs(1),
        };
        let client = Client::new(&upstream, "test", Duration::ZERO);
        (serve(routes(Arc::new(cfg), Arc::new(client))).await, fake)
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

    fn ids(body: &serde_json::Value) -> Vec<String> {
        body["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["providerId"].as_str().unwrap().to_string())
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
            (Some("libris"), Some(true), Some(false))
        );
        assert_eq!(body["mediaTypes"], json!(["book", "audiobook"]));
        assert_eq!(body["searchParams"], json!(["isbn", "limit"]));
        assert_eq!(body["rateLimit"]["minIntervalMs"], 1000);
        assert_eq!(
            (body["avatarUrl"].as_str(), body["color"].as_str()),
            (
                Some("https://libris.kb.se/apple-touch-icon.png"),
                Some("#8db83e")
            )
        );
    }

    #[tokio::test]
    async fn search_puts_the_asked_for_title_first() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!(
                "{base}/search?query=Pippi+L%C3%A5ngstrump&author=Astrid+Lindgren&mediaType=book"
            ),
            None,
        )
        .await;
        assert_eq!(code, 200);
        // Libris put an edition with the author typed into its title first.
        assert_eq!(
            ids(&body),
            [
                "095p9hsgxln9mjfr",
                "zh9mwc29528d253",
                "wf7kt9071bvj902",
                "p0vd06vmm2t5xrhp",
                "r2xg28h0pfhkjcv9",
                "k2vtmlww4mhzs6t"
            ]
        );
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches[2]["title"], "Pippi Långstrump i Söderhavet");
        assert!(matches.iter().all(|m| m["mediaType"] == "book"));
        assert_eq!(fake.asked("q"), ["pippi långstrump astrid lindgren"]);
        assert_eq!(
            fake.asked("not-category.@id"),
            ["https://id.kb.se/term/saobf/ComponentPart"]
        );
        assert_eq!(fake.asked("_limit"), ["10"]);
    }

    #[tokio::test]
    async fn search_falls_back_to_the_title_alone() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Pippi+L%C3%A5ngstrump&author=Astrid+Lindgrn&limit=2"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(ids(&body), ["095p9hsgxln9mjfr", "zh9mwc29528d253"]);
        assert_eq!(
            fake.asked("q"),
            ["pippi långstrump astrid lindgrn", "pippi långstrump"]
        );
    }

    #[tokio::test]
    async fn search_drops_a_subtitle_that_finds_other_books() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Pippi+L%C3%A5ngstrump%3A+extra&author=Astrid+Lindgren"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(ids(&body)[0], "095p9hsgxln9mjfr");
        assert_eq!(
            fake.asked("q"),
            [
                "pippi långstrump extra astrid lindgren",
                "pippi långstrump astrid lindgren"
            ]
        );
    }

    #[tokio::test]
    async fn search_answers_audiobooks_with_audiobooks() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?query=Pippi+L%C3%A5ngstrump&mediaType=audiobook"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        let matches = body["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 6);
        assert!(matches.iter().all(|m| m["mediaType"] == "audiobook"));
        assert_eq!(matches[1]["providerId"], "7jf33fwr5rfk3b87");
        assert_eq!(
            (&matches[1]["duration"], &matches[1]["narrators"]),
            (&json!(13283), &json!(["Astrid Lindgren"]))
        );
        assert_eq!(
            fake.asked("instanceOf.category.@id"),
            crate::libris::AUDIOBOOK_CATEGORIES
        );
    }

    #[tokio::test]
    async fn search_by_isbn() {
        let (base, fake) = provider(None).await;
        let (code, body, _) = get_json(
            &format!("{base}/search?isbn=91-29-65798-9&mediaType=book"),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(ids(&body), ["cwn6mbnj9t696876", "gzrq1kqs0c18tgv"]);
        let first = &body["matches"][0];
        assert_eq!(
            (&first["isbn13"], &first["isbn10"]),
            (&json!("9789129657982"), &json!("9129657989"))
        );
        assert_eq!(fake.asked("q"), ["9789129657982 | 9129657989"]);

        // The ISBN is a printed book's: an audiobook search finds nothing by it, then searches
        // by title.
        let (code, body, _) = get_json(
            &format!(
                "{base}/search?isbn=9789129657982&query=Pippi+L%C3%A5ngstrump&mediaType=audiobook"
            ),
            None,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(body["matches"][0]["mediaType"], "audiobook");
        let (code, body, _) = get_json(
            &format!("{base}/search?isbn=9789129657982&mediaType=audiobook"),
            None,
        )
        .await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
    }

    #[tokio::test]
    async fn search_answers_empty_and_refuses_no_query() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/search?query=nothing"), None).await;
        assert_eq!(
            (code, body["matches"].as_array().map(Vec::len)),
            (200, Some(0))
        );
        let (code, _, _) = get_json(&format!("{base}/search"), None).await;
        assert_eq!(code, 400);
        let (code, _, _) = get_json(&format!("{base}/search?isbn=12345"), None).await;
        assert_eq!(code, 400);
    }

    #[tokio::test]
    async fn lookup_reads_the_record() {
        let (base, _) = provider(None).await;
        let (code, body, _) = get_json(&format!("{base}/book/v61jx8fbsd06xtgv"), None).await;
        assert_eq!(code, 200);
        assert_eq!(
            (
                body["providerId"].as_str(),
                body["title"].as_str(),
                body["mediaType"].as_str(),
                body["pages"].as_u64()
            ),
            (
                Some("v61jx8fbsd06xtgv"),
                Some("Pippi Långstrump"),
                Some("book"),
                Some(135)
            )
        );
        assert_eq!(body["url"], "https://libris.kb.se/v61jx8fbsd06xtgv");

        let (code, body, _) = get_json(&format!("{base}/book/7jf33fwr5rfk3b87"), None).await;
        assert_eq!(code, 200);
        assert_eq!(
            (body["mediaType"].as_str(), body["cover"].as_str()),
            (
                Some("audiobook"),
                Some("https://bookbites-prod-se.covers.bookbites.com/9789198119237")
            )
        );
    }

    #[tokio::test]
    async fn lookup_answers_404_for_unknown_ids() {
        let (base, _) = provider(None).await;
        for id in [
            "zzzzzzzzzzzzzzz",
            "conformance-unknown-id",
            "ABC",
            "..",
            &"a".repeat(21),
        ] {
            let (code, _, _) = get_json(&format!("{base}/book/{id}"), None).await;
            assert_eq!(code, 404, "lookup {id}");
        }
    }

    #[tokio::test]
    async fn upstream_throttle_and_failure() {
        let (base, fake) = provider(None).await;
        fake.status.store(429, Ordering::SeqCst);
        let (code, _, headers) = get_json(&format!("{base}/search?query=Pippi"), None).await;
        assert_eq!(
            (
                code,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        fake.status.store(500, Ordering::SeqCst);
        let (code, _, _) = get_json(&format!("{base}/book/v61jx8fbsd06xtgv"), None).await;
        assert_eq!(code, 502);
        let (code, _, _) = get_json(&format!("{base}/search?query=Pippi"), None).await;
        assert_eq!(code, 502);
        // A missing search endpoint is a failure, never "nothing found".
        fake.status.store(404, Ordering::SeqCst);
        let (code, _, _) = get_json(&format!("{base}/search?query=Pippi"), None).await;
        assert_eq!(code, 502);
    }

    #[tokio::test]
    async fn auth_token() {
        let (base, _) = provider(Some("Bearer s3cret")).await;
        assert_eq!(
            get_json(&format!("{base}/search?query=Pippi"), None)
                .await
                .0,
            401
        );
        assert_eq!(
            get_json(&format!("{base}/search?query=Pippi"), Some("Bearer s3cret"))
                .await
                .0,
            200
        );
        assert_eq!(
            get_json(
                &format!("{base}/book/v61jx8fbsd06xtgv"),
                Some("Bearer wrong")
            )
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
        let pacer = crate::libris::Pacer::new(Duration::from_secs(10));
        assert!(pacer.wait().await.is_ok());
        pacer.push_back(Duration::from_secs(20));
        assert!(matches!(
            pacer.wait().await,
            Err(UpstreamError::RateLimited { .. })
        ));
    }
}
