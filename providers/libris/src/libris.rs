use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// Every hit carries its holdings: about 100 KB for an edition in many libraries.
const MAX_BODY_BYTES: usize = 16 << 20;

/// Longer than this in the pacer's queue and the caller is told to come back later instead.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(15);

const ISBN_SEARCH_LIMIT: &str = "10";

/// Work categories that make a record an audiobook: the Ljudböcker and Talböcker genre terms, and
/// MARC's literary-text type, which only sound recordings carry. KB's spoken-word content type and
/// audio category are not enough on their own: radio programmes and podcasts carry them too.
pub const AUDIOBOOK_CATEGORIES: &[&str] = &[
    "https://id.kb.se/term/saogf/Ljudb%C3%B6cker",
    "https://id.kb.se/term/barngf/Ljudb%C3%B6cker",
    "https://id.kb.se/term/saogf/Talb%C3%B6cker",
    "https://id.kb.se/marc/Fiction",
];
pub const TEXT_CONTENT: &str = "https://id.kb.se/term/rda/Text";
/// A chapter or article catalogued on its own.
pub const COMPONENT_PART: &str = "https://id.kb.se/term/saobf/ComponentPart";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Book,
    Audiobook,
}

impl Media {
    pub fn as_str(self) -> &'static str {
        match self {
            Media::Book => "book",
            Media::Audiobook => "audiobook",
        }
    }
}

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

/// A JSON-LD property: absent, one value or a list. A value of an unexpected shape is skipped
/// instead of failing the record, since records of different ages shape the same property
/// differently.
#[derive(Debug, Clone)]
pub struct Many<T>(pub Vec<T>);

impl<T> Default for Many<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Deref for Many<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Many<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::Null => Vec::new(),
            serde_json::Value::Array(values) => values,
            value => vec![value],
        };
        Ok(Self(
            values
                .into_iter()
                .filter_map(|value| T::deserialize(value).ok())
                .collect(),
        ))
    }
}

impl Many<String> {
    pub fn first_text(&self) -> &str {
        self.iter()
            .map(|value| value.trim())
            .find(|value| !value.is_empty())
            .unwrap_or_default()
    }

    pub fn has(&self, wanted: &str) -> bool {
        self.iter().any(|value| value == wanted)
    }
}

/// The part of a Libris instance (one edition) the provider reads; serde skips the rest,
/// holdings included.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Instance {
    #[serde(rename = "@id")]
    pub id: Many<String>,
    #[serde(rename = "hasTitle")]
    pub titles: Many<Title>,
    #[serde(rename = "instanceOf")]
    pub work: Many<Work>,
    pub contribution: Many<Contribution>,
    pub publication: Many<Publication>,
    #[serde(rename = "identifiedBy")]
    pub identifiers: Many<Identifier>,
    pub extent: Many<Labelled>,
    pub summary: Many<Labelled>,
    #[serde(rename = "hasNote")]
    pub notes: Many<Labelled>,
    #[serde(rename = "seriesMembership")]
    pub series: Many<SeriesMembership>,
    pub category: Many<Term>,
    #[serde(rename = "isPrimaryTopicOf")]
    pub documents: Many<Document>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Work {
    #[serde(rename = "hasTitle")]
    pub titles: Many<Title>,
    pub contribution: Many<Contribution>,
    pub category: Many<Term>,
    #[serde(rename = "genreForm")]
    pub genre_form: Many<Term>,
    pub subject: Many<Term>,
    pub language: Many<Term>,
    pub summary: Many<Labelled>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Title {
    #[serde(rename = "@type")]
    pub kind: Many<String>,
    #[serde(rename = "mainTitle")]
    pub main: Many<String>,
    #[serde(rename = "mainTitleByLang")]
    pub main_by_lang: Many<BTreeMap<String, Many<String>>>,
    pub subtitle: Many<String>,
    #[serde(rename = "titleRemainder")]
    pub remainder: Many<String>,
    #[serde(rename = "hasPart")]
    pub parts: Many<TitlePart>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TitlePart {
    #[serde(rename = "partNumber")]
    pub number: Many<String>,
    #[serde(rename = "partName")]
    pub name: Many<String>,
}

/// A linked term: a category, genre, subject, language or role.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Term {
    #[serde(rename = "@id")]
    pub id: Many<String>,
    pub code: Many<String>,
    #[serde(rename = "langTag")]
    pub lang_tag: Many<String>,
    #[serde(rename = "prefLabel")]
    pub label: Many<String>,
    #[serde(rename = "prefLabelByLang")]
    pub label_by_lang: Many<BTreeMap<String, Many<String>>>,
    #[serde(rename = "sameAs")]
    pub same_as: Many<Link>,
}

impl Term {
    pub fn is(&self, id: &str) -> bool {
        self.id.has(id)
    }

    /// The Swedish label, which is the catalogue's own language.
    pub fn label(&self) -> &str {
        let label = self.label.first_text();
        if !label.is_empty() {
            return label;
        }
        let by_lang = self.label_by_lang.first();
        ["sv", "en"]
            .iter()
            .filter_map(|lang| by_lang.and_then(|labels| labels.get(*lang)))
            .map(Many::first_text)
            .find(|label| !label.is_empty())
            .unwrap_or_default()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Link {
    #[serde(rename = "@id")]
    pub id: Many<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Contribution {
    #[serde(rename = "@type")]
    pub kind: Many<String>,
    pub agent: Many<Agent>,
    pub role: Many<Term>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Agent {
    #[serde(rename = "@type")]
    pub kind: Many<String>,
    #[serde(rename = "givenName")]
    pub given_name: Many<String>,
    #[serde(rename = "familyName")]
    pub family_name: Many<String>,
    pub name: Many<String>,
    pub label: Many<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Publication {
    #[serde(rename = "@type")]
    pub kind: Many<String>,
    pub agent: Many<Agent>,
    pub year: Many<serde_json::Value>,
    pub date: Many<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Identifier {
    #[serde(rename = "@type")]
    pub kind: Many<String>,
    pub value: Many<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Labelled {
    pub label: Many<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SeriesMembership {
    #[serde(rename = "seriesStatement")]
    pub statement: Many<String>,
    #[serde(rename = "seriesEnumeration")]
    pub enumeration: Many<String>,
    #[serde(rename = "inSeries")]
    pub in_series: Many<Series>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Series {
    #[serde(rename = "hasTitle")]
    pub titles: Many<Title>,
    #[serde(rename = "instanceOf")]
    pub work: Many<Work>,
}

/// A document about the record, such as its cover image.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Document {
    pub uri: Many<String>,
    #[serde(rename = "cataloguersNote")]
    pub cataloguers_note: Many<String>,
    #[serde(rename = "marc:publicNote")]
    pub public_note: Many<String>,
}

#[derive(Deserialize)]
struct FindBody {
    #[serde(default)]
    items: Vec<Instance>,
}

#[derive(Deserialize)]
struct RecordBody {
    #[serde(rename = "mainEntity", default)]
    main_entity: Option<Instance>,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    pacer: Pacer,
}

impl Client {
    pub fn new(base: &str, user_agent: &str, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            pacer: Pacer::new(interval),
        }
    }

    /// Editions of one medium matching every word, in Libris's relevance order.
    pub async fn search(
        &self,
        terms: &str,
        media: Media,
        limit: usize,
    ) -> Result<Vec<Instance>, UpstreamError> {
        let limit = limit.to_string();
        let mut query = vec![
            ("q", terms),
            ("@type", "Instance"),
            ("_limit", limit.as_str()),
            ("_stats", "false"),
            ("not-category.@id", COMPONENT_PART),
        ];
        match media {
            Media::Book => query.push(("instanceOf.category.@id", TEXT_CONTENT)),
            // Repeating a filter matches any of its values.
            Media::Audiobook => query.extend(
                AUDIOBOOK_CATEGORIES
                    .iter()
                    .map(|id| ("instanceOf.category.@id", *id)),
            ),
        }
        self.find(&query).await
    }

    /// Editions listing any of these ISBNs, or mentioning them anywhere: the caller checks.
    pub async fn search_isbn(&self, isbns: &[String]) -> Result<Vec<Instance>, UpstreamError> {
        let terms = isbns.join(" | ");
        self.find(&[
            ("q", terms.as_str()),
            ("@type", "Instance"),
            ("_limit", ISBN_SEARCH_LIMIT),
            ("_stats", "false"),
        ])
        .await
    }

    /// One edition by its record id, read from the record itself rather than the search index:
    /// only the record carries the cover link.
    pub async fn instance(&self, id: &str) -> Result<Instance, UpstreamError> {
        let body: RecordBody = self.get(&format!("/{id}/data.json"), &[], true).await?;
        body.main_entity
            .filter(|instance| !instance.work.is_empty())
            .ok_or(UpstreamError::NotFound)
    }

    async fn find(&self, query: &[(&str, &str)]) -> Result<Vec<Instance>, UpstreamError> {
        let body: FindBody = self.get("/find.jsonld", query, false).await?;
        Ok(body.items)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        record: bool,
    ) -> Result<T, UpstreamError> {
        self.pacer.wait().await?;
        let mut res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("Accept", "application/ld+json, application/json")
            .send()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?;

        match res.status().as_u16() {
            // A missing record is gone; a missing search endpoint is a failure, never "no results".
            404 | 410 if record => return Err(UpstreamError::NotFound),
            429 => {
                let retry_after_seconds = res
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|seconds| *seconds > 0)
                    .unwrap_or(60);
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds,
                });
            }
            200..=299 => {}
            status => return Err(UpstreamError::Failed(format!("libris answered {status}"))),
        }

        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?
        {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(UpstreamError::Failed("libris answer too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|error| UpstreamError::Failed(format!("libris answer unreadable: {error}")))
    }
}

/// Spaces request starts at least `interval` apart, across all callers.
pub struct Pacer {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(Instant::now()),
        }
    }

    pub async fn wait(&self) -> Result<(), UpstreamError> {
        if self.interval.is_zero() {
            return Ok(());
        }
        let start = {
            let mut next = self.next.lock().expect("pacer lock poisoned");
            let now = Instant::now();
            let start = (*next).max(now);
            let wait = start - now;
            if wait > MAX_QUEUE_WAIT {
                return Err(UpstreamError::RateLimited {
                    retry_after_seconds: wait.as_secs_f64().ceil() as u64,
                });
            }
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        Ok(())
    }

    #[cfg(test)]
    pub fn push_back(&self, by: Duration) {
        *self.next.lock().unwrap() = Instant::now() + by;
    }
}
