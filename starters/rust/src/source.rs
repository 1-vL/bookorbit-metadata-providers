//! The only file you need to change: describe your books, then fetch them from your source.

use std::future::Future;

use serde::Serialize;

/// One record in the shape BookOrbit reads. Leave a field empty when your source does not have it;
/// empty fields are left out of the answer.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub provider_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub media_type: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub narrators: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    pub series: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_number: Option<f64>,
}

/// What a client asked for, already trimmed.
#[derive(Debug, Default)]
pub struct Query {
    pub title: String,
    pub author: String,
    pub isbn: String,
    pub media_type: String,
    pub limit: usize,
}

#[derive(Debug)]
pub enum SourceError {
    /// Your source asked you to slow down; the client backs off instead of counting a failure.
    #[allow(dead_code)]
    // The example never throttles; a real source returns this on its own 429.
    RateLimited { retry_after_seconds: u64 },
    /// Anything else that went wrong. Never answer an empty list instead: that tells the client
    /// the book is not there.
    Failed(String),
}

/// Search returns matches best first, at most `query.limit` of them. Lookup returns `None` for an
/// id that does not exist.
pub trait Source: Send + Sync + 'static {
    fn search(
        &self,
        query: &Query,
    ) -> impl Future<Output = Result<Vec<BookMetadata>, SourceError>> + Send;
    fn lookup(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<BookMetadata>, SourceError>> + Send;
}

/// Answers from a list in memory. Replace it with calls to your source; add `reqwest` to
/// Cargo.toml for HTTP.
pub struct ExampleSource {
    books: Vec<BookMetadata>,
}

impl ExampleSource {
    pub fn new() -> Self {
        Self {
            books: vec![BookMetadata {
                provider_id: "ex-1".into(),
                media_type: "book".into(),
                title: "A Wizard of Earthsea".into(),
                authors: vec!["Ursula K. Le Guin".into()],
                published_year: "1968".into(),
                series: vec![Series {
                    series: "Earthsea Cycle".into(),
                    sequence_number: Some(1.0),
                }],
                rating: Some(4.2),
                rating_count: Some(1200),
                url: "https://example.org/books/ex-1".into(),
                ..BookMetadata::default()
            }],
        }
    }
}

impl Source for ExampleSource {
    async fn search(&self, query: &Query) -> Result<Vec<BookMetadata>, SourceError> {
        // This source has no audiobooks, so it answers none rather than offering a printed book.
        if query.media_type == "audiobook" {
            return Ok(Vec::new());
        }
        let title = query.title.to_lowercase();
        let author = query.author.to_lowercase();
        Ok(self
            .books
            .iter()
            .filter(|book| title.is_empty() || book.title.to_lowercase().contains(&title))
            .filter(|book| {
                author.is_empty()
                    || book
                        .authors
                        .iter()
                        .any(|name| name.to_lowercase().contains(&author))
            })
            .take(query.limit)
            .cloned()
            .collect())
    }

    async fn lookup(&self, id: &str) -> Result<Option<BookMetadata>, SourceError> {
        Ok(self
            .books
            .iter()
            .find(|book| book.provider_id == id)
            .cloned())
    }
}
