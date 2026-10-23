use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header::{ACCEPT, ACCEPT_LANGUAGE, RETRY_AFTER};
use axum::http::HeaderMap;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Where every link in an answer points, whatever `YANDEX_BOOKS_SITE` fetches from.
pub const SITE: &str = "https://books.yandex.ru";
const MAX_BODY_BYTES: usize = 4 << 20;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;
/// A captcha says the site wants a rest; it never says for how long.
const CAPTCHA_PAUSE_SECONDS: u64 = 300;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

/// The three kinds of title Yandex Books lists, each under its own path and id namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Book,
    Audiobook,
    Comic,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Book, Kind::Audiobook, Kind::Comic];

    /// The page path, and the id prefix that names the kind in a provider id.
    pub fn path(self) -> &'static str {
        match self {
            Kind::Book => "books",
            Kind::Audiobook => "audiobooks",
            Kind::Comic => "comicbooks",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Kind::Book => "book",
            Kind::Audiobook => "audiobook",
            Kind::Comic => "comicbook",
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Kind::Book => "book",
            Kind::Audiobook => "audiobook",
            Kind::Comic => "comic",
        }
    }

    /// The `__typename` of a search result of this kind.
    pub fn from_search_type(name: &str) -> Option<Kind> {
        match name {
            "TextBook" => Some(Kind::Book),
            "AudioBook" => Some(Kind::Audiobook),
            "ComicBook" => Some(Kind::Comic),
            _ => None,
        }
    }
}

/// A title on Yandex Books: its kind and the site's own id for it, such as `MWxAop5b`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    pub kind: Kind,
    pub uuid: String,
}

impl Title {
    /// The provider id: `book-MWxAop5b`, `audiobook-VTVT8GzC` or `comicbook-yjrd9dHo`. Books,
    /// audiobooks and comics are numbered separately, so the kind is part of the id.
    pub fn provider_id(&self) -> String {
        format!("{}-{}", self.kind.prefix(), self.uuid)
    }

    pub fn url(&self) -> String {
        format!("{SITE}/{}/{}", self.kind.path(), self.uuid)
    }

    /// A provider id, or a link to the title's page as someone might paste it.
    pub fn parse(value: &str) -> Option<Title> {
        let value = value.trim();
        if let Some((prefix, uuid)) = value.split_once('-') {
            if let Some(kind) = Kind::ALL.into_iter().find(|kind| kind.prefix() == prefix) {
                return valid_uuid(uuid).then(|| Title {
                    kind,
                    uuid: uuid.to_string(),
                });
            }
        }
        let path = value
            .strip_prefix(SITE)
            .unwrap_or(value)
            .split(['?', '#'])
            .next()
            .unwrap_or_default();
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let (kind, uuid) = (segments.next()?, segments.next()?);
        let kind = Kind::ALL.into_iter().find(|k| k.path() == kind)?;
        (segments.next().is_none() && valid_uuid(uuid)).then(|| Title {
            kind,
            uuid: uuid.to_string(),
        })
    }
}

pub fn valid_uuid(value: &str) -> bool {
    (4..=16).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The search page for a phrase. The site keeps the phrase in the path; `/search/` with a query
/// string is disallowed to robots, and shows the front page anyway.
pub fn search_url(phrase: &str) -> String {
    format!("{SITE}/search/all/{}", encode_path_segment(phrase))
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    /// Set when Yandex answers 429 or a captcha; nothing is sent to it again before then.
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

    /// The HTML of a page on Yandex Books, given by its public address.
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
            .header(ACCEPT_LANGUAGE, "ru-RU,ru;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(failed)?;

        if res.url().path().contains("captcha") {
            return Err(self.captcha());
        }
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
                    "books.yandex.ru answered {status}"
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
        let html = String::from_utf8_lossy(&body).into_owned();
        if is_captcha(&html) {
            return Err(self.captcha());
        }
        Ok(html)
    }

    /// A captcha is never solved: the provider stops asking for a while and says it is throttled.
    fn captcha(&self) -> UpstreamError {
        self.pause(Duration::from_secs(CAPTCHA_PAUSE_SECONDS));
        UpstreamError::RateLimited {
            retry_after_seconds: CAPTCHA_PAUSE_SECONDS,
        }
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

/// Yandex's SmartCaptcha page, which it serves in place of the one asked for. Only the start of
/// the page is read, cut back to a character boundary: the pages are mostly Cyrillic.
fn is_captcha(html: &str) -> bool {
    let mut end = html.len().min(64 << 10);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let head = &html[..end];
    head.contains("showcaptcha") || head.contains("SmartCaptcha") || head.contains("smart-captcha")
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_name_the_kind() {
        for (kind, id) in [
            (Kind::Book, "book-MWxAop5b"),
            (Kind::Audiobook, "audiobook-VTVT8GzC"),
            (Kind::Comic, "comicbook-yjrd9dHo"),
        ] {
            let uuid = id.split_once('-').unwrap().1.to_string();
            let title = Title { kind, uuid };
            assert_eq!(title.provider_id(), id);
            assert_eq!(Title::parse(id), Some(title));
        }
        assert_eq!(
            Title::parse("book-MWxAop5b").unwrap().url(),
            "https://books.yandex.ru/books/MWxAop5b"
        );
    }

    #[test]
    fn parse_takes_pasted_links() {
        for (input, want) in [
            (
                "https://books.yandex.ru/books/MWxAop5b",
                Some("book-MWxAop5b"),
            ),
            (
                "https://books.yandex.ru/audiobooks/VTVT8GzC?utm=x",
                Some("audiobook-VTVT8GzC"),
            ),
            ("/comicbooks/yjrd9dHo/", Some("comicbook-yjrd9dHo")),
            ("books/MWxAop5b", Some("book-MWxAop5b")),
            ("https://books.yandex.ru/books/MWxAop5b/read-online", None),
            (
                "https://books.yandex.ru/authors/mihail-bulgakov-NBpVBMXd",
                None,
            ),
            ("book-", None),
            ("book-../../x", None),
            ("serial-MWxAop5b", None),
            ("MWxAop5b", None),
            ("conformance-unknown-id", None),
        ] {
            assert_eq!(
                Title::parse(input).map(|t| t.provider_id()).as_deref(),
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn search_url_keeps_the_phrase_in_the_path() {
        assert_eq!(
            search_url("Мастер и Маргарита"),
            "https://books.yandex.ru/search/all/%D0%9C%D0%B0%D1%81%D1%82%D0%B5%D1%80%20%D0%B8%20%D0%9C%D0%B0%D1%80%D0%B3%D0%B0%D1%80%D0%B8%D1%82%D0%B0"
        );
        assert_eq!(
            search_url("a/b?c#d"),
            "https://books.yandex.ru/search/all/a%2Fb%3Fc%23d"
        );
    }

    #[test]
    fn captcha_pages_are_recognised() {
        assert!(is_captcha(
            r#"<form action="/checkcaptcha"><div class="SmartCaptcha-Anchor"></div></form>"#
        ));
        assert!(is_captcha(r#"<a href="/showcaptcha?retpath=x">"#));
        assert!(!is_captcha(include_str!("../testdata/book-MWxAop5b.html")));
        assert!(!is_captcha(include_str!(
            "../testdata/search-master-i-margarita.html"
        )));
        // A two-byte letter across the 64 KB mark, as on a real book page.
        let page = format!("{}з{}", "a".repeat((64 << 10) - 1), "a".repeat(10));
        assert!(!is_captcha(&page));
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
