use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;

/// A search answer carries every upstream source's raw record, about 30 KB a series.
const MAX_BODY_BYTES: usize = 16 << 20;

/// Longer than this in the pacer's queue and the caller is told to come back later instead.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

/// The part of a MangaBaka series record the provider reads; serde skips the rest.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Series {
    pub id: u64,
    pub state: String,
    /// `manga`, `manhwa`, `manhua`, `novel` and the like.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub merged_with: Option<u64>,
    pub canonical_url: Option<String>,
    pub title: Option<String>,
    pub romanized_title: Option<String>,
    pub native_title: Option<String>,
    pub titles: Option<Vec<Title>>,
    pub cover: Option<Cover>,
    pub authors: Option<Vec<String>>,
    pub artists: Option<Vec<String>>,
    pub description: Option<String>,
    pub year: Option<i64>,
    pub published: Option<Published>,
    pub original_language: Option<String>,
    pub rating: Option<f64>,
    pub publishers: Option<Vec<Publisher>>,
    pub genres: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
}

/// One of a series' names in one language: official, alternative or a fan translation.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Title {
    pub language: Option<String>,
    pub title: Option<String>,
    pub traits: Option<Vec<String>>,
    pub is_primary: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Cover {
    pub raw: Option<CoverImage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CoverImage {
    pub url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Published {
    pub start_date: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Publisher {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Deserialize)]
struct SearchBody {
    #[serde(default)]
    data: Vec<Series>,
}

#[derive(Deserialize)]
struct SeriesBody {
    data: Option<Series>,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    search: Pacer,
    lookup: Pacer,
}

impl Client {
    pub fn new(
        base: &str,
        user_agent: &str,
        search_interval: Duration,
        lookup_interval: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            search: Pacer::new(search_interval),
            lookup: Pacer::new(lookup_interval),
        }
    }

    pub async fn search_series(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Series>, UpstreamError> {
        let limit = limit.to_string();
        let body: SearchBody = self
            .get(
                &self.search,
                "/v1/series/search",
                &[("q", query), ("limit", &limit)],
            )
            .await?;
        Ok(body.data)
    }

    pub async fn series(&self, id: u64) -> Result<Series, UpstreamError> {
        let body: SeriesBody = self
            .get(&self.lookup, &format!("/v1/series/{id}"), &[])
            .await?;
        body.data.ok_or(UpstreamError::NotFound)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        pacer: &Pacer,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, UpstreamError> {
        pacer.wait().await?;
        let mut res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?;

        match res.status().as_u16() {
            404 => return Err(UpstreamError::NotFound),
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
            status => {
                return Err(UpstreamError::Failed(format!(
                    "mangabaka answered {status}"
                )))
            }
        }

        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?
        {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(UpstreamError::Failed("mangabaka answer too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|error| UpstreamError::Failed(format!("mangabaka answer unreadable: {error}")))
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
