use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::fantlab::{append_unique, Author, Edition, Genre, GenreGroup, Work};

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
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
    pub isbn: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub original_language_code: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pages: i64,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub original_title: String,
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

// fantlab's classifier groups. Reader age (5) and plot linearity (7) describe nothing a library
// filters on, so they are left out.
const GENRE_GROUPS: &[i64] = &[1];
const TAG_GROUPS: &[i64] = &[2, 3, 4, 6];

/// A classifier label counts once at least half of the voters agreed on it.
const MIN_GENRE_SHARE: f64 = 0.5;

pub fn map_work(w: &Work, site: &str, language: &str) -> BookMetadata {
    let use_original = !language.is_empty()
        && language != "ru"
        && language == w.lang_code
        && !w.name_orig.is_empty();
    let (title, title_lang) = if use_original || w.name.is_empty() {
        (w.name_orig.as_str(), w.lang_code.as_str())
    } else {
        (w.name.as_str(), "ru")
    };

    let mut b = BookMetadata {
        media_type: "book".into(),
        provider_id: w.id.to_string(),
        url: format!("{site}/work{}", w.id),
        title: title.trim().to_string(),
        authors: work_authors(&w.authors, title_lang != "ru"),
        description: clean_text(&w.description),
        cover: absolute_url(site, &w.image),
        genres: genre_labels(&w.genre_groups, GENRE_GROUPS),
        tags: genre_labels(&w.genre_groups, TAG_GROUPS),
        language_code: title_lang.to_string(),
        ..BookMetadata::default()
    };
    if w.year > 0 {
        b.published_year = w.year.to_string();
    }
    if !w.lang_code.is_empty() && w.lang_code != title_lang {
        b.original_language_code = w.lang_code.clone();
    }
    if !w.name_orig.is_empty() && w.name_orig != b.title {
        b.original_title = w.name_orig.clone();
    }
    b.series = w
        .cycles
        .iter()
        .map(|cycle| Series {
            series: cycle.name.clone(),
            sequence: String::new(),
        })
        .collect();
    if w.voters > 0 && w.rating > 0.0 && w.rating <= 10.0 {
        // fantlab rates out of 10; the field is out of 5 like every other community rating.
        b.rating = Some((w.rating / 2.0 * 100.0).round() / 100.0);
        b.rating_count = Some(w.voters);
    }
    b.author = b.authors.join(", ");
    b
}

/// Describes the printed book an ISBN names. The edition supplies what is printed on it; its
/// single work, when there is one, supplies description, genres, series and rating.
pub fn map_edition(
    e: &Edition,
    work: Option<&Work>,
    site: &str,
    preferred_isbn: &str,
) -> BookMetadata {
    let mut b = match work {
        Some(work) => map_work(work, site, ""),
        None => BookMetadata {
            media_type: "book".into(),
            ..BookMetadata::default()
        },
    };
    b.provider_id = format!("e{}", e.id);
    b.url = format!("{site}/edition{}", e.id);
    if !e.name.is_empty() {
        b.title = e.name.trim().to_string();
    }
    if !e.authors.is_empty() {
        b.authors = e.authors.clone();
        b.author = e.authors.join(", ");
    }
    if !e.lang_code.is_empty() {
        b.language_code = e.lang_code.clone();
    }
    b.original_language_code.clear();
    b.original_title.clear();
    if let Some(work) = work {
        if !work.lang_code.is_empty() && work.lang_code != b.language_code {
            b.original_language_code = work.lang_code.clone();
        }
        if !work.name_orig.is_empty() && work.name_orig != b.title {
            b.original_title = work.name_orig.clone();
        }
    }
    if let Some(publisher) = e.publishers.first() {
        b.publisher = publisher.clone();
    }
    if e.year > 0 {
        b.published_year = e.year.to_string();
    }
    if e.pages > 0 {
        b.pages = e.pages;
    }
    let cover = absolute_url(site, &e.image);
    if !cover.is_empty() {
        b.cover = cover;
    }
    if b.description.is_empty() {
        b.description = clean_text(&e.description);
    }
    (b.isbn13, b.isbn10) = pick_isbns(&e.isbns, preferred_isbn);
    b.isbn = b.isbn13.clone();
    b
}

fn work_authors(authors: &[Author], original: bool) -> Vec<String> {
    let mut out = Vec::new();
    for a in authors.iter().filter(|a| a.kind == "autor") {
        let name = if original && !a.name_orig.is_empty() {
            &a.name_orig
        } else {
            &a.name
        };
        append_unique(&mut out, name.trim());
    }
    out
}

fn genre_labels(groups: &[GenreGroup], wanted: &[i64]) -> Vec<String> {
    fn walk(genres: &[Genre], out: &mut Vec<String>) {
        for g in genres.iter().filter(|g| g.percent >= MIN_GENRE_SHARE) {
            append_unique(
                out,
                &g.label.split_whitespace().collect::<Vec<_>>().join(" "),
            );
            walk(&g.children, out);
        }
    }
    let mut out = Vec::new();
    for group in groups.iter().filter(|group| wanted.contains(&group.id)) {
        walk(&group.genres, &mut out);
    }
    out
}

static SPOILER_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\[spoiler(?:=[^\]]*)?\].*?\[/spoiler\]").expect("valid pattern")
});
static MARKUP_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\[/?(?:b|i|u|s|url|user|autor|work|edition|pub|series|quote|img|color|size|link|art|film|person|translator|dictor|award|p|center|right|left|list|\*)(?:=[^\]]*)?\]")
        .expect("valid pattern")
});
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid pattern"));

/// Turns fantlab's BBCode into plain paragraphs. Spoiler blocks are dropped rather than unwrapped:
/// stripping the tags alone would print the spoiler as part of the blurb.
pub fn clean_text(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    let s = SPOILER_BLOCK.replace_all(&s, "");
    let s = MARKUP_TAG.replace_all(&s, "");
    let s = HTML_TAG.replace_all(&s, "");
    let s = html_escape::decode_html_entities(&s);
    s.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn absolute_url(site: &str, path: &str) -> String {
    if path.starts_with("https://") || path.starts_with("http://") {
        path.to_string()
    } else if path.starts_with('/') {
        format!("{site}{path}")
    } else {
        String::new()
    }
}

/// A checksum-valid ISBN-10 or ISBN-13 without punctuation, or "".
pub fn normalize_isbn(s: &str) -> String {
    let isbn: String = s
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X')
        .collect();
    match isbn.len() {
        10 if valid_isbn10(&isbn) => isbn,
        13 if valid_isbn13(&isbn) => isbn,
        _ => String::new(),
    }
}

fn valid_isbn10(s: &str) -> bool {
    let mut sum = 0;
    for (i, c) in s.chars().enumerate() {
        let digit = match c {
            'X' if i == 9 => 10,
            '0'..='9' => c as u32 - '0' as u32,
            _ => return false,
        };
        sum += digit * (10 - i as u32);
    }
    sum % 11 == 0
}

fn valid_isbn13(s: &str) -> bool {
    let mut sum = 0;
    for (i, c) in s.chars().enumerate() {
        let Some(digit) = c.to_digit(10) else {
            return false;
        };
        sum += if i % 2 == 1 { digit * 3 } else { digit };
    }
    sum % 10 == 0
}

/// Converts a valid ISBN-10 and passes a valid ISBN-13 through.
fn to_isbn13(isbn: &str) -> String {
    match isbn.len() {
        13 => isbn.to_string(),
        10 => {
            let body = format!("978{}", &isbn[..9]);
            let sum: u32 = body
                .chars()
                .enumerate()
                .map(|(i, c)| c.to_digit(10).unwrap_or(0) * if i % 2 == 1 { 3 } else { 1 })
                .sum();
            format!("{body}{}", (10 - sum % 10) % 10)
        }
        _ => String::new(),
    }
}

pub fn list_contains_isbn<S: AsRef<str>>(list: &[S], target: &str) -> bool {
    let want = to_isbn13(target);
    list.iter().any(|candidate| {
        let isbn = normalize_isbn(candidate.as_ref());
        !isbn.is_empty() && to_isbn13(&isbn) == want
    })
}

/// Prefers the ISBN the client searched for, and only pairs an ISBN-10 that is the same book as
/// the chosen ISBN-13: editions list reprints and co-publishers' numbers side by side.
fn pick_isbns(list: &[String], preferred: &str) -> (String, String) {
    let valid: Vec<String> = list
        .iter()
        .map(|candidate| normalize_isbn(candidate))
        .filter(|isbn| !isbn.is_empty())
        .collect();
    let mut isbn13 = String::new();
    let preferred = normalize_isbn(preferred);
    if !preferred.is_empty() && list_contains_isbn(&valid, &preferred) {
        isbn13 = to_isbn13(&preferred);
    }
    if isbn13.is_empty() {
        if let Some(first) = valid.first() {
            isbn13 = to_isbn13(first);
        }
    }
    let isbn10 = valid
        .iter()
        .find(|isbn| isbn.len() == 10 && to_isbn13(isbn) == isbn13)
        .cloned()
        .unwrap_or_default();
    (isbn13, isbn10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fantlab::{parse_edition, parse_work, Object};

    const SITE: &str = "https://fantlab.ru";

    fn fixture(json: &str) -> Object {
        serde_json::from_str(json).expect("fixture parses")
    }

    fn work_3104() -> Work {
        parse_work(&fixture(include_str!("../testdata/work-3104.json")))
    }

    fn work_1255() -> Work {
        parse_work(&fixture(include_str!("../testdata/work-1255.json")))
    }

    #[test]
    fn map_work_uses_russian_title_by_default() {
        let b = map_work(&work_3104(), SITE, "");
        assert_eq!(
            (b.title.as_str(), b.original_title.as_str()),
            ("Солярис", "Solaris")
        );
        assert_eq!(
            (b.authors.as_slice(), b.author.as_str()),
            (["Станислав Лем".to_string()].as_slice(), "Станислав Лем")
        );
        assert_eq!(
            (b.provider_id.as_str(), b.url.as_str()),
            ("3104", "https://fantlab.ru/work3104")
        );
        assert_eq!(
            (
                b.published_year.as_str(),
                b.language_code.as_str(),
                b.original_language_code.as_str()
            ),
            ("1961", "ru", "pl")
        );
        assert_eq!(
            b.cover,
            "https://fantlab.ru/images/editions/big/121417?r=1643254880"
        );
        assert_eq!((b.rating, b.rating_count), (Some(4.31), Some(9103)));
        assert!(
            b.description
                .starts_with("Солярис. Планета где-то в далёком космосе"),
            "{}",
            b.description
        );
    }

    #[test]
    fn map_work_keeps_only_agreed_classifier_labels() {
        let b = map_work(&work_3104(), SITE, "");
        assert!(
            b.genres.contains(&"Фантастика".into())
                && b.genres.contains(&"Планетарная фантастика".into()),
            "{:?}",
            b.genres
        );
        assert!(
            !b.genres
                .contains(&"«Мягкая» (гуманитарная) научная фантастика".into()),
            "{:?}",
            b.genres
        );
        for want in ["Философское", "Контакт", "Далёкое будущее"] {
            assert!(
                b.tags.contains(&want.into()),
                "tags {:?} miss {want}",
                b.tags
            );
        }
        for unwanted in [
            "Любой",
            "Линейный с экскурсами",
            "Планеты  другой звёздной системы",
        ] {
            assert!(
                !b.tags.contains(&unwanted.into()),
                "tags {:?} have {unwanted}",
                b.tags
            );
        }
    }

    #[test]
    fn map_work_uses_original_title_for_its_own_language() {
        let b = map_work(&work_3104(), SITE, "pl");
        assert_eq!(
            (b.title.as_str(), b.language_code.as_str()),
            ("Solaris", "pl")
        );
        assert_eq!(b.authors, ["Stanisław Lem"]);
        assert!(b.original_title.is_empty() && b.original_language_code.is_empty());
    }

    #[test]
    fn map_work_reads_series() {
        let b = map_work(&work_1255(), SITE, "");
        assert_eq!(
            b.series,
            [Series {
                series: "Дозоры".into(),
                sequence: String::new(),
            }]
        );
        assert!(b.original_language_code.is_empty());
    }

    #[test]
    fn map_edition_takes_printed_details_from_the_edition() {
        let edition = parse_edition(&fixture(include_str!("../testdata/edition-723.json")));
        assert_eq!(edition.work_ids, [1255]);
        let b = map_edition(&edition, Some(&work_1255()), SITE, "9785170254767");
        assert_eq!(
            (b.provider_id.as_str(), b.url.as_str()),
            ("e723", "https://fantlab.ru/edition723")
        );
        assert_eq!(
            (b.isbn13.as_str(), b.isbn10.as_str(), b.isbn.as_str()),
            ("9785170254767", "5170254768", "9785170254767")
        );
        assert_eq!(
            (b.publisher.as_str(), b.published_year.as_str(), b.pages),
            ("АСТ", "2004", 384)
        );
        assert!(b.cover.contains("/images/editions/big/723"), "{}", b.cover);
        assert_eq!(
            b.series,
            [Series {
                series: "Дозоры".into(),
                sequence: String::new(),
            }]
        );
        assert!(b.rating.is_some());
        assert!(
            b.description
                .starts_with("Где-то совсем рядом с нами существует другой мир."),
            "{}",
            b.description
        );
    }

    #[test]
    fn placeholder_titles_are_not_titles() {
        let raw = fixture(
            r#"{"work_id": 1996687, "work_name": "", "work_name_orig": "[оригинальное название не установлено]",
                "lang_code": "nl", "rating": [], "classificatory": []}"#,
        );
        let b = map_work(&parse_work(&raw), SITE, "");
        assert!(b.title.is_empty() && b.original_title.is_empty());
    }

    #[test]
    fn clean_text_strips_markup_and_drops_spoilers() {
        let input =
            "[b]Первая[/b] строка с [autor=9]Лемом[/autor] и [url=http://x]ссылкой[/url].\r\n\r\n\
                     [spoiler]Убийца - дворецкий[/spoiler]Вторая &laquo;строка&raquo; [sic]";
        assert_eq!(
            clean_text(input),
            "Первая строка с Лемом и ссылкой.\n\nВторая «строка» [sic]"
        );
    }

    #[test]
    fn isbn_handling() {
        for (input, want) in [
            ("978-5-17-025476-7", "9785170254767"),
            ("5-17-025476-8", "5170254768"),
            ("978-5-17-025476-8", ""),
            ("ASIN:B08G9PRS1K", ""),
        ] {
            assert_eq!(normalize_isbn(input), want, "{input}");
        }
        assert_eq!(to_isbn13("5170254768"), "9785170254767");
        assert!(list_contains_isbn(&[" 5-17-025476-8"], "9785170254767"));
        let picked = pick_isbns(
            &[
                "5-237-01511-5".into(),
                "978-5-17-025476-7".into(),
                "5-17-025476-8".into(),
            ],
            "",
        );
        assert_eq!(
            picked,
            ("9785237015119".to_string(), "5237015115".to_string())
        );
    }
}
