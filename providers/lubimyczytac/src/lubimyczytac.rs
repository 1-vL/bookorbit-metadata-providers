use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::{ACCEPT, ACCEPT_LANGUAGE, RETRY_AFTER};
use axum::http::HeaderMap;
use regex::Regex;
use reqwest::Url;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Where every link in an answer points, whatever `LUBIMYCZYTAC_SITE` fetches from.
pub const SITE: &str = "https://lubimyczytac.pl";
const SEARCH_PATH: &str = "/szukaj/ksiazki";
const MAX_BODY_BYTES: usize = 4 << 20;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    /// Set when lubimyczytac.pl answers 429; nothing is sent to it again before then.
    paused_until: Mutex<Option<Instant>>,
}

impl Client {
    pub fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            slots: Semaphore::new(concurrency),
            interval,
            next: Mutex::new(Instant::now()),
            paused_until: Mutex::new(None),
        }
    }

    /// The HTML of a page on lubimyczytac.pl, given by its public address.
    pub async fn page(&self, url: &str) -> Result<String, UpstreamError> {
        let target = match url.strip_prefix(SITE) {
            Some(rest) => format!("{}{rest}", self.base),
            None => url.to_string(),
        };
        let _permit = self.acquire().await?;
        // Errors drop the URL: it carries the title someone searched for.
        let failed = |error: reqwest::Error| UpstreamError::Failed(error.without_url().to_string());
        let mut res = self
            .http
            .get(target)
            .header(
                ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(ACCEPT_LANGUAGE, "pl-PL,pl;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(failed)?;

        match res.status().as_u16() {
            404 => return Err(UpstreamError::NotFound),
            429 => {
                let retry_after_seconds = retry_after(res.headers());
                self.pause(Duration::from_secs(retry_after_seconds));
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds,
                });
            }
            200..=299 => {}
            status => {
                return Err(UpstreamError::Failed(format!(
                    "lubimyczytac.pl answered {status}"
                )))
            }
        }

        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(failed)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(UpstreamError::Failed(format!(
                    "page over {MAX_BODY_BYTES} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// At most `concurrency` requests in flight, each starting at least `interval` after the last.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>, UpstreamError> {
        self.check_paused()?;
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?;
        let start = {
            let mut next = self.next.lock().expect("limiter lock poisoned");
            let start = (*next).max(Instant::now());
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        self.check_paused()?;
        Ok(permit)
    }

    fn pause(&self, by: Duration) {
        let until = Instant::now() + by;
        let mut paused = self.paused_until.lock().expect("pause lock poisoned");
        if paused.is_none_or(|current| current < until) {
            *paused = Some(until);
        }
    }

    fn check_paused(&self) -> Result<(), UpstreamError> {
        let paused = *self.paused_until.lock().expect("pause lock poisoned");
        match paused {
            Some(until) if until > Instant::now() => Err(UpstreamError::RateLimited {
                retry_after_seconds: (until - Instant::now()).as_secs_f64().ceil().max(1.0) as u64,
            }),
            _ => Ok(()),
        }
    }
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

static BOOK_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/ksiazka/([0-9]+(?:/[^/?#]+)?)").expect("valid pattern"));
static BARE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+(?:/[^/?#]+)?$").expect("valid pattern"));

/// The search page, as the built-in source asked for it: the phrase, narrowed by the author.
pub fn build_search_url(phrase: &str, author: &str) -> String {
    let mut url = Url::parse(&format!("{SITE}{SEARCH_PATH}")).expect("valid search URL");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("phrase", phrase);
        if !author.trim().is_empty() {
            query.append_pair("author", author.trim());
        }
    }
    url.to_string()
}

/// The page of a stored id, the link BookOrbit has always built for it.
pub fn build_book_url(provider_id: &str) -> String {
    let path = extract_path(provider_id)
        .unwrap_or_else(|| provider_id.trim().trim_matches('/').to_string());
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // A bare numeric id 404s; lubimyczytac.pl serves the book for any non-empty slug.
    if segments.len() == 1 {
        segments.push("-");
    }
    let encoded: Vec<String> = segments.iter().map(|s| encode_uri_component(s)).collect();
    format!("{SITE}/ksiazka/{}", encoded.join("/"))
}

/// The id: the path after `/ksiazka/`, such as `123456/some-slug`, or a bare numeric id stored by
/// older BookOrbit versions.
pub fn extract_path(value: &str) -> Option<String> {
    if let Some(captures) = BOOK_PATH.captures(value) {
        return Some(captures[1].to_string());
    }
    let trimmed = value.trim().trim_matches('/');
    BARE_PATH.is_match(trimmed).then(|| trimmed.to_string())
}

/// The numeric book id alone, which tells two links to the same book apart from two books.
pub fn extract_id(value: &str) -> Option<String> {
    let path = extract_path(value)?;
    let id = path.split('/').next().unwrap_or_default();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then(|| id.to_string())
}

pub fn resolve_url(value: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    Url::parse(SITE)
        .ok()?
        .join(value)
        .ok()
        .map(|url| url.to_string())
}

fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_encodes_phrase_and_author() {
        let url = build_search_url("Wiedźmin", "Andrzej Sapkowski");
        assert!(
            url.starts_with("https://lubimyczytac.pl/szukaj/ksiazki"),
            "{url}"
        );
        assert!(url.contains("phrase=Wied%C5%BAmin"), "{url}");
        assert!(url.contains("author=Andrzej+Sapkowski"), "{url}");
        assert!(!build_search_url("Lalka", "").contains("author="));
        assert!(!build_search_url("Lalka", "  ").contains("author="));
    }

    #[test]
    fn extract_id_reads_the_numeric_id() {
        for (input, want) in [
            (
                "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie",
                Some("123456"),
            ),
            ("/ksiazka/789012", Some("789012")),
            ("4815162342", Some("4815162342")),
            ("https://lubimyczytac.pl/autor/123/foo", None),
        ] {
            assert_eq!(extract_id(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn extract_path_keeps_id_and_slug() {
        for (input, want) in [
            (
                "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie",
                Some("123456/ostatnie-zyczenie"),
            ),
            ("/ksiazka/789012/x?ref=1", Some("789012/x")),
            ("123456/ostatnie-zyczenie", Some("123456/ostatnie-zyczenie")),
            ("/ksiazka/789012", Some("789012")),
            ("not-a-valid-id", None),
            (
                "https://lubimyczytac.pl/ksiazka/wydania/138913/wiedzmin",
                None,
            ),
        ] {
            assert_eq!(extract_path(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn book_url_matches_the_link_bookorbit_builds() {
        assert_eq!(
            build_book_url("123456/ostatnie-zyczenie"),
            "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie"
        );
        assert_eq!(
            build_book_url("123456"),
            "https://lubimyczytac.pl/ksiazka/123456/-"
        );
        assert_eq!(
            build_book_url("1/zażółć gęślą"),
            "https://lubimyczytac.pl/ksiazka/1/za%C5%BC%C3%B3%C5%82%C4%87%20g%C4%99%C5%9Bl%C4%85"
        );
    }

    #[test]
    fn resolve_url_handles_relative_and_protocol_relative_links() {
        for (input, want) in [
            ("/ksiazka/1/a", "https://lubimyczytac.pl/ksiazka/1/a"),
            (
                "//static.lubimyczytac.pl/cover.jpg",
                "https://static.lubimyczytac.pl/cover.jpg",
            ),
            (
                "https://example.com/cover.jpg",
                "https://example.com/cover.jpg",
            ),
        ] {
            assert_eq!(resolve_url(input).as_deref(), Some(want), "{input}");
        }
    }

    #[tokio::test]
    async fn limiter_spaces_request_starts() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::from_millis(100));
        let started = Instant::now();
        for _ in 0..3 {
            drop(client.acquire().await.unwrap());
        }
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn a_pause_refuses_requests_until_it_ends() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::ZERO);
        client.pause(Duration::from_secs(30));
        match client.acquire().await {
            Err(UpstreamError::RateLimited {
                retry_after_seconds,
            }) => assert!((29..=30).contains(&retry_after_seconds)),
            other => panic!("expected a pause, got {:?}", other.map(|_| ())),
        };
    }
}
