use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::douban::{Subject, Suggestion};
use crate::PUBLIC_SITE;

/// Longest description BookOrbit keeps.
const MAX_DESCRIPTION_CHARS: usize = 50_000;

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_date: String,
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
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
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
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

// "[哥伦比亚] 加西亚·马尔克斯", "（美）斯蒂芬·金", "【以】尤瓦尔·赫拉利": the bracket is the author's
// country or dynasty, written in front of the name.
static NATIONALITY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[\[［【〔(（][^\]］】〕)）]{1,12}[\]］】〕)）]\s*"));
// "尤瓦尔·赫拉利（Yuval Noah Harari）": the original name after the Chinese one.
static ORIGINAL_NAME: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\s*[(（]\s*[A-Za-z][A-Za-z .,'\-·]*[)）]$"));
// "鲁迅 著" (by), "某某 主编" (edited by).
static ROLE_SUFFIX: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+(?:著|编著|主编|编|撰)$"));
static DATE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(\d{4})\s*(?:[-./年]\s*(\d{1,2})\s*(?:[-./月]\s*(\d{1,2}))?)?"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| regex(r"\d+"));

pub fn map_subject(s: &Subject) -> BookMetadata {
    let title = s.title.trim().to_string();
    let authors = unique(s.authors.iter().map(|name| clean_name(name)));
    let (published_year, published_date) = parse_published(&s.published);
    let isbn = normalize_isbn(&s.isbn);
    let isbn13 = to_isbn13(&isbn);
    let description: String = s
        .intro
        .join("\n\n")
        .chars()
        .take(MAX_DESCRIPTION_CHARS)
        .collect();

    let mut excluded: HashSet<String> = [&title, &s.original_title]
        .into_iter()
        .chain(s.authors.iter())
        .chain(s.translators.iter())
        .map(|name| compact(name))
        .collect();
    excluded.extend(authors.iter().map(|name| compact(name)));
    // The 丛书 row is the publisher's imprint line ("科幻世界·中国科幻基石丛书"), not story order,
    // so it goes with the tags rather than into a library's series field.
    let tags = unique(
        s.tags
            .iter()
            .filter(|tag| !excluded.contains(&compact(tag)))
            .cloned()
            .chain(Some(s.series.trim().to_string()).filter(|series| !series.is_empty())),
    );

    let mut b = BookMetadata {
        media_type: "book".into(),
        provider_id: s.id.clone(),
        url: format!("{PUBLIC_SITE}/subject/{}/", s.id),
        language_code: script_language(&title).to_string(),
        subtitle: s.subtitle.trim().to_string(),
        authors,
        publisher: s.publisher.trim().to_string(),
        published_year,
        published_date,
        description,
        // No cover: Douban's image servers refuse any request without a Douban referer, so the
        // link would only ever show as a broken image.
        isbn: isbn13.clone(),
        isbn13,
        isbn10: if isbn.len() == 10 {
            isbn
        } else {
            String::new()
        },
        tags,
        pages: DIGITS
            .find(&s.pages)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .filter(|pages| (1..100_000).contains(pages))
            .unwrap_or(0),
        title,
        ..BookMetadata::default()
    };
    if !s.original_title.is_empty() && s.original_title != b.title {
        b.original_title = s.original_title.trim().to_string();
    }
    let rating = s.rating.trim().parse::<f64>().unwrap_or(0.0);
    let votes = s.votes.trim().parse::<i64>().unwrap_or(0);
    if votes > 0 && rating > 0.0 && rating <= 10.0 {
        // Douban rates out of 10; the field is out of 5 like every other community rating.
        b.rating = Some((rating / 2.0 * 100.0).round() / 100.0);
        b.rating_count = Some(votes);
    }
    b
}

/// A name the way the book prints it, without Douban's country or dynasty prefix, a trailing
/// original-language name, or a role word.
pub fn clean_name(name: &str) -> String {
    let name = name.trim();
    let stripped = NATIONALITY.replace(name, "");
    let stripped = if stripped.trim().is_empty() {
        name.to_string()
    } else {
        stripped.into_owned()
    };
    let stripped = if !stripped.is_ascii() {
        ORIGINAL_NAME.replace(&stripped, "").into_owned()
    } else {
        stripped
    };
    ROLE_SUFFIX.replace(stripped.trim(), "").trim().to_string()
}

/// True when the suggestion is by the asked-for author: the same name with separators and
/// country prefixes ignored ("加西亚·马尔克斯" is "[哥伦比亚]加西亚•马尔克斯"), or every word of it
/// in any order ("Liu Cixin" is "Cixin Liu").
pub fn author_matches(suggestion: &Suggestion, author: &str) -> bool {
    let wanted = compact(&clean_name(author));
    if wanted.is_empty() {
        return true;
    }
    let wanted_words = words(author);
    suggestion.author_name.split('/').any(|name| {
        let have = compact(&clean_name(name));
        if have.contains(&wanted) || (have.chars().count() >= 2 && wanted.contains(&have)) {
            return true;
        }
        let have_words: HashSet<String> = words(name).into_iter().collect();
        !wanted_words.is_empty() && wanted_words.iter().all(|word| have_words.contains(word))
    })
}

fn compact(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn unique(list: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    list.map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && seen.insert(compact(value)))
        .collect()
}

/// "2008-1" is a year and a month, "2014-11-11" a whole date; both also come as "2008年1月".
/// Only a whole date is given as `publishedDate`.
pub fn parse_published(text: &str) -> (String, String) {
    let Some(c) = DATE.captures(text) else {
        return (String::new(), String::new());
    };
    let year = c[1].to_string();
    let month = c.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
    let day = c.get(3).and_then(|m| m.as_str().parse::<u32>().ok());
    match (month, day) {
        (Some(month @ 1..=12), Some(day @ 1..=31)) => {
            let date = format!("{year}-{month:02}-{day:02}");
            (year, date)
        }
        _ => (year, String::new()),
    }
}

/// Douban records no language, so it is read from the title's script: kana is Japanese, hangul
/// Korean, and Chinese characters alone are Chinese. A title in Latin script says nothing.
fn script_language(title: &str) -> &'static str {
    let has = |range: &[(char, char)]| {
        title
            .chars()
            .any(|c| range.iter().any(|(low, high)| (*low..=*high).contains(&c)))
    };
    if has(&[('\u{3040}', '\u{30ff}')]) {
        "ja"
    } else if has(&[('\u{1100}', '\u{11ff}'), ('\u{ac00}', '\u{d7af}')]) {
        "ko"
    } else if has(&[('\u{3400}', '\u{4dbf}'), ('\u{4e00}', '\u{9fff}')]) {
        "zh"
    } else {
        ""
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::douban::{looks_challenged, parse_subject};

    fn subject(id: &str) -> Subject {
        let html = match id {
            "2567698" => include_str!("../testdata/subject-2567698.html"),
            "6082808" => include_str!("../testdata/subject-6082808.html"),
            "25985021" => include_str!("../testdata/subject-25985021.html"),
            "25841131" => include_str!("../testdata/subject-25841131.html"),
            _ => unreachable!("no fixture for {id}"),
        };
        parse_subject(id, html).expect("fixture is a subject page")
    }

    #[test]
    fn maps_a_subject() {
        let b = map_subject(&subject("2567698"));
        assert_eq!(
            (
                b.provider_id.as_str(),
                b.title.as_str(),
                b.media_type.as_str()
            ),
            ("2567698", "三体", "book")
        );
        assert_eq!(b.subtitle, "“地球往事”三部曲之一");
        assert_eq!(b.authors, ["刘慈欣"]);
        assert_eq!(
            (
                b.publisher.as_str(),
                b.published_year.as_str(),
                b.published_date.as_str()
            ),
            ("重庆出版社", "2008", "")
        );
        assert_eq!(
            (
                b.isbn.as_str(),
                b.isbn13.as_str(),
                b.isbn10.as_str(),
                b.pages
            ),
            ("9787536692930", "9787536692930", "", 302)
        );
        assert!(b.series.is_empty());
        assert!(b.tags.contains(&"科幻世界·中国科幻基石丛书".to_string()));
        assert_eq!((b.rating, b.rating_count), (Some(4.45), Some(518876)));
        assert_eq!(b.language_code, "zh");
        assert!(b.cover.is_empty());
        assert_eq!(b.url, "https://book.douban.com/subject/2567698/");
        let paragraphs: Vec<&str> = b.description.split("\n\n").collect();
        assert_eq!(paragraphs.len(), 3, "{}", b.description);
        assert!(paragraphs[0].starts_with("文化大革命如火如荼进行的同时。"));
        assert_eq!(paragraphs[2], "人类的末日悄然来临。");
        assert!(
            !b.description.contains("刘慈欣，祖籍河南"),
            "took the author's biography"
        );
    }

    #[test]
    fn tags_leave_out_the_title_and_the_author() {
        let b = map_subject(&subject("2567698"));
        assert_eq!(b.tags[..3], ["科幻", "科幻小说", "中国科幻"]);
        assert!(
            !b.tags.iter().any(|t| t == "三体" || t == "刘慈欣"),
            "{:?}",
            b.tags
        );
    }

    #[test]
    fn translators_are_not_authors() {
        let s = subject("6082808");
        assert_eq!(s.translators, ["范晔"]);
        let b = map_subject(&s);
        assert_eq!(b.authors, ["加西亚·马尔克斯"]);
        assert_eq!(
            (b.original_title.as_str(), b.publisher.as_str()),
            ("Cien años de soledad", "南海出版公司")
        );
        assert_eq!((b.rating, b.rating_count), (Some(4.65), Some(481403)));
        assert!(
            !b.tags.contains(&"加西亚·马尔克斯".to_string()),
            "{:?}",
            b.tags
        );
        assert!(b.subtitle.is_empty());
    }

    #[test]
    fn a_folded_blurb_is_read_in_full() {
        let b = map_subject(&subject("25985021"));
        assert_eq!(
            (b.title.as_str(), b.subtitle.as_str()),
            ("人类简史", "从动物到上帝")
        );
        assert_eq!(b.authors, ["尤瓦尔·赫拉利"]);
        assert!(b.description.contains("【内容简介】"), "{}", b.description);
        assert!(!b.description.contains("展开全部"));
        assert!(!b.description.contains('<'));
    }

    #[test]
    fn a_book_in_latin_script() {
        let b = map_subject(&subject("25841131"));
        assert_eq!(
            (b.title.as_str(), b.language_code.as_str()),
            ("The Three-Body Problem", "")
        );
        assert_eq!(b.authors, ["Cixin Liu"]);
        assert_eq!(
            (
                b.publisher.as_str(),
                b.published_year.as_str(),
                b.published_date.as_str()
            ),
            ("Tor Books", "2014", "2014-11-11")
        );
        assert_eq!(b.original_title, "三体");
        assert_eq!(b.description.split("\n\n").count(), 2, "{}", b.description);
        assert!(b.description.ends_with("enormous scope and vision."));
    }

    #[test]
    fn publisher_is_the_first_of_several() {
        let html = r#"<span property="v:itemreviewed">人类简史</span>
<div id="info">
    <span class="pl">出版社:</span>
      <a href="https://book.douban.com/press/2400">中信出版社</a> / <a href="https://book.douban.com/press/1">中信出版集团</a>
    <br>
    <span class="pl">出版年:</span> 2025-1<br/>
</div>"#;
        let b = map_subject(&parse_subject("37295054", html).unwrap());
        assert_eq!(
            (b.publisher.as_str(), b.published_year.as_str()),
            ("中信出版社", "2025")
        );
    }

    #[test]
    fn pages_that_are_not_subjects() {
        let missing = include_str!("../testdata/subject-not-found.html");
        assert!(parse_subject("999999999", missing).is_none());
        assert!(!looks_challenged(missing));
        let challenge = r#"<html><head><title>禁止访问</title></head><body>检测到有异常请求从你的 IP 发出</body></html>"#;
        assert!(parse_subject("1", challenge).is_none());
        assert!(looks_challenged(challenge));
    }

    #[test]
    fn cleans_names() {
        for (raw, want) in [
            ("[哥伦比亚] 加西亚·马尔克斯", "加西亚·马尔克斯"),
            ("[以色列]尤瓦尔·赫拉利", "尤瓦尔·赫拉利"),
            ("【以】尤瓦尔·赫拉利（Yuval Noah Harari）", "尤瓦尔·赫拉利"),
            ("（美）斯蒂芬·金", "斯蒂芬·金"),
            ("鲁迅 著", "鲁迅"),
            ("Cixin Liu", "Cixin Liu"),
            ("[清]", "[清]"),
        ] {
            assert_eq!(clean_name(raw), want, "{raw}");
        }
    }

    #[test]
    fn matches_authors_across_douban_spellings() {
        let suggestions: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../testdata/suggest-renlei.json")).unwrap();
        for entry in &suggestions {
            let s = Suggestion {
                author_name: entry["author_name"].as_str().unwrap().into(),
                ..Suggestion::default()
            };
            assert!(author_matches(&s, "尤瓦尔·赫拉利"), "{}", s.author_name);
            assert!(author_matches(&s, "Yuval Noah Harari") == s.author_name.contains("Harari"));
            assert!(!author_matches(&s, "刘慈欣"));
        }
        let english = Suggestion {
            author_name: "Cixin Liu".into(),
            ..Suggestion::default()
        };
        assert!(author_matches(&english, "Liu Cixin"));
        assert!(author_matches(&english, ""));
    }

    #[test]
    fn reads_publication_dates() {
        for (raw, year, date) in [
            ("2008-1", "2008", ""),
            ("2014-11-11", "2014", "2014-11-11"),
            ("2024-5-4", "2024", "2024-05-04"),
            ("1998年3月", "1998", ""),
            ("2001年5月1日", "2001", "2001-05-01"),
            ("2008-13-01", "2008", ""),
            ("", "", ""),
        ] {
            assert_eq!(
                parse_published(raw),
                (year.to_string(), date.to_string()),
                "{raw}"
            );
        }
    }

    #[test]
    fn isbn_handling() {
        for (input, want) in [
            ("978-7-5442-5399-4", "9787544253994"),
            ("7-5442-5399-1", ""),
            ("7536692935", "7536692935"),
            ("ASIN:B08G9PRS1K", ""),
        ] {
            assert_eq!(normalize_isbn(input), want, "{input}");
        }
        assert_eq!(to_isbn13("7536692935"), "9787536692930");
        let b = map_subject(&Subject {
            id: "1".into(),
            title: "x".into(),
            isbn: "7-5366-9293-5".into(),
            ..Subject::default()
        });
        assert_eq!(
            (b.isbn13.as_str(), b.isbn10.as_str()),
            ("9787536692930", "7536692935")
        );
    }
}
