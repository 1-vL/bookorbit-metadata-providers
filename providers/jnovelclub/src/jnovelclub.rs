use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;

/// A page of 250 series is about 500 KB.
const MAX_BODY_BYTES: usize = 8 << 20;

/// Longer than this in the pacer's queue and the caller is told to come back later instead.
const MAX_QUEUE_WAIT: Duration = Duration::from_secs(15);

const SERIES_PAGE_SIZE: usize = 250;
/// 10,000 series; J-Novel Club has about 500.
const MAX_SERIES_PAGES: usize = 40;
/// A stale series list is kept after a failed refresh, and retried no sooner than this.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(300);
/// Series whose volume lists are kept, so a library of one series' volumes costs one request.
const MAX_CACHED_SERIES: usize = 64;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

impl UpstreamError {
    pub fn message(&self) -> String {
        match self {
            Self::NotFound => "not found".into(),
            Self::RateLimited {
                retry_after_seconds,
            } => format!("rate limited for {retry_after_seconds}s"),
            Self::Failed(message) => message.clone(),
        }
    }
}

// The API renders protobuf as JSON: unset text, numbers and lists arrive as "", 0 and [], and
// only messages (a cover, a timestamp) can be missing or null.

/// The part of a J-Novel Club series the provider reads; serde skips the rest.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Series {
    pub id: String,
    /// `NOVEL` or `MANGA`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub original_title: String,
    pub slug: String,
    pub hidden: bool,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Volume {
    pub id: String,
    pub title: String,
    pub short_title: String,
    pub number: f64,
    pub position: f64,
    pub creators: Vec<Creator>,
    pub hidden: bool,
    pub publishing: Option<String>,
    pub physical_publishing: Option<String>,
    pub no_ebook: bool,
    pub description: String,
    pub cover: Option<Cover>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Creator {
    pub name: String,
    pub role: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Cover {
    pub original_url: String,
    pub cover_url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Deserialize)]
struct SeriesPage {
    #[serde(default)]
    series: Vec<Series>,
    pagination: Option<Pagination>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Pagination {
    last_page: bool,
}

#[derive(Deserialize)]
struct VolumesBody {
    #[serde(default)]
    volumes: Vec<Volume>,
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

    async fn series_page(&self, skip: usize) -> Result<SeriesPage, UpstreamError> {
        let (skip, limit) = (skip.to_string(), SERIES_PAGE_SIZE.to_string());
        self.get("/series", &[("skip", &skip), ("limit", &limit)])
            .await
    }

    async fn volumes(&self, series_id: &str) -> Result<Vec<Volume>, UpstreamError> {
        let body: VolumesBody = self
            .get(&format!("/series/{}/volumes", safe_id(series_id)?), &[])
            .await?;
        Ok(body.volumes)
    }

    async fn volume(&self, id: &str) -> Result<Volume, UpstreamError> {
        self.get(&format!("/volumes/{}", safe_id(id)?), &[]).await
    }

    async fn volume_series(&self, id: &str) -> Result<Series, UpstreamError> {
        self.get(&format!("/volumes/{}/serie", safe_id(id)?), &[])
            .await
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, UpstreamError> {
        self.pacer.wait().await?;
        // Without format=json the API answers protobuf, whatever the Accept header says.
        let mut res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(&[("format", "json")])
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
                    "j-novel club answered {status}"
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
                return Err(UpstreamError::Failed(
                    "j-novel club answer too large".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|error| {
            UpstreamError::Failed(format!("j-novel club answer unreadable: {error}"))
        })
    }
}

/// Ids go into the request path, so only the API's own shape is let through.
fn safe_id(id: &str) -> Result<&str, UpstreamError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then_some(id).ok_or(UpstreamError::NotFound)
}

#[derive(Clone)]
struct Snapshot {
    at: Instant,
    series: Arc<Vec<Series>>,
}

struct CachedVolumes {
    at: Instant,
    volumes: Arc<Vec<Volume>>,
}

/// J-Novel Club has no text search, so its whole series list is kept here and matched locally,
/// refreshed in the background; volume lists are fetched per series and kept for a while.
pub struct Catalogue {
    client: Arc<Client>,
    refresh_every: Duration,
    volumes_ttl: Duration,
    current: RwLock<Option<Snapshot>>,
    loading: Arc<tokio::sync::Mutex<()>>,
    failed_at: Mutex<Option<Instant>>,
    volumes: Mutex<HashMap<String, CachedVolumes>>,
}

impl Catalogue {
    pub fn new(client: Arc<Client>, refresh_every: Duration, volumes_ttl: Duration) -> Self {
        Self {
            client,
            refresh_every,
            volumes_ttl,
            current: RwLock::new(None),
            loading: Arc::new(tokio::sync::Mutex::new(())),
            failed_at: Mutex::new(None),
            volumes: Mutex::new(HashMap::new()),
        }
    }

    pub fn request_interval(&self) -> Duration {
        self.client.pacer.interval
    }

    /// The series list. A stale one is answered at once and refreshed behind it; only the very
    /// first load makes a caller wait.
    pub async fn series(self: &Arc<Self>) -> Result<Arc<Vec<Series>>, UpstreamError> {
        if let Some(snapshot) = self.snapshot() {
            if snapshot.at.elapsed() >= self.refresh_every && !self.failed_recently() {
                if let Ok(guard) = self.loading.clone().try_lock_owned() {
                    let this = self.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        let _ = this.reload().await;
                    });
                }
            }
            return Ok(snapshot.series);
        }
        let _guard = self.loading.lock().await;
        if let Some(snapshot) = self.snapshot() {
            return Ok(snapshot.series);
        }
        self.reload().await
    }

    fn snapshot(&self) -> Option<Snapshot> {
        self.current
            .read()
            .expect("catalogue lock poisoned")
            .clone()
    }

    fn failed_recently(&self) -> bool {
        self.failed_at
            .lock()
            .expect("catalogue lock poisoned")
            .is_some_and(|at| at.elapsed() < RETRY_AFTER_FAILURE)
    }

    async fn reload(&self) -> Result<Arc<Vec<Series>>, UpstreamError> {
        let started = Instant::now();
        match self.fetch_all().await {
            Ok(series) => {
                let series = Arc::new(series);
                eprintln!(
                    "[catalogue] [end] series={} durationMs={} - series list loaded",
                    series.len(),
                    started.elapsed().as_millis()
                );
                *self.current.write().expect("catalogue lock poisoned") = Some(Snapshot {
                    at: Instant::now(),
                    series: series.clone(),
                });
                *self.failed_at.lock().expect("catalogue lock poisoned") = None;
                Ok(series)
            }
            Err(err) => {
                eprintln!(
                    "[catalogue] [fail] durationMs={} error={:?} - series list unavailable",
                    started.elapsed().as_millis(),
                    err.message()
                );
                *self.failed_at.lock().expect("catalogue lock poisoned") = Some(Instant::now());
                Err(err)
            }
        }
    }

    async fn fetch_all(&self) -> Result<Vec<Series>, UpstreamError> {
        let mut all = Vec::new();
        let mut seen = HashSet::new();
        let mut skip = 0;
        for _ in 0..MAX_SERIES_PAGES {
            let page = self.client.series_page(skip).await?;
            let count = page.series.len();
            // Paging by offset can repeat a series when one is added mid-way.
            all.extend(
                page.series
                    .into_iter()
                    .filter(|series| seen.insert(series.id.clone())),
            );
            if count == 0 || page.pagination.is_none_or(|p| p.last_page) {
                break;
            }
            skip += count;
        }
        if all.is_empty() {
            return Err(UpstreamError::Failed(
                "j-novel club listed no series".into(),
            ));
        }
        Ok(all)
    }

    pub async fn volumes(&self, series_id: &str) -> Result<Arc<Vec<Volume>>, UpstreamError> {
        if let Some(cached) = self.cached_volumes(series_id) {
            return Ok(cached);
        }
        let volumes = Arc::new(self.client.volumes(series_id).await?);
        let mut cache = self.volumes.lock().expect("volume cache lock poisoned");
        cache.retain(|_, entry| entry.at.elapsed() < self.volumes_ttl);
        if cache.len() >= MAX_CACHED_SERIES {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(id, _)| id.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            series_id.to_string(),
            CachedVolumes {
                at: Instant::now(),
                volumes: volumes.clone(),
            },
        );
        Ok(volumes)
    }

    fn cached_volumes(&self, series_id: &str) -> Option<Arc<Vec<Volume>>> {
        let cache = self.volumes.lock().expect("volume cache lock poisoned");
        cache
            .get(series_id)
            .filter(|entry| entry.at.elapsed() < self.volumes_ttl)
            .map(|entry| entry.volumes.clone())
    }

    /// One volume and the series it belongs to, from a recent search when there was one.
    pub async fn volume(&self, id: &str) -> Result<(Volume, Option<Series>), UpstreamError> {
        if let Some(found) = self.cached_volume(id) {
            return Ok(found);
        }
        let volume = self.client.volume(id).await?;
        let series = match self.client.volume_series(id).await {
            Ok(series) => Some(series),
            Err(UpstreamError::NotFound) => None,
            Err(err) => return Err(err),
        };
        Ok((volume, series))
    }

    fn cached_volume(&self, id: &str) -> Option<(Volume, Option<Series>)> {
        let (series_id, volume) = {
            let cache = self.volumes.lock().expect("volume cache lock poisoned");
            cache
                .iter()
                .filter(|(_, entry)| entry.at.elapsed() < self.volumes_ttl)
                .find_map(|(series_id, entry)| {
                    let volume = entry.volumes.iter().find(|v| v.id == id)?;
                    Some((series_id.clone(), volume.clone()))
                })?
        };
        let series = self
            .snapshot()?
            .series
            .iter()
            .find(|s| s.id == series_id)
            .cloned()?;
        Some((volume, Some(series)))
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
