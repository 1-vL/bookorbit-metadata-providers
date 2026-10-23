use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::mangabaka::{Publisher, Series, Title};

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
    #[serde(skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub series: Vec<SeriesEntry>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub language_code: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub published_date: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub cover_width: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub cover_height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
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

// MangaBaka descriptions are Markdown; clients expect plain text.
static MARKDOWN_LINK: LazyLock<Regex> = LazyLock::new(|| regex(r"\[([^\]]+)\]\([^)]*\)"));
static MARKDOWN_STRONG: LazyLock<Regex> = LazyLock::new(|| regex(r"\*\*(.+?)\*\*|__(.+?)__"));
static MARKDOWN_EM: LazyLock<Regex> = LazyLock::new(|| regex(r"\*(.+?)\*"));
static HARD_BREAK: LazyLock<Regex> = LazyLock::new(|| regex(r" {2,}\n"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| regex(r"\n{3,}"));

/// Takes a volume number off a search query: the title is what MangaBaka knows, the number is
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

pub fn map_series(s: &Series, volume: &str) -> BookMetadata {
    let series_title = tidy_title(&english_title(s));
    let raw_cover = s.cover.as_ref().and_then(|cover| cover.raw.as_ref());
    let mut authors = unique(s.authors.as_deref().unwrap_or_default());
    if authors.is_empty() {
        authors = unique(s.artists.as_deref().unwrap_or_default());
    }
    let authors: Vec<String> = authors.iter().map(|name| given_name_first(name)).collect();
    let publishers = s.publishers.as_deref().unwrap_or_default();
    let publisher = pick_publisher(publishers);
    // The English publisher stands for the English edition, so its language goes with it.
    let english = publishers.iter().any(|p| {
        p.kind.as_deref() == Some("English") && p.name.as_deref() == Some(publisher.as_str())
    });
    // The series' start says when volume 1 came out, not a later one.
    let dated = volume.is_empty() || volume == "1";
    let published_date = s
        .published
        .as_ref()
        .and_then(|published| published.start_date.clone())
        .filter(|date| dated && date.len() == "2006-01-02".len())
        .unwrap_or_default();
    BookMetadata {
        // Comics unless MangaBaka files it as a novel, so a client can tell a light novel from its
        // manga of the same name.
        media_type: if s.kind.as_deref() == Some("novel") {
            "book"
        } else {
            "comic"
        },
        // A volume's id carries its number, so looking it up again answers the same book.
        provider_id: if volume.is_empty() {
            s.id.to_string()
        } else {
            format!("{}v{volume}", s.id)
        },
        title: if volume.is_empty() {
            series_title.clone()
        } else {
            format!("{series_title}, Vol. {volume}")
        },
        series: vec![SeriesEntry {
            series: series_title,
            sequence: volume.to_string(),
        }],
        authors,
        publisher,
        published_year: s
            .year
            .filter(|year| dated && *year > 0)
            .map(|year| year.to_string())
            .unwrap_or_default(),
        description: s.description.as_deref().map(plain_text).unwrap_or_default(),
        cover: raw_cover
            .and_then(|raw| raw.url.clone())
            .unwrap_or_default(),
        genres: genre_labels(s.genres.as_deref().unwrap_or_default()),
        tags: unique(s.tags.as_deref().unwrap_or_default()),
        language_code: if english {
            "en".to_string()
        } else {
            s.original_language.clone().unwrap_or_default()
        },
        published_date,
        cover_width: raw_cover.and_then(|raw| raw.width).unwrap_or(0),
        cover_height: raw_cover.and_then(|raw| raw.height).unwrap_or(0),
        // MangaBaka rates out of 100; the spec rates out of 5.
        rating: s
            .rating
            .filter(|rating| *rating > 0.0)
            .map(|rating| (rating.min(100.0) / 20.0 * 100.0).round() / 100.0),
        url: s.canonical_url.clone().unwrap_or_default(),
    }
}

/// The official English name, which is what an English edition's cover and a file name carry.
/// MangaBaka's own `title` is sometimes a fan or older name ("FULL METAL ALCHEMIST").
fn english_title(s: &Series) -> String {
    let english: Vec<&Title> = s
        .titles
        .iter()
        .flatten()
        .filter(|t| {
            t.language.as_deref() == Some("en")
                && t.title.as_deref().is_some_and(|t| !t.trim().is_empty())
        })
        .filter(|t| t.traits.iter().flatten().any(|tr| tr == "official"))
        .collect();
    english
        .iter()
        .find(|t| t.is_primary == Some(true))
        .or_else(|| english.first())
        .and_then(|t| t.title.clone())
        .or_else(|| s.title.clone().filter(|t| !t.trim().is_empty()))
        .or_else(|| s.romanized_title.clone())
        .unwrap_or_default()
}

const SMALL_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "in", "nor", "of", "on", "or", "the", "to",
    "vs", "x",
];

/// Catalogue capitals ("ONE PIECE", "SPY×FAMILY") read as the title a library shelves: "One
/// Piece", "Spy x Family". A title that already mixes case is left as its publisher wrote it.
pub fn tidy_title(title: &str) -> String {
    let title = SPACES
        .replace_all(&title.replace('×', " x "), " ")
        .trim()
        .to_string();
    // A one-letter word ("x") says nothing about whether the rest is shouted.
    let letters: Vec<char> = title
        .split(' ')
        .filter(|word| word.chars().filter(|c| c.is_alphabetic()).count() > 1)
        .flat_map(str::chars)
        .filter(|c| c.is_alphabetic())
        .collect();
    let shouted = letters.len() > 3
        && letters.iter().all(|c| !c.is_lowercase())
        && letters.iter().any(|c| c.is_uppercase());
    if !shouted {
        return title;
    }
    title
        .split(' ')
        .enumerate()
        .map(|(i, word)| {
            let lower = word.to_lowercase();
            if i > 0 && SMALL_WORDS.contains(&lower.as_str()) {
                return lower;
            }
            let mut chars = lower.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// "MIURA Kentaro" is the family-name-first, family-name-in-capitals way MangaBaka writes some
/// creators; a library files the same person as "Kentaro Miura".
pub fn given_name_first(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let is_upper = |w: &str| {
        w.chars().filter(|c| c.is_alphabetic()).count() > 1 && !w.chars().any(|c| c.is_lowercase())
    };
    if words.len() < 2 || !is_upper(words[0]) || words[1..].iter().any(|w| is_upper(w)) {
        return name.trim().to_string();
    }
    let mut family = words[0].to_lowercase();
    if let Some(first) = family.chars().next() {
        family = first.to_uppercase().chain(family.chars().skip(1)).collect();
    }
    format!("{} {family}", words[1..].join(" "))
}

/// Lowercase words of a title, with "×" read as "x", for comparing against what was asked for.
pub fn title_words(title: &str) -> Vec<String> {
    title
        .replace('×', " x ")
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn all_titles(s: &Series) -> Vec<&str> {
    [&s.title, &s.romanized_title, &s.native_title]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(s.titles.iter().flatten().filter_map(|t| t.title.as_deref()))
        .collect()
}

/// One of the series' names is exactly what was asked for.
pub fn names_exactly(s: &Series, query: &str) -> bool {
    let wanted = title_words(query);
    !wanted.is_empty()
        && all_titles(s)
            .iter()
            .any(|title| title_words(title) == wanted)
}

/// One of the series' names shares a whole word with the query. MangaBaka's search is fuzzy, so
/// "Dune" also brings back "June"; a record no name of which has any asked-for word is not it.
pub fn shares_a_word(s: &Series, query: &str) -> bool {
    let wanted = title_words(query);
    let significant: Vec<&String> = wanted.iter().filter(|w| w.chars().count() >= 3).collect();
    let wanted: Vec<&String> = if significant.is_empty() {
        wanted.iter().collect()
    } else {
        significant
    };
    all_titles(s).iter().any(|title| {
        let words = title_words(title);
        wanted.iter().any(|w| words.contains(w))
    })
}

/// The English-language publisher is the edition most libraries hold, then the original one.
fn pick_publisher(publishers: &[Publisher]) -> String {
    for wanted in ["English", "Original"] {
        if let Some(name) = publishers
            .iter()
            .find(|p| {
                p.kind.as_deref() == Some(wanted)
                    && p.name.as_deref().is_some_and(|name| !name.is_empty())
            })
            .and_then(|p| p.name.clone())
        {
            return name;
        }
    }
    publishers
        .first()
        .and_then(|p| p.name.clone())
        .unwrap_or_default()
}

/// "school_life" reads as "School Life".
fn genre_labels(genres: &[String]) -> Vec<String> {
    let labels: Vec<String> = genres
        .iter()
        .map(|genre| {
            genre
                .replace('_', " ")
                .split_whitespace()
                .map(|word| {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().chain(chars).collect())
                        .unwrap_or_default()
                })
                .collect::<Vec<String>>()
                .join(" ")
        })
        .collect();
    unique(&labels)
}

pub fn plain_text(markdown: &str) -> String {
    let text = MARKDOWN_LINK.replace_all(markdown, "$1");
    let text = MARKDOWN_STRONG.replace_all(&text, "$1$2");
    let text = MARKDOWN_EM.replace_all(&text, "$1");
    let text = text.replace("\r\n", "\n");
    let text = HARD_BREAK.replace_all(&text, "\n");
    BLANK_LINES.replace_all(&text, "\n\n").trim().to_string()
}

fn unique(list: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    list.iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
        .map(str::to_string)
        .collect()
}

/// True when every word of the asked-for author is in one of the series' names, in any order:
/// MangaBaka writes "MIURA Kentaro" where a file says "Kentaro Miura". A name that differs only
/// in its separators matches too: "Chu-Gong" is "Chugong".
pub fn author_matches(s: &Series, author: &str) -> bool {
    let wanted = name_words(author);
    if wanted.is_empty() {
        return true;
    }
    let names = s.authors.iter().flatten().chain(s.artists.iter().flatten());
    names.into_iter().any(|name| {
        let words = name_words(name);
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

    pub fn load_series(json: &str) -> Series {
        #[derive(serde::Deserialize)]
        struct Body {
            data: Series,
        }
        serde_json::from_str::<Body>(json)
            .expect("fixture parses")
            .data
    }

    const SERIES_1692: &str = include_str!("../testdata/series-1692.json");

    #[test]
    fn splits_a_volume_off_the_query() {
        let cases = [
            ("Berserk Vol. 3", "Berserk", "3"),
            ("Berserk, Vol. 03", "Berserk", "3"),
            ("Berserk v12", "Berserk", "12"),
            ("Berserk volume 41", "Berserk", "41"),
            ("One Piece #5", "One Piece", "5"),
            ("Berserk (Vol 2)", "Berserk", "2"),
            ("Chainsaw Man Vol. 10.5", "Chainsaw Man", "10.5"),
            ("Mob Psycho 100", "Mob Psycho 100", ""),
            ("Revolutionary Girl Utena", "Revolutionary Girl Utena", ""),
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
    fn maps_a_series() {
        let book = map_series(&load_series(SERIES_1692), "3");
        assert_eq!(
            (
                book.provider_id.as_str(),
                book.title.as_str(),
                book.media_type
            ),
            ("1692v3", "Berserk, Vol. 3", "comic")
        );
        assert_eq!(book.authors, ["Kentaro Miura", "Kouji Mori"]);
        assert_eq!(book.publisher, "Dark Horse Manga");
        // A later volume keeps no date: the series' start is volume 1's.
        assert_eq!(
            (
                book.published_year.as_str(),
                book.published_date.as_str(),
                book.language_code.as_str()
            ),
            ("", "", "en")
        );
        assert_eq!(book.rating, Some(4.57));
        assert_eq!(
            book.series,
            [SeriesEntry {
                series: "Berserk".into(),
                sequence: "3".into()
            }]
        );
        assert!(!book.cover.is_empty());
        assert_eq!((book.cover_width, book.cover_height), (751, 1051));
        assert_eq!(book.genres[..3], ["Action", "Adventure", "Drama"]);
        assert_eq!(
            book.genres.last().map(String::as_str),
            Some("Award Winning")
        );
        assert_eq!(book.url, "https://mangabaka.org/manga/1692/BERSERK");
    }

    #[test]
    fn dates_the_first_volume_and_the_series_itself() {
        let series = load_series(SERIES_1692);
        for volume in ["", "1"] {
            let book = map_series(&series, volume);
            assert_eq!(
                (book.published_year.as_str(), book.published_date.as_str()),
                ("1989", "1989-08-25"),
                "{volume:?}"
            );
        }
        assert_eq!(map_series(&series, "").title, "Berserk");
        assert_eq!(map_series(&series, "").provider_id, "1692");
    }

    #[test]
    fn only_a_novel_is_a_book() {
        let typed = |kind: &str| {
            map_series(
                &Series {
                    kind: Some(kind.into()),
                    ..Series::default()
                },
                "",
            )
            .media_type
        };
        assert_eq!(
            [typed("manga"), typed("manhwa"), typed("novel")],
            ["comic", "comic", "book"]
        );
    }

    #[test]
    fn prefers_the_official_english_title() {
        let series = Series {
            title: Some("FULL METAL ALCHEMIST".into()),
            titles: Some(vec![
                Title {
                    language: Some("en".into()),
                    title: Some("Alchemist of Steel".into()),
                    traits: Some(vec![]),
                    is_primary: Some(false),
                },
                Title {
                    language: Some("en".into()),
                    title: Some("FULLMETAL ALCHEMIST".into()),
                    traits: Some(vec!["official".into()]),
                    is_primary: Some(true),
                },
            ]),
            ..Series::default()
        };
        assert_eq!(map_series(&series, "").title, "Fullmetal Alchemist");
    }

    #[test]
    fn tidies_catalogue_capitals() {
        for (raw, tidy) in [
            ("ONE PIECE", "One Piece"),
            ("SPY×FAMILY", "Spy x Family"),
            ("ATTACK ON TITAN", "Attack on Titan"),
            (
                "Frieren: Beyond Journey’s End",
                "Frieren: Beyond Journey’s End",
            ),
            ("GTO", "GTO"),
        ] {
            assert_eq!(tidy_title(raw), tidy, "{raw}");
        }
    }

    #[test]
    fn puts_the_given_name_first() {
        assert_eq!(given_name_first("MIURA Kentaro"), "Kentaro Miura");
        assert_eq!(given_name_first("Eiichirou Oda"), "Eiichirou Oda");
        assert_eq!(given_name_first("CLAMP"), "CLAMP");
        assert_eq!(given_name_first("Chugong"), "Chugong");
    }

    #[test]
    fn keeps_only_series_that_share_a_word() {
        let named = |title: &str| Series {
            title: Some(title.into()),
            ..Series::default()
        };
        assert!(shares_a_word(&named("SPY×FAMILY"), "Spy x Family"));
        assert!(!shares_a_word(&named("June"), "Dune"));
        assert!(!shares_a_word(
            &named("In the SAND DUNES where the black feathers dance"),
            "Dune"
        ));
        assert!(names_exactly(&named("SPY×FAMILY"), "Spy x Family"));
        assert!(!names_exactly(
            &named("Spy x Family: Family Portrait"),
            "Spy x Family"
        ));
    }

    #[test]
    fn turns_markdown_into_plain_text() {
        let got = plain_text("**THE WEAKEST HUNTER!**\n\n\n\nFrom [Tapas](https://tapas.io):  \nThe *prototype* chapter, __really__.");
        assert_eq!(
            got,
            "THE WEAKEST HUNTER!\n\nFrom Tapas:\nThe prototype chapter, really."
        );
    }

    #[test]
    fn labels_genres() {
        let genres = ["school_life", "award_winning", "action", "Action"].map(String::from);
        assert_eq!(
            genre_labels(&genres),
            ["School Life", "Award Winning", "Action"]
        );
    }

    #[test]
    fn matches_authors_in_any_order() {
        let s = load_series(SERIES_1692);
        assert!(author_matches(&s, "Kentaro Miura"));
        assert!(author_matches(&s, "miura"));
        assert!(!author_matches(&s, "Eiichiro Oda"));
        let chugong = Series {
            authors: Some(vec!["Chu-Gong".into()]),
            ..Series::default()
        };
        assert!(author_matches(&chugong, "Chugong"));
    }
}
