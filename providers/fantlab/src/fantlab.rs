use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Map, Value};
use tokio::sync::{Semaphore, SemaphorePermit};

const MAX_BODY_BYTES: usize = 4 << 20;
/// A cycle's reading order changes when fantlab adds a book to it, which is rare.
const CYCLE_ORDER_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug)]
pub enum FantlabError {
    NotFound,
    RateLimited { retry_after: String },
    Unavailable(String),
}

pub type Object = Map<String, Value>;

pub struct WorkHit {
    pub work_id: i64,
    pub type_slug: String,
    pub author_names: String,
    pub author_ru_names: String,
}

pub struct EditionHit {
    pub edition_id: i64,
    pub isbn: String,
}

#[derive(Debug, Default)]
pub struct Author {
    pub name: String,
    pub name_orig: String,
    pub kind: String,
}

#[derive(Debug, Default)]
pub struct Genre {
    pub label: String,
    pub percent: f64,
    pub children: Vec<Genre>,
}

#[derive(Debug, Default)]
pub struct GenreGroup {
    pub id: i64,
    pub genres: Vec<Genre>,
}

#[derive(Debug, Default)]
pub struct Work {
    pub id: i64,
    pub name: String,
    pub name_orig: String,
    pub year: i64,
    pub lang_code: String,
    pub image: String,
    pub description: String,
    pub authors: Vec<Author>,
    pub rating: f64,
    pub voters: i64,
    pub genre_groups: Vec<GenreGroup>,
    /// The nearest cycle of each chain the work belongs to.
    pub cycles: Vec<Cycle>,
    /// The first chain, nearest cycle first, for finding the one that numbers the work.
    pub first_chain: Vec<Cycle>,
}

/// The cycle a work belongs to, nearest first.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Cycle {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Edition {
    pub id: i64,
    pub name: String,
    pub year: i64,
    pub pages: i64,
    pub lang_code: String,
    pub image: String,
    pub description: String,
    pub isbns: Vec<String>,
    pub authors: Vec<String>,
    pub publishers: Vec<String>,
    pub work_ids: Vec<i64>,
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    cycle_orders: Mutex<HashMap<i64, (Instant, Vec<i64>)>>,
}

impl Client {
    pub fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
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
            cycle_orders: Mutex::new(HashMap::new()),
        }
    }

    /// Where a work sits in its cycle's own reading order, counting from 1. Only the main line
    /// counts: fantlab marks side stories, comics and the like as extras, and nests sub-cycles.
    pub async fn cycle_position(
        &self,
        cycle_id: i64,
        work_id: i64,
    ) -> Result<Option<usize>, FantlabError> {
        let cached = {
            let orders = self.cycle_orders.lock().expect("cycle cache lock poisoned");
            orders
                .get(&cycle_id)
                .filter(|(at, _)| at.elapsed() < CYCLE_ORDER_TTL)
                .map(|(_, order)| order.clone())
        };
        let order = match cached {
            Some(order) => order,
            None => {
                let order =
                    cycle_order(&self.get(&format!("/work/{cycle_id}/extended"), &[]).await?);
                self.cycle_orders
                    .lock()
                    .expect("cycle cache lock poisoned")
                    .insert(cycle_id, (Instant::now(), order.clone()));
                order
            }
        };
        Ok(order
            .iter()
            .position(|id| *id == work_id)
            .map(|index| index + 1))
    }

    pub async fn search_works(&self, q: &str) -> Result<Vec<WorkHit>, FantlabError> {
        let raw = self
            .get("/search-works", &[("page", "1"), ("q", q)])
            .await?;
        Ok(array(&raw, "matches")
            .iter()
            .filter_map(Value::as_object)
            .map(|hit| WorkHit {
                work_id: flex_int(hit.get("work_id")),
                type_slug: string(hit, "name_eng"),
                author_names: string(hit, "all_autor_name"),
                author_ru_names: string(hit, "all_autor_rusname"),
            })
            .collect())
    }

    pub async fn search_editions(&self, q: &str) -> Result<Vec<EditionHit>, FantlabError> {
        let raw = self
            .get("/search-editions", &[("page", "1"), ("q", q)])
            .await?;
        Ok(array(&raw, "matches")
            .iter()
            .filter_map(Value::as_object)
            .map(|hit| EditionHit {
                edition_id: flex_int(hit.get("edition_id")),
                isbn: string(hit, "isbn"),
            })
            .collect())
    }

    pub async fn work(&self, id: i64) -> Result<Work, FantlabError> {
        Ok(parse_work(
            &self.get(&format!("/work/{id}/extended"), &[]).await?,
        ))
    }

    pub async fn edition(&self, id: i64) -> Result<Edition, FantlabError> {
        Ok(parse_edition(
            &self.get(&format!("/edition/{id}/extended"), &[]).await?,
        ))
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Object, FantlabError> {
        let _permit = self.acquire().await?;
        let unavailable = |error: reqwest::Error| FantlabError::Unavailable(error.to_string());
        let mut res = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(unavailable)?;

        match res.status().as_u16() {
            404 => return Err(FantlabError::NotFound),
            429 => {
                let retry_after = res
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("60")
                    .to_string();
                return Err(FantlabError::RateLimited { retry_after });
            }
            200 => {}
            status => return Err(FantlabError::Unavailable(format!("status {status}"))),
        }

        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(unavailable)? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY_BYTES {
                return Err(FantlabError::Unavailable(format!(
                    "response over {MAX_BODY_BYTES} bytes"
                )));
            }
        }
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(raw)) => Ok(raw),
            Ok(_) => Ok(Object::new()),
            Err(error) => Err(FantlabError::Unavailable(format!("invalid JSON: {error}"))),
        }
    }

    /// At most `concurrency` requests in flight, each starting at least `interval` after the last.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>, FantlabError> {
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|error| FantlabError::Unavailable(error.to_string()))?;
        let start = {
            let mut next = self.next.lock().expect("limiter lock poisoned");
            let start = (*next).max(Instant::now());
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        Ok(permit)
    }
}

/// The main-line works of a cycle, in fantlab's order: direct children that are neither marked as
/// extras (`plus`) nor sub-cycles.
pub fn cycle_order(raw: &Object) -> Vec<i64> {
    const SUBCYCLE: i64 = 4;
    array(raw, "children")
        .iter()
        .filter_map(Value::as_object)
        .filter(|child| flex_int(child.get("deep")) <= 1)
        .filter(|child| flex_int(child.get("plus")) == 0)
        .filter(|child| flex_int(child.get("work_type_id")) != SUBCYCLE)
        .map(|child| flex_int(child.get("work_id")))
        .filter(|id| *id > 0)
        .collect()
}

/// Reads every field on its own because the API is PHP-backed: an empty object can arrive as `[]`
/// and a number as a string, and one odd field must not lose the whole record.
pub fn parse_work(raw: &Object) -> Work {
    let rating = object(raw, "rating");
    let groups = object(raw, "classificatory")
        .map(|c| array(c, "genre_group"))
        .unwrap_or_default();
    // fantlab lists a chain's cycles root-first for some works and root-last for others, so the
    // root saga is what says which end is the root.
    let root = array(raw, "work_root_saga")
        .first()
        .and_then(Value::as_object)
        .map(|saga| flex_int(saga.get("work_id")))
        .unwrap_or_default();
    let chains: Vec<Vec<Cycle>> = object(raw, "parents")
        .map(|p| array(p, "cycles"))
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_array)
        .map(|chain| {
            let cycles: Vec<Cycle> = chain
                .iter()
                .filter_map(Value::as_object)
                .map(|cycle| Cycle {
                    id: flex_int(cycle.get("work_id")),
                    name: string(cycle, "work_name"),
                })
                .filter(|cycle| !cycle.name.is_empty())
                .collect();
            let (mut nearest, roots): (Vec<Cycle>, Vec<Cycle>) =
                cycles.into_iter().partition(|c| c.id != root);
            if chain
                .first()
                .and_then(Value::as_object)
                .is_some_and(|first| flex_int(first.get("work_id")) == root)
            {
                // Root-first: the cycle next to the work is the last one.
                nearest.reverse();
            }
            nearest.extend(roots);
            nearest
        })
        .filter(|chain| !chain.is_empty())
        .collect();
    Work {
        id: flex_int(raw.get("work_id")),
        year: flex_int(raw.get("work_year")),
        name: real_title(&string(raw, "work_name")),
        name_orig: real_title(&string(raw, "work_name_orig")),
        lang_code: string(raw, "lang_code"),
        image: string(raw, "image"),
        description: string(raw, "work_description"),
        authors: array(raw, "authors")
            .iter()
            .filter_map(Value::as_object)
            .map(|a| Author {
                name: string(a, "name"),
                name_orig: string(a, "name_orig"),
                kind: string(a, "type"),
            })
            .collect(),
        rating: rating.map(|r| flex_float(r.get("rating"))).unwrap_or(0.0),
        voters: rating.map(|r| flex_int(r.get("voters"))).unwrap_or(0),
        genre_groups: groups
            .iter()
            .filter_map(Value::as_object)
            .map(|g| GenreGroup {
                id: flex_int(g.get("genre_group_id")),
                genres: genres(g),
            })
            .collect(),
        cycles: chains
            .iter()
            .filter_map(|chain| chain.first().cloned())
            .collect(),
        first_chain: chains.into_iter().next().unwrap_or_default(),
    }
}

fn genres(parent: &Object) -> Vec<Genre> {
    array(parent, "genre")
        .iter()
        .filter_map(Value::as_object)
        .map(|g| Genre {
            label: string(g, "label"),
            percent: flex_float(g.get("percent")),
            children: genres(g),
        })
        .collect()
}

static WORK_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="/work(\d+)""#).expect("valid pattern"));

pub fn parse_edition(raw: &Object) -> Edition {
    let creators = object(raw, "creators");
    let names = |key: &str| {
        let mut out = Vec::new();
        for person in creators
            .map(|c| array(c, key))
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_object)
        {
            append_unique(&mut out, string(person, "name").trim());
        }
        out
    };
    // `content` lists the edition's works as HTML snippets; the work id only appears in the link.
    let mut seen = HashSet::new();
    let work_ids = array(raw, "content")
        .iter()
        .filter_map(Value::as_str)
        .flat_map(|line| {
            WORK_LINK
                .captures_iter(line)
                .filter_map(|m| m[1].parse::<i64>().ok())
                .collect::<Vec<_>>()
        })
        .filter(|id| seen.insert(*id))
        .collect();
    Edition {
        id: flex_int(raw.get("edition_id")),
        year: flex_int(raw.get("year")),
        pages: flex_int(raw.get("pages")),
        name: string(raw, "edition_name"),
        lang_code: string(raw, "lang_code"),
        image: string(raw, "image"),
        description: string(raw, "description"),
        isbns: array(raw, "isbns")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        authors: names("authors"),
        publishers: names("publishers"),
        work_ids,
    }
}

/// Drops fantlab's bracketed placeholders such as "[оригинальное название не установлено]"
/// (original title not recorded), which would otherwise be offered as a title.
fn real_title(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('[') && s.ends_with(']') {
        String::new()
    } else {
        s.to_string()
    }
}

pub fn append_unique(list: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !list.iter().any(|existing| existing == value) {
        list.push(value.to_string());
    }
}

fn string(raw: &Object, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn object<'a>(raw: &'a Object, key: &str) -> Option<&'a Object> {
    raw.get(key).and_then(Value::as_object)
}

fn array<'a>(raw: &'a Object, key: &str) -> &'a [Value] {
    raw.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A number, or a number written as a string, as fantlab sends both.
fn flex_float(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn flex_int(value: Option<&Value>) -> i64 {
    flex_float(value) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chains_put_the_nearest_cycle_first_whichever_way_fantlab_lists_them() {
        let work = |chain: &str, root: i64| {
            let raw: Value = serde_json::from_str(&format!(
                r#"{{"work_id": 1, "work_name": "x", "parents": {{"cycles": [{chain}]}}, "work_root_saga": [{{"work_id": {root}}}]}}"#
            ))
            .unwrap();
            parse_work(raw.as_object().unwrap())
        };
        let root_first = work(
            r#"[{"work_id": 303358, "work_name": "Волкодав и его мир"}, {"work_id": 2483, "work_name": "Волкодав"}]"#,
            303358,
        );
        let root_last = work(
            r#"[{"work_id": 38222, "work_name": "История будущего"}, {"work_id": 17792, "work_name": "Мир Полудня"}]"#,
            17792,
        );
        assert_eq!(root_first.cycles[0].name, "Волкодав");
        assert_eq!(
            root_first
                .first_chain
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            [2483, 303358]
        );
        assert_eq!(root_last.cycles[0].name, "История будущего");
        assert_eq!(
            root_last
                .first_chain
                .iter()
                .map(|c| c.id)
                .collect::<Vec<_>>(),
            [38222, 17792]
        );
    }

    #[test]
    fn cycle_order_keeps_the_main_line_in_order() {
        let raw: Value = serde_json::from_str(include_str!("../testdata/cycle-1279.json")).unwrap();
        let order = cycle_order(raw.as_object().unwrap());
        assert_eq!(order, [1255, 1260, 1276, 12167, 239082, 513545, 1090326]);
    }
}
