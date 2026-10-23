use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::header::{HeaderValue, ACCEPT, COOKIE, LOCATION, RETRY_AFTER};
use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Subject pages run 100-150 KB; a long blurb with an excerpt can double that.
const MAX_BODY_BYTES: usize = 4 << 20;

/// Longer than this in the queue and the caller is told to come back later instead: BookOrbit
/// gives a provider 10 seconds by default.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(8);

/// How long the provider stops asking once Douban challenges it. Asking again sooner only
/// deepens the block on the address.
pub const BLOCKED_COOLDOWN: Duration = Duration::from_secs(600);

#[derive(Debug)]
pub enum DoubanError {
    NotFound,
    /// Douban answered 429, or the provider's own queue is too long.
    RateLimited {
        retry_after_seconds: u64,
    },
    /// Douban answered with a captcha, a login wall or a refusal instead of the page.
    Blocked {
        retry_after_seconds: u64,
    },
    Failed(String),
}

/// One `j/subject_suggest` entry. Douban mixes in other kinds than books (`type` "b").
#[derive(Debug, Default)]
pub struct Suggestion {
    pub id: String,
    pub kind: String,
    pub author_name: String,
}

/// What a subject page says, as written there; `mapping` turns it into a record.
#[derive(Debug, Default)]
pub struct Subject {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub authors: Vec<String>,
    pub translators: Vec<String>,
    pub publisher: String,
    pub published: String,
    pub pages: String,
    pub isbn: String,
    pub series: String,
    pub original_title: String,
    pub rating: String,
    pub votes: String,
    pub intro: Vec<String>,
    pub tags: Vec<String>,
}

struct Page {
    status: u16,
    location: String,
    body: String,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    cookie: Option<HeaderValue>,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    blocked_until: Mutex<Option<Instant>>,
}

impl Client {
    pub fn new(
        base: &str,
        user_agent: &str,
        cookie: Option<&str>,
        in_flight: usize,
        interval: Duration,
    ) -> Self {
        // Redirects are read, not followed: an ISBN answers with one, and so does a challenge.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        let cookie = cookie.map(|cookie| {
            let mut value =
                HeaderValue::from_str(cookie).expect("DOUBAN_COOKIE is not a valid header value");
            value.set_sensitive(true);
            value
        });
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            cookie,
            slots: Semaphore::new(in_flight.max(1)),
            interval,
            next: Mutex::new(Instant::now()),
            blocked_until: Mutex::new(None),
        }
    }

    pub async fn suggest(&self, query: &str) -> Result<Vec<Suggestion>, DoubanError> {
        let page = self
            .fetch(
                "suggest",
                "/j/subject_suggest",
                &[("q", query)],
                "application/json",
            )
            .await?;
        if page.status != 200 {
            return Err(match self.unexpected("suggest", &page) {
                DoubanError::NotFound => DoubanError::Failed("suggest: douban answered 404".into()),
                error => error,
            });
        }
        match serde_json::from_str::<Value>(&page.body) {
            Ok(Value::Array(entries)) => Ok(entries.iter().filter_map(suggestion).collect()),
            Ok(_) => Err(DoubanError::Failed("suggest answer is not a list".into())),
            Err(_) if looks_challenged(&page.body) => Err(self.block("suggest", 200)),
            Err(error) => Err(DoubanError::Failed(format!(
                "suggest answer unreadable: {error}"
            ))),
        }
    }

    /// The subject page for an id. A subject Douban merged into another answers with that one.
    pub async fn subject(&self, id: &str) -> Result<Subject, DoubanError> {
        let page = self.subject_page(id).await?;
        match page.status {
            200 => self.read_subject("subject", id, &page.body),
            301 | 302 | 303 | 307 | 308 => match subject_id(&page.location) {
                Some(target) if target != id => {
                    let page = self.subject_page(&target).await?;
                    match page.status {
                        200 => self.read_subject("subject", &target, &page.body),
                        _ => Err(self.unexpected("subject", &page)),
                    }
                }
                _ => Err(self.unexpected("subject", &page)),
            },
            _ => Err(self.unexpected("subject", &page)),
        }
    }

    /// Douban's `/isbn/{isbn}/` redirects to the subject that carries the ISBN, or answers 404.
    pub async fn subject_by_isbn(&self, isbn: &str) -> Result<Option<Subject>, DoubanError> {
        let page = self
            .fetch("isbn", &format!("/isbn/{isbn}/"), &[], "text/html")
            .await?;
        match page.status {
            301 | 302 | 303 | 307 | 308 => match subject_id(&page.location) {
                Some(id) => match self.subject(&id).await {
                    Ok(subject) => Ok(Some(subject)),
                    Err(DoubanError::NotFound) => Ok(None),
                    Err(error) => Err(error),
                },
                None => Err(self.unexpected("isbn", &page)),
            },
            404 => Ok(None),
            200 => {
                let id = OG_URL
                    .captures(&page.body)
                    .and_then(|c| subject_id(&c[1]))
                    .unwrap_or_default();
                self.read_subject("isbn", &id, &page.body).map(Some)
            }
            _ => Err(self.unexpected("isbn", &page)),
        }
    }

    async fn subject_page(&self, id: &str) -> Result<Page, DoubanError> {
        self.fetch("subject", &format!("/subject/{id}/"), &[], "text/html")
            .await
    }

    fn read_subject(&self, op: &str, id: &str, html: &str) -> Result<Subject, DoubanError> {
        match parse_subject(id, html) {
            Some(subject) if !id.is_empty() => Ok(subject),
            _ if looks_challenged(html) => Err(self.block(op, 200)),
            _ => Err(DoubanError::Failed(format!("{op}: not a subject page"))),
        }
    }

    /// Sorts every answer the callers do not expect: a missing page, a challenge, or a failure.
    fn unexpected(&self, op: &str, page: &Page) -> DoubanError {
        match page.status {
            404 => DoubanError::NotFound,
            200 if looks_challenged(&page.body) => self.block(op, 200),
            status => DoubanError::Failed(format!("{op}: douban answered {status}")),
        }
    }

    async fn fetch(
        &self,
        op: &str,
        path: &str,
        query: &[(&str, &str)],
        accept: &str,
    ) -> Result<Page, DoubanError> {
        let _permit = self.acquire().await?;
        // reqwest puts the URL in its errors, and the URL carries what someone is reading.
        let failed =
            |error: reqwest::Error| DoubanError::Failed(format!("{op}: {}", error.without_url()));
        let mut request = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header(ACCEPT, accept);
        if let Some(cookie) = &self.cookie {
            request = request.header(COOKIE, cookie.clone());
        }
        let mut res = request.send().await.map_err(failed)?;

        let status = res.status().as_u16();
        match status {
            429 => {
                let seconds = res
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|seconds| *seconds > 0)
                    .unwrap_or(60);
                self.pause(Duration::from_secs(seconds));
                eprintln!("[upstream] [fail] op={op} status=429 retryAfterSeconds={seconds} - douban is rate limiting");
                return Err(DoubanError::RateLimited {
                    retry_after_seconds: seconds,
                });
            }
            // 418 is what Douban answers a client it has decided is a bot.
            403 | 418 => return Err(self.block(op, status)),
            500..=599 => {
                return Err(DoubanError::Failed(format!(
                    "{op}: douban answered {status}"
                )))
            }
            _ => {}
        }
        let location = res
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if (300..400).contains(&status) && challenge_location(&location) {
            return Err(self.block(op, status));
        }

        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(failed)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(DoubanError::Failed(format!("{op}: answer too large")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Page {
            status,
            location,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    /// At most `in_flight` requests at once, each starting at least `interval` after the last,
    /// and none at all while Douban is challenging the provider.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>, DoubanError> {
        self.check_blocked()?;
        let start = {
            let mut next = self.next.lock().expect("pacer lock poisoned");
            let now = Instant::now();
            let start = (*next).max(now);
            let wait = start - now;
            if wait > MAX_QUEUE_WAIT {
                return Err(DoubanError::RateLimited {
                    retry_after_seconds: wait.as_secs_f64().ceil() as u64,
                });
            }
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|error| DoubanError::Failed(error.to_string()))?;
        // A challenge may have arrived while this request waited for its turn.
        self.check_blocked()?;
        Ok(permit)
    }

    fn check_blocked(&self) -> Result<(), DoubanError> {
        let until = *self.blocked_until.lock().expect("block lock poisoned");
        match until {
            Some(until) if until > Instant::now() => Err(DoubanError::Blocked {
                retry_after_seconds: seconds_until(until),
            }),
            _ => Ok(()),
        }
    }

    fn pause(&self, by: Duration) {
        let mut until = self.blocked_until.lock().expect("block lock poisoned");
        let candidate = Instant::now() + by;
        if until.is_none_or(|current| current < candidate) {
            *until = Some(candidate);
        }
    }

    fn block(&self, op: &str, status: u16) -> DoubanError {
        self.pause(BLOCKED_COOLDOWN);
        eprintln!(
            "[upstream] [fail] op={op} status={status} cooldownSeconds={} - douban challenged the request; pausing",
            BLOCKED_COOLDOWN.as_secs()
        );
        DoubanError::Blocked {
            retry_after_seconds: BLOCKED_COOLDOWN.as_secs(),
        }
    }

    #[cfg(test)]
    pub fn push_back(&self, by: Duration) {
        *self.next.lock().unwrap() = Instant::now() + by;
    }
}

fn seconds_until(until: Instant) -> u64 {
    until
        .saturating_duration_since(Instant::now())
        .as_secs_f64()
        .ceil()
        .max(1.0) as u64
}

fn suggestion(entry: &Value) -> Option<Suggestion> {
    let entry = entry.as_object()?;
    let field = |key: &str| match entry.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    Some(Suggestion {
        id: field("id"),
        kind: field("type"),
        author_name: field("author_name"),
    })
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

static SUBJECT_ID: LazyLock<Regex> = LazyLock::new(|| regex(r"/subject/(\d{1,12})(?:/|$)"));
static OG_URL: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"<meta property="og:url" content="([^"]+)""#));
static TITLE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?s)<span property="v:itemreviewed">(.*?)</span>"#));
static SUBTITLE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?s)<span property="v:subtitle">(.*?)</span>"#));
static INFO: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?s)<div id="info"[^>]*>(.*?)</div>"#));
static LINE_BREAK: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)<br\s*/?>"));
static LABEL: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?s)<span class="pl">(.*?)</span>"#));
static LINK_TEXT: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<a\b[^>]*>(.*?)</a>"));
static AVERAGE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"property="v:average"[^>]*>\s*([0-9.]+)\s*<"#));
static VOTES: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"property="v:votes"[^>]*>\s*([0-9]+)\s*<"#));
static LINKED_DATA: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#));
static PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<p\b[^>]*>(.*?)</p>"));
// The page no longer shows its tags, but still lists the top ones for its ad slots.
static AD_CRITERIA: LazyLock<Regex> = LazyLock::new(|| regex(r"criteria\s*=\s*'([^']*)'"));
static TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)<[^>]*>"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+"));

fn subject_id(url: &str) -> Option<String> {
    SUBJECT_ID.captures(url).map(|c| c[1].to_string())
}

fn challenge_location(location: &str) -> bool {
    [
        "sec.douban.com",
        "/misc/sorry",
        "accounts.douban.com",
        "/accounts/login",
        "passport/login",
    ]
    .iter()
    .any(|marker| location.contains(marker))
}

/// A page that is not the one asked for and says why: Douban's captcha, "abnormal requests from
/// your IP" and "access forbidden" pages.
pub fn looks_challenged(html: &str) -> bool {
    ["sec.douban.com", "异常请求", "禁止访问", "captcha"]
        .iter()
        .any(|marker| html.contains(marker))
}

/// Reads a subject page, or `None` when the page is not one.
pub fn parse_subject(id: &str, html: &str) -> Option<Subject> {
    let linked = LINKED_DATA
        .captures(html)
        .and_then(|c| serde_json::from_str::<Value>(c[1].trim()).ok())
        .unwrap_or(Value::Null);
    let mut subject = Subject {
        id: id.to_string(),
        title: TITLE
            .captures(html)
            .map(|c| inline_text(&c[1]))
            .filter(|title| !title.is_empty())
            .or_else(|| linked["name"].as_str().map(inline_text))
            .unwrap_or_default(),
        subtitle: SUBTITLE
            .captures(html)
            .map(|c| inline_text(&c[1]))
            .unwrap_or_default(),
        rating: AVERAGE
            .captures(html)
            .map(|c| c[1].to_string())
            .unwrap_or_default(),
        votes: VOTES
            .captures(html)
            .map(|c| c[1].to_string())
            .unwrap_or_default(),
        intro: intro(html),
        tags: ad_tags(html),
        ..Subject::default()
    };
    if subject.title.is_empty() {
        return None;
    }

    let info = INFO
        .captures(html)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    for row in LINE_BREAK.split(&info) {
        let Some(label) = LABEL.captures(row) else {
            continue;
        };
        let value = &row[label.get(0).expect("whole match").end()..];
        let label = inline_text(&label[1]);
        match label.trim_end_matches([':', '：']).trim() {
            "作者" => subject.authors = people(value),
            "译者" => subject.translators = people(value),
            // A co-published book lists each house; the first is the one on the spine.
            "出版社" => subject.publisher = people(value).into_iter().next().unwrap_or_default(),
            "出版年" => subject.published = row_text(value),
            "页数" => subject.pages = row_text(value),
            "ISBN" => subject.isbn = row_text(value),
            "丛书" => subject.series = row_text(value),
            "原作名" => subject.original_title = row_text(value),
            "副标题" if subject.subtitle.is_empty() => subject.subtitle = row_text(value),
            _ => {}
        }
    }

    if subject.authors.is_empty() {
        subject.authors = match &linked["author"] {
            Value::Array(people) => people
                .iter()
                .filter_map(|person| person["name"].as_str())
                .map(inline_text)
                .filter(|name| !name.is_empty())
                .collect(),
            _ => Vec::new(),
        };
    }
    if subject.isbn.is_empty() {
        subject.isbn = linked["isbn"].as_str().unwrap_or_default().to_string();
    }
    Some(subject)
}

/// The book's own blurb: the full text when Douban folds a long one behind "(展开全部)" (show all).
/// The author's biography further down uses the same markup, so the search stops at the next
/// section heading.
fn intro(html: &str) -> Vec<String> {
    let Some(start) = html.find(r#"id="link-report""#) else {
        return Vec::new();
    };
    let section = &html[start..];
    let section = &section[..section.find("<h2").unwrap_or(section.len())];
    let section = section
        .find(r#"class="all hidden""#)
        .map_or(section, |full| &section[full..]);
    let Some(intro_start) = section.find(r#"<div class="intro">"#) else {
        return Vec::new();
    };
    let intro = &section[intro_start..];
    let intro = &intro[..intro.find("</div>").unwrap_or(intro.len())];
    let mut paragraphs: Vec<String> = PARAGRAPH
        .captures_iter(intro)
        .map(|c| inline_text(&c[1]))
        .collect();
    if paragraphs.is_empty() {
        paragraphs = LINE_BREAK.split(intro).map(inline_text).collect();
    }
    paragraphs.retain(|p| !p.is_empty() && p != "(展开全部)");
    paragraphs
}

fn ad_tags(html: &str) -> Vec<String> {
    AD_CRITERIA
        .captures(html)
        .map(|c| {
            c[1].split('|')
                .filter_map(|entry| entry.strip_prefix("7:"))
                .map(inline_text)
                .filter(|tag| !tag.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Names in an info row: each is a link, separated by " / ", and an unlinked name is plain text.
fn people(value: &str) -> Vec<String> {
    let linked: Vec<String> = LINK_TEXT
        .captures_iter(value)
        .map(|c| inline_text(&c[1]))
        .filter(|name| !name.is_empty())
        .collect();
    if !linked.is_empty() {
        return linked;
    }
    row_text(value)
        .split('/')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

fn row_text(value: &str) -> String {
    inline_text(value)
        .trim_start_matches([':', '：', ' '])
        .trim()
        .to_string()
}

/// Plain text of an HTML fragment on one line. Tags go without a trace, since a space inside
/// Chinese text is a visible gap.
pub fn inline_text(html: &str) -> String {
    let text = LINE_BREAK.replace_all(html, " ");
    let text = TAG.replace_all(&text, "");
    let text = html_escape::decode_html_entities(&text).replace('\u{a0}', " ");
    SPACES.replace_all(&text, " ").trim().to_string()
}
