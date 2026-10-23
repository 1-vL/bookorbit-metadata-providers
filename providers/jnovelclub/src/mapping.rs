use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::jnovelclub::{Cover, Series, Volume};

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: &'static str,
    pub provider_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    pub publisher: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesEntry>,
    pub language_code: &'static str,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_date: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub cover_width: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub cover_height: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct SeriesEntry {
    pub series: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sequence: String,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

// "Berserk Vol. 3", "Berserk v03", "Berserk volume 3", "One Piece #3". A bare trailing number is
// left alone, since it is as often part of a title ("Mob Psycho 100").
static VOLUME: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)(?:(?-u:\b)(?:vol(?:ume)?|v)\.?\s*|#\s*)0*([0-9]{1,4}(?:\.[0-9]+)?)(?-u:\b)")
});
static EMPTY_BRACKETS: LazyLock<Regex> = LazyLock::new(|| regex(r"\(\s*\)|\[\s*\]"));
static TRAILING_JUNK: LazyLock<Regex> =
    LazyLock::new(|| regex(r"[\s,.:;_\-\x{2013}\x{2014}(\[]+$"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| regex(r"\n{3,}"));
static CDN_COVER: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(https://cdn\.j-novel\.club/pub/img/)[0-9]+/(?:webp|jpg)/([A-Za-z0-9/._-]+)$")
});

/// The cover height asked of J-Novel Club's image CDN: the largest its own site uses.
const COVER_HEIGHT: u32 = 1200;

/// J-Novel Club tags mix genres with imprints, tropes and shop labels ("available in print"); only
/// these are genres.
const GENRES: &[(&str, &str)] = &[
    ("action", "Action"),
    ("adventure", "Adventure"),
    ("boys' love", "Boys' Love"),
    ("bl", "Boys' Love"),
    ("comedy", "Comedy"),
    ("cyberpunk", "Cyberpunk"),
    ("dark fantasy", "Dark Fantasy"),
    ("drama", "Drama"),
    ("dystopia", "Dystopia"),
    ("ecchi", "Ecchi"),
    ("fantasy", "Fantasy"),
    ("harem", "Harem"),
    ("high fantasy", "High Fantasy"),
    ("historical", "Historical"),
    ("historical fantasy", "Historical Fantasy"),
    ("horror", "Horror"),
    ("isekai", "Isekai"),
    ("josei", "Josei"),
    ("martial arts", "Martial Arts"),
    ("mecha", "Mecha"),
    ("military", "Military"),
    ("mystery", "Mystery"),
    ("post-apocalyptic", "Post-Apocalyptic"),
    ("psychological", "Psychological"),
    ("reverse harem", "Reverse Harem"),
    ("romance", "Romance"),
    ("romantic comedy", "Romantic Comedy"),
    ("school life", "School Life"),
    ("sci-fi", "Science Fiction"),
    ("science fiction", "Science Fiction"),
    ("seinen", "Seinen"),
    ("shojo", "Shojo"),
    ("shoujo", "Shojo"),
    ("shonen", "Shonen"),
    ("shounen", "Shonen"),
    ("slice of life", "Slice of Life"),
    ("space opera", "Space Opera"),
    ("sports", "Sports"),
    ("steampunk", "Steampunk"),
    ("supernatural", "Supernatural"),
    ("suspense", "Suspense"),
    ("thriller", "Thriller"),
    ("urban fantasy", "Urban Fantasy"),
    ("yuri", "Yuri"),
];

/// Words in a file's title that say what kind of file it is, not which book.
const NOISE_WORDS: &[&str] = &[
    "light", "novel", "ln", "epub", "ebook", "digital", "edition", "premium", "jnc", "english",
];

/// Takes a volume number off a search query: the title is what J-Novel Club knows, the number is
/// which book of it the file is.
pub fn split_volume(query: &str) -> (String, String) {
    let Some(captures) = VOLUME.captures(query) else {
        return (query.to_string(), String::new());
    };
    let whole = captures.get(0).expect("whole match");
    let rest = format!("{} {}", &query[..whole.start()], &query[whole.end()..]);
    let rest = EMPTY_BRACKETS.replace_all(&rest, " ");
    let rest = SPACES.replace_all(&rest, " ");
    let rest = TRAILING_JUNK.replace_all(rest.trim(), "").into_owned();
    if rest.is_empty() {
        return (query.to_string(), String::new());
    }
    (rest, captures[1].to_string())
}

/// A title as comparable words: lowercase, no punctuation, "Vol." and "v03" read as "volume 3",
/// "World’s" as "worlds", numbers without leading zeros.
pub fn words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let decimal_point = c == '.'
            && !current.is_empty()
            && current.chars().all(|d| d.is_ascii_digit())
            && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if c.is_alphanumeric() || decimal_point {
            current.push(c);
        } else if c == '\'' || c == '\u{2019}' {
            continue;
        } else {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            if c == '&' {
                tokens.push("and".to_string());
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens.into_iter().flat_map(normalize_word).collect()
}

fn normalize_word(word: String) -> Vec<String> {
    if word == "vol" {
        return vec!["volume".into()];
    }
    if let Some(number) = word.strip_prefix('v').filter(|rest| is_number(rest)) {
        return vec!["volume".into(), trim_zeros(number)];
    }
    if is_number(&word) {
        return vec![trim_zeros(&word)];
    }
    vec![word]
}

fn is_number(word: &str) -> bool {
    !word.is_empty() && word.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn trim_zeros(number: &str) -> String {
    let trimmed = number.trim_start_matches('0');
    if trimmed.is_empty() || trimmed.starts_with('.') {
        format!("0{trimmed}")
    } else {
        trimmed.to_string()
    }
}

pub struct SeriesMatch<'a> {
    pub series: &'a Series,
    pub score: u32,
}

/// Series whose title matches, best first. `title` is the query with its volume taken off.
pub fn rank_series<'a>(catalogue: &'a [Series], title: &str, query: &str) -> Vec<SeriesMatch<'a>> {
    let wanted = words(title);
    let full = words(query);
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<SeriesMatch> = catalogue
        .iter()
        .filter(|series| !series.hidden)
        .filter_map(|series| {
            let score = series_score(series, &wanted, &full);
            (score > 0).then_some(SeriesMatch { series, score })
        })
        .collect();
    found.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.series.title.len().cmp(&b.series.title.len()))
    });
    found
}

fn series_score(series: &Series, wanted: &[String], full: &[String]) -> u32 {
    let score = title_score(series, wanted, full);
    // "Arifureta 3": a bare number after a shortened title is the volume, not part of the title.
    match wanted.split_last() {
        Some((last, rest)) if is_number(last) && !rest.is_empty() => {
            score.max(title_score(series, rest, full).saturating_sub(1))
        }
        _ => score,
    }
}

fn title_score(series: &Series, wanted: &[String], full: &[String]) -> u32 {
    let title = words(&series.title);
    if title.is_empty() {
        return 0;
    }
    if title == wanted
        || title == full
        || (!series.original_title.is_empty() && words(&series.original_title) == wanted)
    {
        return 100;
    }
    // A manga adaptation is titled "<novel> (Manga)"; a file of it is often named like the novel.
    let core = manga_core(&title);
    if core == Some(wanted) {
        return 95;
    }
    // The query carries more than the series title: a part, a volume subtitle.
    if wanted.starts_with(&title) {
        return 70 + title.len().min(20) as u32;
    }
    if let Some(core) = core.filter(|core| wanted.starts_with(core)) {
        return 69 + core.len().min(20) as u32;
    }
    // The query is the series title cut short, or its words in another order.
    let share = (20 * wanted.len() / title.len()) as u32;
    if title.starts_with(wanted) {
        return 50 + share;
    }
    if wanted.iter().all(|word| title.contains(word)) {
        return 30 + share;
    }
    0
}

fn manga_core(title: &[String]) -> Option<&[String]> {
    match title.split_last() {
        Some((last, rest)) if last == "manga" && !rest.is_empty() => Some(rest),
        _ => None,
    }
}

/// The query's words beyond the series title: "part 3" of "Ascendance of a Bookworm Part 3".
fn leftover(wanted: &[String], series: &Series) -> Vec<String> {
    let title = words(&series.title);
    let prefix = if wanted.starts_with(&title) {
        title.len()
    } else {
        manga_core(&title)
            .filter(|core| wanted.starts_with(core))
            .map_or(0, <[String]>::len)
    };
    wanted[prefix..]
        .iter()
        .filter(|word| prefix > 0 || !title.contains(word))
        .cloned()
        .collect()
}

pub struct VolumeMatch {
    pub index: usize,
    pub score: u32,
    pub exact: bool,
}

/// The volume of a series a query names: by its full title, by the number taken off the query,
/// or by the words left after the series title. Without either, the series' first volume.
pub fn pick_volume(
    series: &Series,
    volumes: &[Volume],
    title: &str,
    query: &str,
    volume: &str,
) -> Option<VolumeMatch> {
    let full = words(query);
    let mut extra = leftover(&words(title), series);
    extra.retain(|word| !NOISE_WORDS.contains(&word.as_str()));
    if let Ok(number) = volume.parse::<f64>() {
        return best_volume(series, volumes, &full, Some(number), &extra);
    }
    // A bare number after the series title is most likely the volume: "The Apothecary Diaries 3".
    if let Some((number, rest)) = extra
        .split_last()
        .and_then(|(last, rest)| Some((last.parse::<f64>().ok()?, rest)))
    {
        if let Some(found) = best_volume(series, volumes, &full, Some(number), rest) {
            return Some(found);
        }
    }
    if let Some(found) = best_volume(series, volumes, &full, None, &extra) {
        return Some(found);
    }
    if !extra.is_empty() {
        return None;
    }
    volumes
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.hidden)
        .min_by(|(_, a), (_, b)| a.position.total_cmp(&b.position))
        .map(|(index, _)| VolumeMatch {
            index,
            score: 1,
            exact: false,
        })
}

fn best_volume(
    series: &Series,
    volumes: &[Volume],
    full: &[String],
    wanted: Option<f64>,
    extra: &[String],
) -> Option<VolumeMatch> {
    volumes
        .iter()
        .enumerate()
        .filter(|(_, v)| !v.hidden)
        .filter_map(|(index, v)| {
            let (score, exact) = volume_score(series, v, full, wanted, extra);
            (score > 0).then_some((index, score, exact, v.position))
        })
        .max_by(|a, b| a.1.cmp(&b.1).then(b.3.total_cmp(&a.3)))
        .map(|(index, score, exact, _)| VolumeMatch {
            index,
            score,
            exact,
        })
}

fn volume_score(
    series: &Series,
    v: &Volume,
    full: &[String],
    wanted: Option<f64>,
    extra: &[String],
) -> (u32, bool) {
    let title = words(&v.title);
    if title == full || words(&format!("{} {}", series.title, v.short_title)) == full {
        return (100, true);
    }
    // Words the query has beyond the series title that this volume lacks: likely another book.
    // A number among them ("part 5") must be there; of the rest, at least half.
    let (numbers, others): (Vec<&String>, Vec<&String>) =
        extra.iter().partition(|word| is_number(word));
    if numbers.iter().any(|number| !title.contains(number)) {
        return (0, false);
    }
    let overlap = if others.is_empty() {
        1.0
    } else {
        others.iter().filter(|word| title.contains(word)).count() as f64 / others.len() as f64
    };
    if overlap < 0.5 {
        return (0, false);
    }
    match wanted {
        Some(number) => {
            let by_number = v.number == number;
            let by_title = title_number(&title) == Some(number);
            let by_position = v.position == number;
            if !(by_number || by_title || by_position) {
                return (0, false);
            }
            let score = 60
                + (30.0 * overlap) as u32
                + 3 * u32::from(by_number)
                + 2 * u32::from(by_title)
                + u32::from(by_position);
            (score, false)
        }
        None if !extra.is_empty() => (40 + (40.0 * overlap) as u32, false),
        None => (0, false),
    }
}

/// The number a volume's title gives it: after "volume" or "level" when it says so, else its last.
fn title_number(title: &[String]) -> Option<f64> {
    let after_label = title
        .windows(2)
        .rev()
        .find(|pair| matches!(pair[0].as_str(), "volume" | "level"))
        .and_then(|pair| pair[1].parse().ok());
    after_label.or_else(|| title.iter().rev().find_map(|word| word.parse().ok()))
}

pub fn map_volume(v: &Volume, series: Option<&Series>) -> BookMetadata {
    let (cover, cover_width, cover_height) = v.cover.as_ref().map(best_cover).unwrap_or_default();
    // J-Novel Club sells ebooks first; a volume it never released as one went straight to print.
    let date = if v.no_ebook {
        v.physical_publishing.as_ref().or(v.publishing.as_ref())
    } else {
        v.publishing.as_ref().or(v.physical_publishing.as_ref())
    }
    .and_then(|date| date.get(..10))
    .filter(|date| is_iso_date(date))
    .unwrap_or_default()
    .to_string();
    let title = match (v.title.trim(), series) {
        ("", Some(series)) => format!("{} {}", series.title.trim(), v.short_title.trim()),
        (title, _) => title.to_string(),
    };
    let description = if v.description.trim().is_empty() {
        series.map(|s| s.description.as_str()).unwrap_or_default()
    } else {
        v.description.as_str()
    };
    let position = (v.position > 0.0).then(|| format_number(v.position));
    BookMetadata {
        // A manga volume is a comic, so a client can tell it from the novel of the same name.
        media_type: if series.is_some_and(|s| s.kind == "MANGA") {
            "comic"
        } else {
            "book"
        },
        provider_id: v.id.clone(),
        title: title.trim().to_string(),
        authors: authors(v),
        publisher: "J-Novel Club",
        published_year: date.get(..4).unwrap_or_default().to_string(),
        description: plain_text(description),
        cover,
        genres: series.map(|s| genres(&s.tags)).unwrap_or_default(),
        series: series
            .filter(|s| !s.title.trim().is_empty())
            .map(|s| SeriesEntry {
                series: s.title.trim().to_string(),
                sequence: position.clone().unwrap_or_default(),
            })
            .into_iter()
            .collect(),
        language_code: "en",
        published_date: date,
        cover_width,
        cover_height,
        // The site has no page per volume; its series page has an anchor for each, by position.
        url: match (series, &position) {
            (Some(s), Some(position)) if is_slug(&s.slug) => {
                format!("https://j-novel.club/series/{}#volume-{position}", s.slug)
            }
            (Some(s), None) if is_slug(&s.slug) => {
                format!("https://j-novel.club/series/{}", s.slug)
            }
            _ => String::new(),
        },
    }
}

/// The API links a 480-pixel WebP. The same CDN serves the 1200-pixel size J-Novel Club's own
/// pages use, here as JPEG for readers that cannot show WebP, scaled to that height and never
/// enlarged.
fn best_cover(c: &Cover) -> (String, u32, u32) {
    if c.original_url.starts_with("https://") {
        return (c.original_url.clone(), c.width, c.height);
    }
    let Some(captures) = CDN_COVER.captures(&c.cover_url) else {
        let url = Some(c.cover_url.clone()).filter(|url| url.starts_with("https://"));
        return (url.unwrap_or_default(), 0, 0);
    };
    let url = format!("{}{COVER_HEIGHT}/jpg/{}", &captures[1], &captures[2]);
    let (width, height) = if c.height > COVER_HEIGHT {
        let width = f64::from(c.width) * f64::from(COVER_HEIGHT) / f64::from(c.height);
        (width.round() as u32, COVER_HEIGHT)
    } else {
        (c.width, c.height)
    };
    (url, width, height)
}

/// Writers only: illustrators, translators, editors and letterers are not authors. A manga with
/// no writer credited is its artist's.
fn authors(v: &Volume) -> Vec<String> {
    let named = |role: &str| -> Vec<String> {
        let names: Vec<String> = v
            .creators
            .iter()
            .filter(|c| c.role == role)
            .map(|c| c.name.clone())
            .collect();
        unique(&names)
    };
    let writers = named("AUTHOR");
    if writers.is_empty() {
        named("ARTIST")
    } else {
        writers
    }
}

pub fn genres(tags: &[String]) -> Vec<String> {
    let labels: Vec<String> = tags
        .iter()
        .filter_map(|tag| {
            let tag = tag.trim().to_lowercase().replace('\u{2019}', "'");
            GENRES
                .iter()
                .find(|(name, _)| *name == tag)
                .map(|(_, label)| label.to_string())
        })
        .collect();
    unique(&labels)
}

/// Descriptions are already plain text; this only evens out line breaks.
pub fn plain_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    BLANK_LINES.replace_all(&text, "\n\n").trim().to_string()
}

fn format_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{}", number as i64)
    } else {
        number.to_string()
    }
}

fn is_iso_date(date: &str) -> bool {
    date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

fn is_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn unique(list: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    list.iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
        .map(str::to_string)
        .collect()
}

/// True when every word of the asked-for author is in one of the volume's writer or artist
/// names, in any order: "Kazuki Miya" finds "Miya Kazuki". A name that differs only in its
/// separators matches too.
pub fn author_matches(v: &Volume, author: &str) -> bool {
    let wanted = name_words(author);
    if wanted.is_empty() {
        return true;
    }
    let names = v
        .creators
        .iter()
        .filter(|c| c.role == "AUTHOR" || c.role == "ARTIST");
    names.into_iter().any(|creator| {
        let words = name_words(&creator.name);
        if words.concat() == wanted.concat() {
            return true;
        }
        let have: HashSet<String> = words.into_iter().collect();
        wanted.iter().all(|word| have.contains(word))
    })
}

fn name_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphabetic() && !c.is_ascii_digit())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct SeriesPage {
        series: Vec<Series>,
    }

    #[derive(serde::Deserialize)]
    struct VolumesBody {
        volumes: Vec<Volume>,
    }

    fn catalogue() -> Vec<Series> {
        [
            include_str!("../testdata/series-page-1.json"),
            include_str!("../testdata/series-page-2.json"),
        ]
        .iter()
        .flat_map(|page| {
            serde_json::from_str::<SeriesPage>(page)
                .expect("fixture parses")
                .series
        })
        .collect()
    }

    fn volumes(json: &str) -> Vec<Volume> {
        serde_json::from_str::<VolumesBody>(json)
            .expect("fixture parses")
            .volumes
    }

    fn series(id: &str) -> Series {
        catalogue()
            .into_iter()
            .find(|s| s.id == id)
            .expect("series in fixture")
    }

    const BOOKWORM: &str = include_str!("../testdata/volumes-SER-Q7K7IYA.json");
    const BOOKWORM_MANGA: &str = include_str!("../testdata/volumes-SER-LKJPCCI.json");
    const GRIMGAR: &str = include_str!("../testdata/volumes-SER-N6KH4FA.json");

    /// The id of the volume `query` picks from a series.
    fn picked(series_id: &str, fixture: &str, query: &str) -> Option<String> {
        let (title, volume) = split_volume(query);
        let all = volumes(fixture);
        pick_volume(&series(series_id), &all, &title, query, &volume)
            .map(|found| all[found.index].id.clone())
    }

    #[test]
    fn splits_a_volume_off_the_query() {
        let cases = [
            ("Berserk Vol. 3", "Berserk", "3"),
            ("Berserk, Vol. 03", "Berserk", "3"),
            ("Berserk v12", "Berserk", "12"),
            ("One Piece #5", "One Piece", "5"),
            ("Berserk (Vol 2)", "Berserk", "2"),
            ("Chainsaw Man Vol. 10.5", "Chainsaw Man", "10.5"),
            (
                "Ascendance of a Bookworm: Part 3 Volume 2",
                "Ascendance of a Bookworm: Part 3",
                "2",
            ),
            ("Mob Psycho 100", "Mob Psycho 100", ""),
            ("Vol. 3", "Vol. 3", ""),
        ];
        for (query, title, volume) in cases {
            assert_eq!(
                split_volume(query),
                (title.to_string(), volume.to_string()),
                "{query}"
            );
        }
    }

    #[test]
    fn reads_titles_as_words() {
        assert_eq!(
            words("Ascendance of a Bookworm: Part 3 Vol. 02"),
            words("ascendance of a bookworm part 3 volume 2")
        );
        assert_eq!(
            words("Arifureta: From Commonplace to World’s Strongest v03"),
            [
                "arifureta",
                "from",
                "commonplace",
                "to",
                "worlds",
                "strongest",
                "volume",
                "3"
            ]
        );
        assert_eq!(words("Level 14.5 & more"), ["level", "14.5", "and", "more"]);
        assert_eq!(words("level.14+"), ["level", "14"]);
    }

    #[test]
    fn ranks_series_by_title() {
        let all = catalogue();
        let ranked = |query: &str| -> Vec<String> {
            let (title, _) = split_volume(query);
            rank_series(&all, &title, query)
                .iter()
                .map(|m| m.series.id.clone())
                .collect()
        };
        // The novel, then its manga, then the spin-offs that start with the same words.
        assert_eq!(
            ranked("Ascendance of a Bookworm")[..3],
            ["SER-Q7K7IYA", "SER-LKJPCCI", "SER-DGZBWMY"]
        );
        assert_eq!(
            ranked("Ascendance of a Bookworm (Manga) Vol. 4")[..2],
            ["SER-LKJPCCI", "SER-Q7K7IYA"]
        );
        assert_eq!(ranked("Apothecary Diaries")[0], "SER-Y47WOHQ");
        assert_eq!(
            ranked("Grimgar of Fantasy and Ash level.2 Everything is Precious")[0],
            "SER-N6KH4FA"
        );
        assert_eq!(ranked("Grimgar 2")[0], "SER-N6KH4FA");
        assert!(ranked("Berserk").is_empty());
    }

    #[test]
    fn picks_the_volume_a_query_names() {
        let cases = [
            ("Ascendance of a Bookworm: Part 3 Volume 2", "VOL-SD5FU4Q"),
            ("Ascendance of a Bookworm Part 3 Vol. 02", "VOL-SD5FU4Q"),
            ("Ascendance of a Bookworm Vol. 3", "VOL-BAWJ2DI"),
            // Its ninth volume, rather than one of the parts' ninth.
            ("Ascendance of a Bookworm v09", "VOL-SD5FU4Q"),
            ("Ascendance of a Bookworm 3", "VOL-BAWJ2DI"),
            (
                "Ascendance of a Bookworm (Light Novel) Vol 3",
                "VOL-BAWJ2DI",
            ),
            ("Ascendance of a Bookworm", "VOL-ILVLS2Y"),
        ];
        for (query, id) in cases {
            assert_eq!(
                picked("SER-Q7K7IYA", BOOKWORM, query).as_deref(),
                Some(id),
                "{query}"
            );
        }
        // A manga part counts its volumes from one again; its position runs through the series.
        assert_eq!(
            picked(
                "SER-LKJPCCI",
                BOOKWORM_MANGA,
                "Ascendance of a Bookworm (Manga) Part 3 Volume 2"
            )
            .as_deref(),
            Some("VOL-KRQFYFA")
        );
        assert_eq!(
            picked(
                "SER-N6KH4FA",
                GRIMGAR,
                "Grimgar of Fantasy and Ash: Everything is Precious"
            )
            .as_deref(),
            Some("VOL-QQKUUAQ")
        );
        assert_eq!(
            picked("SER-N6KH4FA", GRIMGAR, "Grimgar 2").as_deref(),
            Some("VOL-QQKUUAQ")
        );
        // A volume the series does not have, and words no volume of it has: the manga has no
        // fifth part.
        assert_eq!(
            picked(
                "SER-LKJPCCI",
                BOOKWORM_MANGA,
                "Ascendance of a Bookworm Part 5 Volume 3"
            ),
            None
        );
        assert_eq!(
            picked("SER-Q7K7IYA", BOOKWORM, "Ascendance of a Bookworm Vol. 99"),
            None
        );
        assert_eq!(
            picked(
                "SER-Q7K7IYA",
                BOOKWORM,
                "Ascendance of a Bookworm: Hannelore's Fifth Year"
            ),
            None
        );
    }

    #[test]
    fn a_manga_volume_is_a_comic() {
        let page: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/series-page-1.json")).unwrap();
        let all: Vec<Series> = serde_json::from_value(page["series"].clone()).unwrap();
        let manga = all
            .iter()
            .find(|s| s.kind == "MANGA")
            .expect("fixture has a manga series");
        let novel = all
            .iter()
            .find(|s| s.kind == "NOVEL")
            .expect("fixture has a novel series");
        assert_eq!(
            map_volume(&Volume::default(), Some(manga)).media_type,
            "comic"
        );
        assert_eq!(
            map_volume(&Volume::default(), Some(novel)).media_type,
            "book"
        );
    }

    #[test]
    fn maps_a_volume() {
        let volume: Volume =
            serde_json::from_str(include_str!("../testdata/volume-VOL-SD5FU4Q.json")).unwrap();
        let series: Series =
            serde_json::from_str(include_str!("../testdata/serie-VOL-SD5FU4Q.json")).unwrap();
        let book = map_volume(&volume, Some(&series));
        assert_eq!(
            (book.provider_id.as_str(), book.title.as_str()),
            ("VOL-SD5FU4Q", "Ascendance of a Bookworm: Part 3 Volume 2")
        );
        assert_eq!(book.authors, ["Miya Kazuki"]);
        assert_eq!(
            (
                book.publisher,
                book.published_date.as_str(),
                book.published_year.as_str(),
                book.language_code
            ),
            ("J-Novel Club", "2020-10-19", "2020", "en")
        );
        assert_eq!(
            book.series,
            [SeriesEntry {
                series: "Ascendance of a Bookworm".into(),
                sequence: "9".into()
            }]
        );
        assert_eq!(
            (book.cover.as_str(), book.cover_width, book.cover_height),
            (
                "https://cdn.j-novel.club/pub/img/1200/jpg/01J/9/V/J6YEE47E5PGXC9Y7MJBDN.jpg",
                283,
                400
            )
        );
        assert_eq!(
            book.url,
            "https://j-novel.club/series/ascendance-of-a-bookworm#volume-9"
        );
        assert!(book.description.starts_with("Rozemyne, now both"));
        assert!(book
            .description
            .contains("\n\nThis is the most action-packed"));
        assert_eq!(book.genres, ["Fantasy"]);
    }

    #[test]
    fn scales_a_large_cover_to_the_size_served() {
        let cover = |width, height| Cover {
            cover_url:
                "https://cdn.j-novel.club/pub/img/480/webp/01K/Q/W/G8V11HA4RNNCQNYRBMCYH.jpg".into(),
            width,
            height,
            ..Cover::default()
        };
        // Sizes the CDN answered with on 2026-10-22.
        let (url, width, height) = best_cover(&cover(860, 1214));
        assert_eq!(
            (url.as_str(), width, height),
            (
                "https://cdn.j-novel.club/pub/img/1200/jpg/01K/Q/W/G8V11HA4RNNCQNYRBMCYH.jpg",
                850,
                1200
            )
        );
        assert_eq!(best_cover(&cover(3033, 4309)).1, 845);
        assert_eq!(best_cover(&cover(1513, 2150)).1, 844);
        let elsewhere = Cover {
            cover_url: "https://example.com/cover.jpg".into(),
            width: 860,
            height: 1214,
            ..Cover::default()
        };
        assert_eq!(
            best_cover(&elsewhere),
            ("https://example.com/cover.jpg".into(), 0, 0)
        );
    }

    #[test]
    fn credits_writers_only() {
        let manga = volumes(BOOKWORM_MANGA);
        assert_eq!(map_volume(&manga[0], None).authors, ["Miya Kazuki"]);
        let artist_only = Volume {
            creators: vec![
                crate::jnovelclub::Creator {
                    name: "Someone".into(),
                    role: "TRANSLATOR".into(),
                },
                crate::jnovelclub::Creator {
                    name: "Romeda Ando".into(),
                    role: "ARTIST".into(),
                },
            ],
            ..Volume::default()
        };
        assert_eq!(authors(&artist_only), ["Romeda Ando"]);
        assert!(author_matches(&manga[0], "Suzuka"));
        assert!(author_matches(&manga[0], "Kazuki Miya"));
        assert!(!author_matches(&manga[0], "Quof"));
    }

    #[test]
    fn keeps_only_genre_tags() {
        let tags = [
            "romance",
            "Boys’ Love",
            "j-novel knight",
            "available in print",
            "Sci-Fi",
            "science fiction",
        ]
        .map(String::from);
        assert_eq!(genres(&tags), ["Romance", "Boys' Love", "Science Fiction"]);
    }
}
