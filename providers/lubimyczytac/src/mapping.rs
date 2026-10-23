use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::lubimyczytac::{extract_id, extract_path, resolve_url};

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pages: i64,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub series: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sequence: String,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// What one book page says, before it is shaped into an answer.
#[derive(Debug, Default)]
pub struct BookPage {
    pub provider_id: String,
    pub title: String,
    pub authors: Vec<String>,
    pub description: String,
    pub publisher: String,
    pub published_date: String,
    pub published_year: Option<i64>,
    pub language: String,
    pub page_count: Option<i64>,
    pub isbn10: String,
    pub isbn13: String,
    pub series_name: String,
    pub series_index: String,
    pub genres: Vec<String>,
    pub cover_url: String,
    /// On a 0-5 scale; the site rates out of 10.
    pub rating: Option<f64>,
    pub rating_count: Option<i64>,
}

#[derive(Debug, PartialEq)]
pub struct SearchLink {
    pub provider_id: String,
    pub url: String,
}

const MAX_GENRES: usize = 10;
const MIN_PUBLISHED_YEAR: i64 = 1000;
const MAX_PUBLISHED_YEAR: i64 = 2200;

const LANGUAGES: &[(&str, &str)] = &[
    ("polski", "pl"),
    ("angielski", "en"),
    ("niemiecki", "de"),
    ("francuski", "fr"),
    ("hiszpański", "es"),
    ("włoski", "it"),
];

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("valid selector")
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

static CARD_TITLE: LazyLock<Selector> = LazyLock::new(|| selector("a.book-card__title"));
static LEGACY_TITLE: LazyLock<Selector> =
    LazyLock::new(|| selector(".authorAllBooks__single .authorAllBooks__singleTextTitle"));
static TITLE: LazyLock<Selector> = LazyLock::new(|| selector("h1.book__title"));
static AUTHOR_LINK: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"a.link-name[href*="/autor/"], a[href*="/autor/"]"#));
static DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| selector(".book__description"));
static OG_DESCRIPTION: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"meta[property="og:description"]"#));
static PUBLISHER: LazyLock<Selector> = LazyLock::new(|| selector(r#"a[href*="/wydawnictwo/"]"#));
static DT: LazyLock<Selector> = LazyLock::new(|| selector("dt"));
static ISBN: LazyLock<Selector> = LazyLock::new(|| selector(r#"meta[property="books:isbn"]"#));
static SERIES_LINE: LazyLock<Selector> = LazyLock::new(|| selector("span.d-none.d-sm-block.mt-1"));
static CATEGORY: LazyLock<Selector> = LazyLock::new(|| selector("a.book__category"));
static TAG: LazyLock<Selector> = LazyLock::new(|| selector(r#"a[href*="/ksiazki/t/"]"#));
static COVER: LazyLock<Selector> = LazyLock::new(|| selector(".book-cover img"));
static OG_IMAGE: LazyLock<Selector> = LazyLock::new(|| selector(r#"meta[property="og:image"]"#));
static RATING_VALUE: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"meta[property="books:rating:value"]"#));
static RATING_SCALE: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"meta[property="books:rating:scale"]"#));
static JSON_LD: LazyLock<Selector> =
    LazyLock::new(|| selector(r#"script[type="application/ld+json"]"#));

static LANGUAGE_LABEL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)język"));
static SERIES_INDEX: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\(tom\s+([0-9]+(?:[.,][0-9]+)?)\)"));
static TRAILING_INDEX: LazyLock<Regex> = LazyLock::new(|| regex(r"\s([0-9]+(?:[.,][0-9]+)?)$"));
static SLUG_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| regex(r"[-_]+"));
static DATE_KEY: LazyLock<Regex> = LazyLock::new(|| regex(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$"));
static SEPARATED_DATE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^([0-9]{4})[-/.]([0-9]{1,2})[-/.]([0-9]{1,2})(?:[^0-9].*)?$"));
static YEAR: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|[^0-9])([0-9]{4})(?:[^0-9]|$)"));
static FIRST_NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"[0-9]+"));

/// Book links from a search page, best first, one per book.
pub fn search_results(html: &str, limit: usize) -> Vec<SearchLink> {
    let doc = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen = HashSet::new();
    // Today's result cards first, then the older author-list layout.
    for el in doc.select(&CARD_TITLE).chain(doc.select(&LEGACY_TITLE)) {
        if results.len() >= limit {
            break;
        }
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let (Some(id), Some(provider_id), Some(url)) =
            (extract_id(href), extract_path(href), resolve_url(href))
        else {
            continue;
        };
        if seen.insert(id) {
            results.push(SearchLink { provider_id, url });
        }
    }
    results
}

pub fn parse_book_page(html: &str, source_url: Option<&str>) -> BookPage {
    let doc = Html::parse_document(html);
    let ld = parse_json_ld(&doc);
    let (isbn10, isbn13) = extract_isbn(&doc);
    let (series_name, series_index) = extract_series(&doc);
    let (rating, rating_count) = extract_rating(&doc, &ld);

    BookPage {
        provider_id: ld
            .url
            .as_deref()
            .and_then(extract_path)
            .or_else(|| source_url.and_then(extract_path))
            .unwrap_or_default(),
        title: first_text(&doc, &TITLE),
        authors: if ld.authors.is_empty() {
            unique_texts(doc.select(&AUTHOR_LINK).map(element_text))
        } else {
            ld.authors.clone()
        },
        description: extract_description(&doc),
        publisher: first_text(&doc, &PUBLISHER),
        published_date: ld.published_date.clone().unwrap_or_default(),
        published_year: ld.published_year,
        language: extract_language(&doc)
            .or_else(|| ld.language.as_deref().map(map_language))
            .unwrap_or_default(),
        page_count: ld.page_count,
        isbn10,
        isbn13,
        series_name,
        series_index,
        genres: extract_genres(&doc, ld.genre.as_deref()),
        cover_url: extract_cover(&doc),
        rating,
        rating_count,
    }
}

/// The answer for a page, or nothing when it has no title or no id to file it under.
pub fn map_book(page: &BookPage, url: &str, fallback_id: &str) -> Option<BookMetadata> {
    let provider_id = if page.provider_id.is_empty() {
        extract_path(url).unwrap_or_else(|| fallback_id.to_string())
    } else {
        page.provider_id.clone()
    };
    if page.title.is_empty() || provider_id.is_empty() {
        return None;
    }
    let mut book = BookMetadata {
        media_type: "book".into(),
        provider_id,
        title: page.title.clone(),
        authors: page.authors.clone(),
        description: page.description.clone(),
        publisher: page.publisher.clone(),
        published_date: page.published_date.clone(),
        published_year: page
            .published_year
            .map(|year| year.to_string())
            .unwrap_or_default(),
        language_code: if page.language.is_empty() {
            "pl".into()
        } else {
            page.language.clone()
        },
        pages: page.page_count.filter(|pages| *pages > 0).unwrap_or(0),
        isbn13: page.isbn13.clone(),
        isbn10: page.isbn10.clone(),
        genres: page.genres.clone(),
        cover: page.cover_url.clone(),
        url: url.to_string(),
        ..BookMetadata::default()
    };
    if !page.series_name.is_empty() {
        book.series.push(Series {
            series: page.series_name.clone(),
            sequence: page.series_index.clone(),
        });
    }
    if page.rating.is_some() {
        book.rating = page.rating;
        book.rating_count = page.rating_count;
    }
    Some(book)
}

fn element_text(el: ElementRef) -> String {
    clean_text(&el.text().collect::<String>())
}

fn first_text(doc: &Html, selector: &Selector) -> String {
    doc.select(selector)
        .next()
        .map(element_text)
        .unwrap_or_default()
}

fn first_attr(doc: &Html, selector: &Selector, attr: &str) -> String {
    doc.select(selector)
        .next()
        .and_then(|el| el.value().attr(attr))
        .unwrap_or_default()
        .to_string()
}

fn unique_texts(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

/// The blurb. `.book__description` is the publisher's text; `.collapse-content`, which the page
/// also uses, holds reader reviews and the ratings histogram.
fn extract_description(doc: &Html) -> String {
    let blurb = doc
        .select(&DESCRIPTION)
        .next()
        .map(|el| {
            let mut raw = String::new();
            block_text(el, &mut raw);
            paragraphs(&raw)
        })
        .unwrap_or_default();
    if blurb.is_empty() {
        paragraphs(&first_attr(doc, &OG_DESCRIPTION, "content"))
    } else {
        blurb
    }
}

/// Text with line breaks where the markup breaks lines; source newlines are only spaces.
fn block_text(el: ElementRef, out: &mut String) {
    for child in el.children() {
        if let Some(text) = child.value().as_text() {
            out.push_str(&text.replace(['\n', '\r'], " "));
        } else if let Some(element) = ElementRef::wrap(child) {
            match element.value().name() {
                "br" => out.push('\n'),
                "script" | "style" => {}
                "p" | "div" | "li" | "ul" | "ol" | "blockquote" | "h1" | "h2" | "h3" | "h4"
                | "h5" | "h6" => {
                    out.push('\n');
                    block_text(element, out);
                    out.push('\n');
                }
                _ => block_text(element, out),
            }
        }
    }
}

/// Lines with their spacing collapsed; a run of empty lines becomes one paragraph break.
fn paragraphs(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in raw.split('\n') {
        let line = clean_text(line);
        if line.is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(&line);
        blank = false;
    }
    out
}

fn extract_language(doc: &Html) -> Option<String> {
    for dt in doc.select(&DT) {
        if !LANGUAGE_LABEL.is_match(&dt.text().collect::<String>()) {
            continue;
        }
        let raw = dt
            .next_siblings()
            .find_map(ElementRef::wrap)
            .filter(|next| next.value().name() == "dd")
            .map(element_text)
            .unwrap_or_default();
        if !raw.is_empty() {
            return Some(map_language(&raw));
        }
    }
    None
}

/// A language the site names in Polish, as a code; anything else passes through as written.
fn map_language(raw: &str) -> String {
    let normalized = raw.trim().to_lowercase();
    LANGUAGES
        .iter()
        .find(|(name, _)| *name == normalized)
        .map(|(_, code)| code.to_string())
        .unwrap_or(normalized)
}

fn extract_isbn(doc: &Html) -> (String, String) {
    let raw = clean_text(&first_attr(doc, &ISBN, "content"));
    let isbn: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .collect::<String>()
        .to_uppercase();
    match isbn.len() {
        13 => (String::new(), isbn),
        10 => (isbn, String::new()),
        _ => (String::new(), String::new()),
    }
}

/// "Cykl: Wiedźmin (tom 4)", or "Cykl: Komisarz Tymon Hanter 4" on some pages.
fn extract_series(doc: &Html) -> (String, String) {
    let Some(line) = doc
        .select(&SERIES_LINE)
        .map(element_text)
        .find(|text| text.starts_with("Cykl:"))
    else {
        return (String::new(), String::new());
    };
    let text = line["Cykl:".len()..].trim();
    if let Some(captures) = SERIES_INDEX.captures(text) {
        let name = SERIES_INDEX.replace(text, "").trim().to_string();
        return (name, captures[1].replacen(',', ".", 1));
    }
    if let Some(captures) = TRAILING_INDEX.captures(text) {
        let start = captures.get(0).expect("whole match").start();
        return (
            text[..start].trim().to_string(),
            captures[1].replacen(',', ".", 1),
        );
    }
    (text.to_string(), String::new())
}

fn extract_genres(doc: &Html, ld_genre: Option<&str>) -> Vec<String> {
    let mut genres: Vec<String> = Vec::new();
    // The category line, such as "fantasy, science fiction", names one genre per comma.
    for el in doc.select(&CATEGORY) {
        for part in el.text().collect::<String>().split(',') {
            add_genre(&mut genres, part);
        }
    }
    for el in doc.select(&TAG) {
        add_genre(&mut genres, &el.text().collect::<String>());
    }
    if genres.is_empty() {
        if let Some(slug) = ld_genre {
            add_genre(&mut genres, &SLUG_SEPARATORS.replace_all(slug, " "));
        }
    }
    genres
}

fn add_genre(genres: &mut Vec<String>, value: &str) {
    let genre = clean_text(value);
    if !genre.is_empty() && genres.len() < MAX_GENRES && !genres.contains(&genre) {
        genres.push(genre);
    }
}

fn extract_cover(doc: &Html) -> String {
    let src = doc
        .select(&COVER)
        .next()
        .and_then(|el| el.value().attr("src"))
        .filter(|src| !src.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| first_attr(doc, &OG_IMAGE, "content"));
    if src.is_empty() {
        return String::new();
    }
    resolve_url(&src).unwrap_or_default()
}

/// The page's own average, with the vote count from its structured data.
fn extract_rating(doc: &Html, ld: &JsonLd) -> (Option<f64>, Option<i64>) {
    let meta_value = parse_decimal(&first_attr(doc, &RATING_VALUE, "content"));
    let meta_scale = parse_decimal(&first_attr(doc, &RATING_SCALE, "content"));
    let (value, scale) = match meta_value {
        Some(value) => (value, meta_scale.unwrap_or(10.0)),
        None => match ld.rating_value {
            Some(value) => (value, ld.best_rating.unwrap_or(10.0)),
            None => return (None, None),
        },
    };
    if !(value > 0.0 && scale > 0.0 && value <= scale) {
        return (None, None);
    }
    // Out of 10 on the site; the field is out of 5 like every other community rating.
    (
        Some((value / scale * 500.0).round() / 100.0),
        ld.rating_count.filter(|count| *count > 0),
    )
}

fn parse_decimal(raw: &str) -> Option<f64> {
    raw.trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[derive(Debug, Default)]
struct JsonLd {
    authors: Vec<String>,
    page_count: Option<i64>,
    published_date: Option<String>,
    published_year: Option<i64>,
    genre: Option<String>,
    language: Option<String>,
    url: Option<String>,
    rating_value: Option<f64>,
    best_rating: Option<f64>,
    rating_count: Option<i64>,
}

/// The first value each field has across the page's JSON-LD blocks.
fn parse_json_ld(doc: &Html) -> JsonLd {
    let mut fields = JsonLd::default();
    for el in doc.select(&JSON_LD) {
        let Ok(value) = serde_json::from_str::<Value>(&el.text().collect::<String>()) else {
            continue;
        };
        let Some(node) = find_book_node(&value) else {
            continue;
        };

        if fields.authors.is_empty() {
            fields.authors = json_ld_authors(node.get("author"));
        }
        if fields.page_count.is_none() {
            fields.page_count = parse_integer(node.get("numberOfPages"));
        }
        if fields.published_year.is_none() {
            let published = node.get("datePublished").and_then(Value::as_str);
            if let Some(date) = published.and_then(parse_date_key) {
                fields.published_year = date[..4].parse().ok();
                fields.published_date = Some(date);
            } else {
                fields.published_year = published.and_then(parse_year);
            }
        }
        if fields.genre.is_none() {
            fields.genre = node
                .get("genre")
                .and_then(Value::as_str)
                .and_then(|genre| genre.split('/').rfind(|s| !s.is_empty()))
                .map(str::to_string);
        }
        if fields.language.as_deref().unwrap_or_default().is_empty() {
            if let Some(language) = node.get("inLanguage").and_then(Value::as_str) {
                fields.language = Some(language.to_string());
            }
        }
        if fields.url.as_deref().unwrap_or_default().is_empty() {
            if let Some(url) = node.get("url").and_then(Value::as_str) {
                fields.url = Some(url.to_string());
            }
        }
        if fields.rating_value.is_none() {
            if let Some(rating) = node.get("aggregateRating").and_then(Value::as_object) {
                fields.rating_value = json_decimal(rating.get("ratingValue"));
                fields.best_rating = json_decimal(rating.get("bestRating"));
                fields.rating_count = parse_integer(rating.get("ratingCount"));
            }
        }
    }
    fields
}

fn find_book_node(value: &Value) -> Option<&Map<String, Value>> {
    match value {
        Value::Array(items) => items.iter().find_map(find_book_node),
        Value::Object(node) => {
            if let Some(graph) = node.get("@graph") {
                return find_book_node(graph);
            }
            if matches!(
                node.get("@type").and_then(Value::as_str),
                Some("Book" | "Audiobook")
            ) {
                return Some(node);
            }
            ["author", "numberOfPages", "datePublished", "genre"]
                .iter()
                .any(|key| node.contains_key(*key))
                .then_some(node)
        }
        _ => None,
    }
}

fn json_ld_authors(author: Option<&Value>) -> Vec<String> {
    let name = |entry: &Value| match entry {
        Value::String(name) => clean_text(name),
        Value::Object(person) => person
            .get("name")
            .and_then(Value::as_str)
            .map(clean_text)
            .unwrap_or_default(),
        _ => String::new(),
    };
    let names: Vec<String> = match author {
        Some(Value::Array(entries)) => entries.iter().map(name).collect(),
        Some(entry) => vec![name(entry)],
        None => Vec::new(),
    };
    names.into_iter().filter(|name| !name.is_empty()).collect()
}

/// A whole number, or the first run of digits in a string ("1,024 pages" is 1024).
fn parse_integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| n.trunc() as i64),
        Value::String(text) => FIRST_NUMBER
            .find(&text.replace(',', ""))
            .and_then(|digits| digits.as_str().parse().ok()),
        _ => None,
    }
}

fn json_decimal(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite()),
        Value::String(text) => parse_decimal(text),
        _ => None,
    }
}

/// A `YYYY-MM-DD` date that exists, from an ISO date or one with other separators.
fn parse_date_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(c) = DATE_KEY.captures(trimmed) {
        if valid_date(&c[1], &c[2], &c[3]) {
            return Some(trimmed.to_string());
        }
    }
    let c = SEPARATED_DATE.captures(trimmed)?;
    let (year, month, day): (i64, i64, i64) =
        (c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?);
    let key = format!("{year:04}-{month:02}-{day:02}");
    valid_date(&key[..4], &key[5..7], &key[8..10]).then_some(key)
}

fn valid_date(year: &str, month: &str, day: &str) -> bool {
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<i64>(),
        month.parse::<i64>(),
        day.parse::<i64>(),
    ) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (MIN_PUBLISHED_YEAR..=MAX_PUBLISHED_YEAR).contains(&year) && (1..=days).contains(&day)
}

fn parse_year(raw: &str) -> Option<i64> {
    let year: i64 = YEAR.captures(raw.trim())?[1].parse().ok()?;
    (MIN_PUBLISHED_YEAR..=MAX_PUBLISHED_YEAR)
        .contains(&year)
        .then_some(year)
}

pub fn clean_text(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c == '\u{feff}')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie";

    const BOOK_HTML: &str = r#"
      <h1 class="book__title">Ostatnie życzenie</h1>
      <div class="book-cover"><img src="/upload/books/cover.jpg" /></div>
      <a href="/wydawnictwo/123/superNowa">superNOWA</a>
      <dl>
        <dt>Język:</dt><dd>polski</dd>
      </dl>
      <div class="book__description text-collapse">Opis książki o wiedźminie Geralcie.</div>
      <meta property="books:isbn" content="978-83-7578-063-5" />
      <a class="book__category d-sm-block d-none" href="/ksiazki/k/64/fantasy-fantastyka">Fantasy, fantastyka</a>
      <span class="d-none d-sm-block mt-1">Cykl: Wiedźmin (tom 1)</span>
      <script type="application/ld+json">
      {
        "@context": "https://schema.org",
        "@type": "Book",
        "name": "Ostatnie życzenie",
        "numberOfPages": 330,
        "datePublished": "2014-05-12",
        "author": [{ "@type": "Person", "name": "Andrzej Sapkowski" }],
        "genre": "https://lubimyczytac.pl/ksiazki/k/64/fantasy-fantastyka"
      }
      </script>
    "#;

    fn page(html: &str) -> BookPage {
        parse_book_page(html, None)
    }

    fn titled(rest: &str) -> BookPage {
        page(&format!(r#"<h1 class="book__title">Test</h1>{rest}"#))
    }

    #[test]
    fn search_results_are_unique_book_links() {
        let html = r#"
          <div class="book-card">
            <a class="book-card__cover-link" href="/ksiazka/123456/ostatnie-zyczenie"><img /></a>
            <a class="book-card__title" href="/ksiazka/123456/ostatnie-zyczenie">Ostatnie życzenie</a>
          </div>
          <div class="book-card">
            <a class="book-card__title" href="https://lubimyczytac.pl/ksiazka/789012/miecz-przeznaczenia">Miecz przeznaczenia</a>
          </div>
          <div class="book-card">
            <a class="book-card__title" href="/ksiazka/123456/inny-slug">Duplicate</a>
            <a class="book-card__title" href="/autor/1/sapkowski">Not a book</a>
          </div>
        "#;
        assert_eq!(
            search_results(html, 5),
            [
                SearchLink {
                    provider_id: "123456/ostatnie-zyczenie".into(),
                    url: "https://lubimyczytac.pl/ksiazka/123456/ostatnie-zyczenie".into()
                },
                SearchLink {
                    provider_id: "789012/miecz-przeznaczenia".into(),
                    url: "https://lubimyczytac.pl/ksiazka/789012/miecz-przeznaczenia".into()
                },
            ]
        );
    }

    #[test]
    fn search_results_fall_back_to_the_legacy_layout() {
        let html = r#"
          <div class="authorAllBooks__single">
            <a class="authorAllBooks__singleTextTitle" href="/ksiazka/555/legacy">Legacy</a>
          </div>
        "#;
        assert_eq!(
            search_results(html, 5),
            [SearchLink {
                provider_id: "555/legacy".into(),
                url: "https://lubimyczytac.pl/ksiazka/555/legacy".into()
            }]
        );
    }

    #[test]
    fn search_results_respect_the_limit() {
        let html = r#"
          <a class="book-card__title" href="/ksiazka/1/a">A</a>
          <a class="book-card__title" href="/ksiazka/2/b">B</a>
        "#;
        assert_eq!(search_results(html, 1).len(), 1);
    }

    #[test]
    fn real_search_page_keeps_the_site_order() {
        let links = search_results(include_str!("../testdata/search-wiedzmin.html"), 5);
        let ids: Vec<&str> = links.iter().map(|l| l.provider_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "138913/wiedzmin",
                "4811730/wiedzmin-szpony-i-kly",
                "5244386/wiedzmin-ostatnie-zyczenie",
                "303348/wiedzmin-ostatnie-zyczenie",
                "98232/wiedzmin-zbior-opowiadan",
            ]
        );
        assert_eq!(
            links[0].url,
            "https://lubimyczytac.pl/ksiazka/138913/wiedzmin"
        );
    }

    #[test]
    fn parses_a_complete_book_page() {
        let data = parse_book_page(BOOK_HTML, Some(SOURCE));
        assert_eq!(data.provider_id, "123456/ostatnie-zyczenie");
        assert_eq!(data.title, "Ostatnie życzenie");
        assert_eq!(data.authors, ["Andrzej Sapkowski"]);
        assert_eq!(data.description, "Opis książki o wiedźminie Geralcie.");
        assert_eq!(data.publisher, "superNOWA");
        assert_eq!(data.language, "pl");
        assert_eq!(data.page_count, Some(330));
        assert_eq!(data.published_year, Some(2014));
        assert_eq!(data.published_date, "2014-05-12");
        assert_eq!(
            (data.isbn13.as_str(), data.isbn10.as_str()),
            ("9788375780635", "")
        );
        assert_eq!(
            (data.series_name.as_str(), data.series_index.as_str()),
            ("Wiedźmin", "1")
        );
        assert_eq!(data.genres, ["Fantasy", "fantastyka"]);
        assert_eq!(
            data.cover_url,
            "https://lubimyczytac.pl/upload/books/cover.jpg"
        );
    }

    #[test]
    fn real_page_maps_every_field() {
        let url = "https://lubimyczytac.pl/ksiazka/240310/ostatnie-zyczenie";
        let page = parse_book_page(include_str!("../testdata/book-240310.html"), Some(url));
        let b = map_book(&page, url, "240310/ostatnie-zyczenie").expect("a book");
        assert_eq!(
            (b.provider_id.as_str(), b.url.as_str()),
            ("240310/ostatnie-zyczenie", url)
        );
        assert_eq!(b.title, "Ostatnie życzenie");
        assert_eq!(b.authors, ["Andrzej Sapkowski"]);
        assert_eq!(b.publisher, "superNOWA");
        assert_eq!(
            (b.published_year.as_str(), b.published_date.as_str()),
            ("2014", "2014-09-25")
        );
        assert_eq!(
            (b.isbn13.as_str(), b.isbn10.as_str()),
            ("9788375780635", "")
        );
        assert_eq!((b.pages, b.language_code.as_str()), (332, "pl"));
        assert_eq!(
            b.series,
            [Series {
                series: "Wiedźmin Geralt z Rivii".into(),
                sequence: "1".into()
            }]
        );
        assert_eq!(b.genres, ["fantasy", "science fiction"]);
        assert_eq!(
            b.cover,
            "https://s.lubimyczytac.pl/upload/books/240000/240310/1313778-352x500.jpg"
        );
        assert_eq!((b.rating, b.rating_count), (Some(4.18), Some(61225)));
        assert!(
            b.description
                .starts_with("„Ostatnie życzenie” to jeden ze zbiorów opowiadań")
                && b.description
                    .contains("gry komputerowej.\n\nGłówny bohater, Geralt z Rivii")
                && b.description
                    .contains("Spis treści:\n\n„Głos rozsądku 1-7”\n„Wiedźmin”\n"),
            "{}",
            b.description
        );
    }

    #[test]
    fn real_page_without_isbn_or_pages() {
        let url = "https://lubimyczytac.pl/ksiazka/138913/wiedzmin";
        let page = parse_book_page(include_str!("../testdata/book-138913.html"), Some(url));
        let b = map_book(&page, url, "138913/wiedzmin").expect("a book");
        assert_eq!(
            (b.title.as_str(), b.publisher.as_str()),
            ("Wiedźmin", "Nowa Fantastyka")
        );
        // The page lists "02091631", which is neither ISBN length.
        assert!(b.isbn13.is_empty() && b.isbn10.is_empty());
        assert_eq!(b.pages, 0);
        assert_eq!(
            b.series,
            [Series {
                series: "Wiedźmin. Opowiadania".into(),
                sequence: "1".into()
            }]
        );
        assert_eq!(
            b.genres,
            [
                "fantasy",
                "science fiction",
                "wiedźmin",
                "sapkowski",
                "fantastyka",
                "geralt"
            ]
        );
        assert_eq!((b.rating, b.rating_count), (Some(4.22), Some(2226)));
        assert_eq!(b.published_year, "1986");
    }

    #[test]
    fn genres_fall_back_to_the_json_ld_slug() {
        let data = page(
            r#"<h1 class="book__title">Bez tagów</h1>
            <script type="application/ld+json">
            { "@type": "Book", "genre": "https://lubimyczytac.pl/ksiazki/k/69/poradniki" }
            </script>"#,
        );
        assert_eq!(data.genres, ["poradniki"]);
        let data = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "genre": "https://lubimyczytac.pl/ksiazki/k/41/fantasy-science-fiction" }</script>"#,
        );
        assert_eq!(data.genres, ["fantasy science fiction"]);
    }

    #[test]
    fn genres_come_from_user_tags() {
        let data = titled(
            r#"<a href="/ksiazki/t/1/fantasy">Fantasy</a><a href="/ksiazki/t/2/kryminał">Kryminał</a>"#,
        );
        assert_eq!(data.genres, ["Fantasy", "Kryminał"]);
        assert!(titled("").genres.is_empty());
    }

    #[test]
    fn genres_stop_at_ten() {
        let tags: String = (0..15)
            .map(|i| format!(r#"<a href="/ksiazki/t/{i}">tag {i}</a>"#))
            .collect();
        assert_eq!(titled(&tags).genres.len(), 10);
    }

    #[test]
    fn series_index_forms() {
        for (line, name, index) in [
            (
                r#"Cykl: <a href="/cykl/1/x">Komisarz Tymon Hanter 4</a>"#,
                "Komisarz Tymon Hanter",
                "4",
            ),
            ("Cykl: Wiedźmin (tom 2,5)", "Wiedźmin", "2.5"),
            (
                r#"Cykl: <a href="/cykl/1/x">Wiedźmin (tom 5,10)</a>"#,
                "Wiedźmin",
                "5.10",
            ),
            ("Cykl: Wiedźmin", "Wiedźmin", ""),
            ("Cykl: Wiedźmin (TOM 3)", "Wiedźmin", "3"),
        ] {
            let data = titled(&format!(
                r#"<span class="d-none d-sm-block mt-1">{line}</span>"#
            ));
            assert_eq!(
                (data.series_name.as_str(), data.series_index.as_str()),
                (name, index),
                "{line}"
            );
        }
        let data = titled("");
        assert!(data.series_name.is_empty() && data.series_index.is_empty());
    }

    #[test]
    fn series_skips_other_lines_with_the_same_classes() {
        let data = titled(
            r#"<span class="d-none d-sm-block mt-1">Ekranizacje: Wiedźmin (2019)</span>
               <span class="mt-1 d-sm-block d-none">Cykl: Saga (tom 7)</span>"#,
        );
        assert_eq!(
            (data.series_name.as_str(), data.series_index.as_str()),
            ("Saga", "7")
        );
    }

    #[test]
    fn empty_page_has_no_title() {
        assert!(page("<html><body></body></html>").title.is_empty());
        assert!(map_book(&page("<html><body></body></html>"), SOURCE, "1").is_none());
    }

    #[test]
    fn provider_id_sources() {
        let canonical = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "url": "https://lubimyczytac.pl/ksiazka/999/canonical-slug" }</script>"#,
        );
        assert_eq!(canonical.provider_id, "999/canonical-slug");
        let from_source = parse_book_page(
            r#"<h1 class="book__title">Test</h1>"#,
            Some("https://lubimyczytac.pl/ksiazka/777/from-source"),
        );
        assert_eq!(from_source.provider_id, "777/from-source");
        assert!(titled("").provider_id.is_empty());
    }

    #[test]
    fn broken_json_ld_is_skipped() {
        let data = titled(r#"<script type="application/ld+json">{ broken json }</script>"#);
        assert_eq!((data.title.as_str(), data.page_count), ("Test", None));
    }

    #[test]
    fn page_counts() {
        for (pages, want) in [
            (r#""330""#, Some(330)),
            (r#""brak danych""#, None),
            (r#"{"value": 300}"#, None),
            ("1e309", None),
            (r#""1,024 stron""#, Some(1024)),
        ] {
            let data = titled(&format!(
                r#"<script type="application/ld+json">{{ "@type": "Book", "numberOfPages": {pages} }}</script>"#
            ));
            assert_eq!(data.page_count, want, "{pages}");
        }
    }

    #[test]
    fn published_dates() {
        for (date, year, key) in [
            ("2020", None, ""),
            (r#""unknown""#, None, ""),
            (r#""2015""#, Some(2015), ""),
            (r#""2021/3/5""#, Some(2021), "2021-03-05"),
            (r#""2021-02-30""#, Some(2021), ""),
            (r#""2014-05-12T10:00:00""#, Some(2014), "2014-05-12"),
        ] {
            let data = titled(&format!(
                r#"<script type="application/ld+json">{{ "@type": "Book", "datePublished": {date} }}</script>"#
            ));
            assert_eq!(
                (data.published_year, data.published_date.as_str()),
                (year, key),
                "{date}"
            );
        }
    }

    #[test]
    fn isbn10_is_kept_apart() {
        let data = titled(r#"<meta property="books:isbn" content="0-7432-7356-7" />"#);
        assert_eq!(
            (data.isbn10.as_str(), data.isbn13.as_str()),
            ("0743273567", "")
        );
    }

    #[test]
    fn cover_fallbacks() {
        let og = titled(r#"<meta property="og:image" content="https://example.com/cover.jpg" />"#);
        assert_eq!(og.cover_url, "https://example.com/cover.jpg");
        let relative = titled(
            r#"<div class="book-cover"><img src="//static.lubimyczytac.pl/cover.jpg" /></div>"#,
        );
        assert_eq!(
            relative.cover_url,
            "https://static.lubimyczytac.pl/cover.jpg"
        );
    }

    #[test]
    fn description_fallback_and_paragraphs() {
        let og = titled(r#"<meta property="og:description" content="Opis z og." />"#);
        assert_eq!(og.description, "Opis z og.");
        let blurb = titled(
            "<div class=\"book__description\"> Pierwszy\n akapit.<br /><br />Drugi<br/>wiersz &amp; <b>więcej</b>.  </div>",
        );
        assert_eq!(
            blurb.description,
            "Pierwszy akapit.\n\nDrugi\nwiersz & więcej."
        );
    }

    #[test]
    fn languages() {
        let unknown = titled("<dl><dt>Język:</dt><dd>klingon</dd></dl>");
        assert_eq!(unknown.language, "klingon");
        let ld = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "inLanguage": "pl" }</script>"#,
        );
        assert_eq!(ld.language, "pl");
        let ld_name = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "inLanguage": "Angielski" }</script>"#,
        );
        assert_eq!(ld_name.language, "en");
        let none = titled("");
        assert!(none.language.is_empty());
        assert_eq!(
            map_book(&none, SOURCE, "1").map(|b| b.language_code),
            Some("pl".to_string())
        );
    }

    #[test]
    fn json_ld_authors() {
        for (author, want) in [
            (r#""Andrzej Sapkowski""#, vec!["Andrzej Sapkowski"]),
            (r#"{ "name": 42 }"#, vec![]),
            (r#"{ "name": "   " }"#, vec![]),
            (r#""   ""#, vec![]),
        ] {
            let data = titled(&format!(
                r#"<script type="application/ld+json">{{ "@type": "Book", "author": {author} }}</script>"#
            ));
            assert_eq!(data.authors, want, "{author}");
        }
    }

    #[test]
    fn html_authors_are_deduplicated() {
        let data = titled(
            r#"<a class="link-name" href="/autor/1/sapkowski">Andrzej Sapkowski</a>
               <a href="/autor/1/sapkowski">Andrzej Sapkowski</a>
               <a href="/autor/2/x">Inny Autor</a>"#,
        );
        assert_eq!(data.authors, ["Andrzej Sapkowski", "Inny Autor"]);
    }

    #[test]
    fn json_ld_node_shapes() {
        let graph = titled(
            r#"<script type="application/ld+json">{ "@graph": [{ "@type": "Book", "numberOfPages": 200, "datePublished": "2020-01-01" }] }</script>"#,
        );
        assert_eq!(
            (graph.page_count, graph.published_year),
            (Some(200), Some(2020))
        );
        let array = titled(
            r#"<script type="application/ld+json">[{ "@type": "WebPage" }, { "@type": "Book", "numberOfPages": 100 }]</script>"#,
        );
        assert_eq!(array.page_count, Some(100));
        let second = titled(
            r#"<script type="application/ld+json">{ "@type": "WebSite" }</script>
               <script type="application/ld+json">{ "@type": "Book", "numberOfPages": 150 }</script>"#,
        );
        assert_eq!(second.page_count, Some(150));
        let none = titled(
            r#"<script type="application/ld+json">[{ "@type": "WebPage" }, { "@type": "WebSite" }]</script>"#,
        );
        assert_eq!((none.page_count, none.published_year), (None, None));
        let audio = titled(
            r#"<script type="application/ld+json">{ "@type": "Audiobook", "numberOfPages": 400, "datePublished": "2021-03-15" }</script>"#,
        );
        assert_eq!(
            (audio.page_count, audio.published_year),
            (Some(400), Some(2021))
        );
        let first_wins = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "numberOfPages": 100, "datePublished": "2019" }</script>
               <script type="application/ld+json">{ "@type": "Book", "numberOfPages": 999, "datePublished": "2099" }</script>"#,
        );
        assert_eq!(
            (first_wins.page_count, first_wins.published_year),
            (Some(100), Some(2019))
        );
    }

    #[test]
    fn ratings() {
        let meta = titled(
            r#"<meta property="books:rating:value" content="7.5" /><meta property="books:rating:scale" content="10" />"#,
        );
        assert_eq!(meta.rating, Some(3.75));
        let ld = titled(
            r#"<script type="application/ld+json">{ "@type": "Book", "aggregateRating": { "bestRating": 10, "ratingValue": "8,4", "ratingCount": 12 } }</script>"#,
        );
        assert_eq!((ld.rating, ld.rating_count), (Some(4.2), Some(12)));
        let unrated = titled(r#"<meta property="books:rating:value" content="0" />"#);
        assert_eq!(unrated.rating, None);
    }

    #[test]
    fn empty_fields_are_left_out_of_the_answer() {
        let data = parse_book_page(
            r#"<h1 class="book__title">Test Book</h1>
            <script type="application/ld+json">{ "@type": "Book", "url": "https://lubimyczytac.pl/ksiazka/1/test" }</script>"#,
            None,
        );
        let book = map_book(&data, SOURCE, "1").expect("a book");
        let json = serde_json::to_value(&book).unwrap();
        for absent in ["authors", "genres", "series", "rating", "pages", "isbn13"] {
            assert!(json.get(absent).is_none(), "{absent} in {json}");
        }
        assert_eq!(json["providerId"], "1/test");
    }
}
