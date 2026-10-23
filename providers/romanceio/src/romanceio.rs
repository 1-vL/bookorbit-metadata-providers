use std::collections::HashSet;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::RETRY_AFTER;
use regex::Regex;
use reqwest::Url;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::task::JoinSet;

pub const SITE: &str = "https://www.romance.io";
const MAX_BODY_BYTES: usize = 4 << 20;
const MAX_GENRES: usize = 50;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;

static BOOK_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{24}$").expect("valid book id pattern"));
static BOOK_ID_IN_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)/books/([0-9a-f]{24})(?:/|$)").expect("valid book URL pattern")
});
static PAGES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([0-9][0-9,]*)\s+pages\b").expect("valid page pattern"));
static SPICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:spice/steam/heat level:\s*|rated ['"]?)([0-5](?:\.[0-9]+)?)/5\s*-\s*([^.'"]+)"#,
    )
    .expect("valid spice pattern")
});
static SERIES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\((.+)\s+#([0-9]+(?:\.[0-9]+)?)\)$").expect("valid series pattern")
});
static YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^0-9])([12][0-9]{3})(?:[^0-9]|$)").expect("valid year pattern")
});

static JSON_LD: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"script[type="application/ld+json"]"#));
static DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| selector("#book-description"));
static TOPICS: LazyLock<Selector> = LazyLock::new(|| {
    selector(r#"#valid-topics-list li.tagged-topic a.topic[href^="/topics/best/"]"#)
});
static STATS: LazyLock<Selector> = LazyLock::new(|| selector("#book-stats"));
static STEAM: LazyLock<Selector> = LazyLock::new(|| selector(".desc-steam-rating"));
static SERIES_LINK: LazyLock<Selector> = LazyLock::new(|| selector(r#"h1 a[href^="/series/"]"#));

#[derive(Debug)]
pub enum SourceError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_date: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesMembership>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesMembership {
    pub series: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchAnswer {
    pub matches: Vec<BookMetadata>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

#[derive(Debug, Default)]
pub struct Query {
    pub title: String,
    pub author: String,
    pub media_type: String,
    pub limit: usize,
}

pub trait Source: Send + Sync + 'static {
    fn search(
        &self,
        query: &Query,
    ) -> impl Future<Output = Result<SearchAnswer, SourceError>> + Send;

    fn lookup(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<BookMetadata>, SourceError>> + Send;
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchInfo {
    #[serde(default)]
    title: String,
    #[serde(default, rename = "avgRating")]
    average_rating: Option<f64>,
    #[serde(default, rename = "numRating")]
    rating_count: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchAuthor {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchSeries {
    #[serde(default)]
    title: String,
    #[serde(default)]
    no: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchHit {
    #[serde(default, rename = "_id")]
    id: String,
    #[serde(default)]
    info: SearchInfo,
    #[serde(default)]
    authors: Vec<SearchAuthor>,
    #[serde(default)]
    series: Vec<SearchSeries>,
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    success: bool,
    #[serde(default)]
    books: Vec<SearchHit>,
}

pub struct RomanceIo {
    client: Client,
}

impl RomanceIo {
    pub fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        Self {
            client: Client::new(base, user_agent, concurrency, interval),
        }
    }

    async fn detail(
        &self,
        id: &str,
        fallback: Option<&SearchHit>,
    ) -> Result<BookMetadata, SourceError> {
        let html = self.client.book_page(id).await?;
        parse_book_page(&html, id, fallback)
            .ok_or_else(|| SourceError::Failed("book page did not contain metadata".into()))
    }
}

impl Source for RomanceIo {
    async fn search(&self, query: &Query) -> Result<SearchAnswer, SourceError> {
        if query.media_type == "audiobook" {
            return Ok(SearchAnswer {
                matches: Vec::new(),
                partial: false,
            });
        }

        let phrase = format!("{} {}", query.title, query.author)
            .trim()
            .to_string();
        let hits = self.client.search(&phrase).await?;
        let hits: Vec<SearchHit> = hits
            .into_iter()
            .filter(|hit| valid_book_id(&hit.id).is_some() && !hit.info.title.trim().is_empty())
            .take(query.limit)
            .collect();

        let mut tasks = JoinSet::new();
        for (index, hit) in hits.iter().cloned().enumerate() {
            let client = self.client.clone();
            tasks.spawn(async move {
                let result = match client.book_page(&hit.id).await {
                    Ok(html) => parse_book_page(&html, &hit.id, Some(&hit)).ok_or_else(|| {
                        SourceError::Failed("book page did not contain metadata".into())
                    }),
                    Err(error) => Err(error),
                };
                (index, hit, result)
            });
        }

        let mut found: Vec<Option<BookMetadata>> = (0..hits.len()).map(|_| None).collect();
        let mut partial = false;
        let mut retry_after_seconds = None;
        while let Some(joined) = tasks.join_next().await {
            let (index, hit, result) =
                joined.map_err(|error| SourceError::Failed(error.to_string()))?;
            match result {
                Ok(book) => found[index] = Some(book),
                Err(SourceError::RateLimited {
                    retry_after_seconds: seconds,
                }) => {
                    retry_after_seconds = Some(retry_after_seconds.unwrap_or(0).max(seconds));
                }
                Err(SourceError::NotFound) => {
                    partial = true;
                }
                Err(SourceError::Failed(_)) => {
                    partial = true;
                    found[index] = Some(map_search_hit(&hit));
                }
            }
        }

        if let Some(retry_after_seconds) = retry_after_seconds {
            return Err(SourceError::RateLimited {
                retry_after_seconds,
            });
        }

        Ok(SearchAnswer {
            matches: found.into_iter().flatten().collect(),
            partial,
        })
    }

    async fn lookup(&self, id: &str) -> Result<Option<BookMetadata>, SourceError> {
        let Some(id) = valid_book_id(id) else {
            return Ok(None);
        };
        match self.detail(&id, None).await {
            Ok(book) => Ok(Some(book)),
            Err(SourceError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone)]
struct Client {
    inner: std::sync::Arc<ClientInner>,
}

struct ClientInner {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    paused_until: Mutex<Option<Instant>>,
}

impl Client {
    fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            inner: std::sync::Arc::new(ClientInner {
                base: base.trim_end_matches('/').to_string(),
                http,
                slots: Semaphore::new(concurrency),
                interval,
                next: Mutex::new(Instant::now()),
                paused_until: Mutex::new(None),
            }),
        }
    }

    async fn search(&self, phrase: &str) -> Result<Vec<SearchHit>, SourceError> {
        let body = self
            .get(
                "/json/search_books",
                &[("search", phrase)],
                "application/json",
            )
            .await?;
        let envelope: SearchEnvelope = serde_json::from_slice(&body)
            .map_err(|error| SourceError::Failed(format!("invalid search JSON: {error}")))?;
        if !envelope.success {
            return Err(SourceError::Failed("search answered success=false".into()));
        }
        Ok(envelope.books)
    }

    async fn book_page(&self, id: &str) -> Result<String, SourceError> {
        let id = valid_book_id(id).ok_or(SourceError::NotFound)?;
        let body = self
            .get(
                &format!("/books/{id}"),
                &[],
                "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8",
            )
            .await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
        accept: &str,
    ) -> Result<Vec<u8>, SourceError> {
        let _permit = self.acquire().await?;
        let mut url = Url::parse(&format!("{}{path}", self.inner.base))
            .map_err(|error| SourceError::Failed(error.to_string()))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let failed = |error: reqwest::Error| SourceError::Failed(error.without_url().to_string());
        let mut response = self
            .inner
            .http
            .get(url)
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .await
            .map_err(failed)?;

        match response.status().as_u16() {
            404 => return Err(SourceError::NotFound),
            429 => {
                let retry_after_seconds = retry_after(response.headers());
                self.pause(Duration::from_secs(retry_after_seconds));
                return Err(SourceError::RateLimited {
                    retry_after_seconds,
                });
            }
            200..=299 => {}
            403 => {
                return Err(SourceError::Failed(
                    "romance.io denied the request, possibly through Cloudflare".into(),
                ))
            }
            status => return Err(SourceError::Failed(format!("romance.io answered {status}"))),
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(SourceError::Failed(format!(
                    "response over {MAX_BODY_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    async fn acquire(&self) -> Result<SemaphorePermit<'_>, SourceError> {
        self.check_paused()?;
        let permit = self
            .inner
            .slots
            .acquire()
            .await
            .map_err(|error| SourceError::Failed(error.to_string()))?;
        let start = {
            let mut next = self.inner.next.lock().expect("limiter lock poisoned");
            let start = (*next).max(Instant::now());
            *next = start + self.inner.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        self.check_paused()?;
        Ok(permit)
    }

    fn pause(&self, duration: Duration) {
        let until = Instant::now() + duration;
        let mut paused = self.inner.paused_until.lock().expect("pause lock poisoned");
        if paused.is_none_or(|current| current < until) {
            *paused = Some(until);
        }
    }

    fn check_paused(&self) -> Result<(), SourceError> {
        let paused = *self.inner.paused_until.lock().expect("pause lock poisoned");
        match paused {
            Some(until) if until > Instant::now() => Err(SourceError::RateLimited {
                retry_after_seconds: (until - Instant::now()).as_secs_f64().ceil().max(1.0) as u64,
            }),
            _ => Ok(()),
        }
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

fn parse_book_page(
    html: &str,
    fallback_id: &str,
    fallback: Option<&SearchHit>,
) -> Option<BookMetadata> {
    let document = Html::parse_document(html);
    let structured = structured_book(&document);
    let provider_id = structured
        .as_ref()
        .and_then(|book| book.get("url"))
        .and_then(Value::as_str)
        .and_then(id_from_url)
        .or_else(|| valid_book_id(fallback_id))?;

    let mut book = fallback.map(map_search_hit).unwrap_or_default();
    book.media_type = "book".into();
    book.provider_id = provider_id.clone();
    book.url = canonical_book_url(&provider_id);
    book.cover = format!("https://s3.amazonaws.com/romance.io/books/large/{provider_id}.jpg");

    if let Some(structured) = structured.as_ref() {
        if let Some(title) = string_field(structured, "name") {
            book.title = title;
        }
        let authors = structured_authors(structured);
        if !authors.is_empty() {
            book.authors = authors;
        }
        if let Some(cover) = string_field(structured, "image").filter(|url| is_https_url(url)) {
            book.cover = cover;
        }
        if let Some(language) = string_field(structured, "inLanguage") {
            book.language_code = language;
        }
        if let Some(published) = string_field(structured, "datePublished") {
            let (date, year) = published_date(&published);
            book.published_date = date;
            book.published_year = year;
        }
        if let Some(rating) = structured.get("aggregateRating").and_then(Value::as_object) {
            book.rating =
                flex_float(rating.get("ratingValue")).filter(|value| (0.0..=5.0).contains(value));
            book.rating_count = flex_u64(rating.get("ratingCount"));
        }
    }

    if let Some(description) = document.select(&DESCRIPTION).next() {
        let text = text_without_cover(description);
        if !text.is_empty() {
            book.description = text;
        }
    }

    if let Some(stats) = document.select(&STATS).next() {
        let text = element_text(stats);
        book.pages = PAGES
            .captures(&text)
            .and_then(|captures| captures[1].replace(',', "").parse::<u32>().ok())
            .filter(|pages| *pages > 0);
    }

    let mut genres = Vec::new();
    let spice_text = document
        .select(&STEAM)
        .map(element_text)
        .find(|text| !text.is_empty())
        .or_else(|| {
            structured
                .as_ref()
                .and_then(|book| string_field(book, "description"))
        });
    if let Some(spice) = spice_text.as_deref().and_then(spice_genre) {
        genres.push(spice);
    }
    genres.extend(
        document
            .select(&TOPICS)
            .map(element_text)
            .filter(|topic| !topic.is_empty()),
    );
    book.genres = unique_limited(genres, MAX_GENRES);

    if let Some(series) = document.select(&SERIES_LINK).next().and_then(parse_series) {
        book.series = vec![series];
    }

    (!book.title.is_empty()).then_some(book)
}

fn structured_book(document: &Html) -> Option<serde_json::Map<String, Value>> {
    for script in document.select(&JSON_LD) {
        let raw = script.text().collect::<String>();
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let candidates: Vec<&Value> = value
            .get("@graph")
            .and_then(Value::as_array)
            .map(|graph| graph.iter().collect())
            .unwrap_or_else(|| vec![&value]);
        for candidate in candidates {
            let Some(object) = candidate.as_object() else {
                continue;
            };
            if object.get("@type").and_then(Value::as_str) == Some("Book") {
                return Some(object.clone());
            }
        }
    }
    None
}

fn structured_authors(book: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(value) = book.get("author") else {
        return Vec::new();
    };
    let values: Vec<&Value> = match value.as_array() {
        Some(authors) => authors.iter().collect(),
        None => vec![value],
    };
    unique_limited(
        values
            .into_iter()
            .filter_map(Value::as_object)
            .filter_map(|author| string_field(author, "name")),
        20,
    )
}

fn parse_series(link: ElementRef<'_>) -> Option<SeriesMembership> {
    let text = element_text(link);
    let captures = SERIES.captures(&text)?;
    let name = normalize_text(&captures[1]);
    if name.is_empty() {
        return None;
    }
    Some(SeriesMembership {
        series: name,
        sequence_number: captures[2].parse().ok(),
    })
}

fn map_search_hit(hit: &SearchHit) -> BookMetadata {
    let id = valid_book_id(&hit.id).unwrap_or_default();
    BookMetadata {
        media_type: "book".into(),
        provider_id: id.clone(),
        title: normalize_text(&hit.info.title),
        authors: unique_limited(
            hit.authors
                .iter()
                .map(|author| normalize_text(&author.name))
                .filter(|name| !name.is_empty()),
            20,
        ),
        cover: if id.is_empty() {
            String::new()
        } else {
            format!("https://s3.amazonaws.com/romance.io/books/large/{id}.jpg")
        },
        series: hit
            .series
            .iter()
            .filter_map(|series| {
                let name = normalize_text(&series.title);
                (!name.is_empty()).then_some(SeriesMembership {
                    series: name,
                    sequence_number: series.no,
                })
            })
            .collect(),
        rating: hit
            .info
            .average_rating
            .filter(|rating| (0.0..=5.0).contains(rating)),
        rating_count: hit.info.rating_count,
        url: if id.is_empty() {
            String::new()
        } else {
            canonical_book_url(&id)
        },
        ..BookMetadata::default()
    }
}

fn text_without_cover(root: ElementRef<'_>) -> String {
    let mut parts = Vec::new();
    for node in root.descendants() {
        let scraper::node::Node::Text(text) = node.value() else {
            continue;
        };
        let excluded = node
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|element| {
                element.value().attr("class").is_some_and(|classes| {
                    classes
                        .split_whitespace()
                        .any(|class| class == "book-cover-container")
                }) || element.value().attr("class").is_some_and(|classes| {
                    classes
                        .split_whitespace()
                        .any(|class| class == "description-more" || class == "desc-steam-rating")
                })
            });
        if !excluded {
            parts.push(text.text.as_ref());
        }
    }
    normalize_text(&parts.join(" "))
}

fn spice_genre(text: &str) -> Option<String> {
    let captures = SPICE.captures(text)?;
    let rating = captures[1].trim();
    let label = normalize_text(&captures[2]);
    (!label.is_empty()).then(|| format!("Spice: {rating}/5 - {label}"))
}

fn published_date(raw: &str) -> (String, String) {
    let year = YEAR
        .captures(raw)
        .map(|captures| captures[1].to_string())
        .unwrap_or_default();
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != year {
        return (String::new(), year);
    }
    let day = parts[0].parse::<u8>().ok();
    let month = match parts[1].to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    };
    match (day, month) {
        (Some(day @ 1..=31), Some(month)) => (format!("{year}-{month:02}-{day:02}"), year),
        _ => (String::new(), year),
    }
}

fn valid_book_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    BOOK_ID
        .is_match(trimmed)
        .then(|| trimmed.to_ascii_lowercase())
}

fn id_from_url(raw: &str) -> Option<String> {
    BOOK_ID_IN_URL
        .captures(raw)
        .and_then(|captures| valid_book_id(&captures[1]))
}

fn canonical_book_url(id: &str) -> String {
    format!("{SITE}/books/{id}")
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(decode_html_text)
        .filter(|value| !value.is_empty())
}

fn flex_float(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(string)) => string.trim().parse().ok(),
        _ => None,
    }
}

fn flex_u64(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(string)) => string.trim().parse().ok(),
        _ => None,
    }
}

fn element_text(element: ElementRef<'_>) -> String {
    normalize_text(&element.text().collect::<Vec<_>>().join(" "))
}

fn normalize_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_html_text(raw: &str) -> String {
    let fragment = Html::parse_fragment(raw);
    normalize_text(&fragment.root_element().text().collect::<Vec<_>>().join(" "))
}

fn unique_limited<I>(values: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    let mut output = Vec::new();
    for value in values {
        let value = normalize_text(&value);
        if !value.is_empty() && seen.insert(value.to_lowercase()) {
            output.push(value);
            if output.len() == limit {
                break;
            }
        }
    }
    output
}

fn is_https_url(raw: &str) -> bool {
    Url::parse(raw)
        .ok()
        .is_some_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;

    use super::*;

    const SEARCH_JSON: &str = include_str!("../testdata/search-pride-and-prejudice.json");
    const BOOK_HTML: &str = include_str!("../testdata/pride-and-prejudice.html");
    const SERIES_HTML: &str = include_str!("../testdata/pride-and-prejudice-manga.html");

    #[test]
    fn search_json_maps_stable_ids_and_summary_fields() {
        let envelope: SearchEnvelope = serde_json::from_str(SEARCH_JSON).unwrap();
        assert!(envelope.success);
        assert_eq!(envelope.books.len(), 2);
        let book = map_search_hit(&envelope.books[0]);
        assert_eq!(book.provider_id, "5484ecd47a5936fb0405756c");
        assert_eq!(book.title, "Pride and Prejudice");
        assert_eq!(book.authors, ["Jane Austen"]);
        assert_eq!(book.rating, Some(4.56));
        assert_eq!(book.rating_count, Some(1670));
        assert!(book.cover.ends_with("/5484ecd47a5936fb0405756c.jpg"));
    }

    #[test]
    fn book_page_maps_topics_spice_and_core_metadata() {
        let book = parse_book_page(BOOK_HTML, "5484ecd47a5936fb0405756c", None).unwrap();
        assert_eq!(book.title, "Pride and Prejudice");
        assert_eq!(book.authors, ["Jane Austen"]);
        assert_eq!(book.published_year, "1813");
        assert_eq!(book.language_code, "en");
        assert_eq!(book.pages, Some(435));
        assert_eq!(book.rating, Some(4.56));
        assert_eq!(book.rating_count, Some(1670));
        assert_eq!(
            book.description,
            "When Elizabeth Bennet first meets Mr Darcy. Their first impressions prove incomplete."
        );
        assert_eq!(
            book.genres,
            [
                "Spice: 1/5 - Glimpses and kisses",
                "historical",
                "enemies to lovers",
                "slow burn"
            ]
        );
    }

    #[test]
    fn book_page_maps_full_date_and_series() {
        let book = parse_book_page(SERIES_HTML, "64aae9ea0f498c040bf904ad", None).unwrap();
        assert_eq!(book.title, "Pride & Prejudice");
        assert_eq!(book.published_date, "2021-01-19");
        assert_eq!(book.published_year, "2021");
        assert_eq!(
            book.series,
            [SeriesMembership {
                series: "Manga Classics".into(),
                sequence_number: Some(2.0)
            }]
        );
        assert!(book.genres.iter().all(|genre| !genre.starts_with("Spice:")));
    }

    #[test]
    fn ids_and_dates_are_strict() {
        assert_eq!(
            valid_book_id("5484ECD47A5936FB0405756C").as_deref(),
            Some("5484ecd47a5936fb0405756c")
        );
        assert_eq!(valid_book_id("../admin"), None);
        assert_eq!(
            id_from_url("https://www.romance.io/books/5484ecd47a5936fb0405756c/title").as_deref(),
            Some("5484ecd47a5936fb0405756c")
        );
        assert_eq!(published_date("1813"), (String::new(), "1813".into()));
        assert_eq!(
            published_date("19 Jan 2021"),
            ("2021-01-19".into(), "2021".into())
        );
    }

    #[tokio::test]
    async fn source_fetches_search_results_and_book_pages() {
        let app = Router::new()
            .route(
                "/json/search_books",
                get(|| async { ([("content-type", "application/json")], SEARCH_JSON) }),
            )
            .route(
                "/books/{id}",
                get(|| async { ([("content-type", "text/html")], BOOK_HTML) }),
            );
        let site = serve(app).await;
        let source = RomanceIo::new(&site, "test", 1, Duration::ZERO);
        let answer = source
            .search(&Query {
                title: "Pride and Prejudice".into(),
                author: "Jane Austen".into(),
                media_type: "book".into(),
                limit: 1,
            })
            .await
            .unwrap();

        assert!(!answer.partial);
        assert_eq!(answer.matches.len(), 1);
        assert_eq!(answer.matches[0].title, "Pride and Prejudice");
        assert_eq!(
            answer.matches[0].genres[0],
            "Spice: 1/5 - Glimpses and kisses"
        );
    }

    #[tokio::test]
    async fn source_preserves_upstream_rate_limits() {
        let app = Router::new().route(
            "/json/search_books",
            get(|| async {
                (StatusCode::TOO_MANY_REQUESTS, [(RETRY_AFTER, "42")], "").into_response()
            }),
        );
        let site = serve(app).await;
        let source = RomanceIo::new(&site, "test", 1, Duration::ZERO);
        let result = source
            .search(&Query {
                title: "Pride and Prejudice".into(),
                limit: 1,
                ..Query::default()
            })
            .await;

        assert!(matches!(
            result,
            Err(SourceError::RateLimited {
                retry_after_seconds: 42
            })
        ));
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn limiter_spaces_request_starts_and_pause_is_honored() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::from_millis(50));
        let started = Instant::now();
        for _ in 0..3 {
            drop(client.acquire().await.unwrap());
        }
        assert!(started.elapsed() >= Duration::from_millis(100));

        client.pause(Duration::from_secs(30));
        assert!(matches!(
            client.acquire().await,
            Err(SourceError::RateLimited { .. })
        ));
    }
}
