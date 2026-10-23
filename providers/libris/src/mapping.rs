use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::libris::{
    Agent, Contribution, Instance, Many, Media, Term, Title, Work, AUDIOBOOK_CATEGORIES,
    COMPONENT_PART, TEXT_CONTENT,
};

pub const SITE: &str = "https://libris.kb.se";

/// Libris record ids are 15 or 16 lowercase letters and digits; the margin keeps a longer one
/// working.
pub const MAX_ID_LENGTH: usize = 20;

const AUTHOR: &str = "https://id.kb.se/relator/author";
const NARRATOR: &str = "https://id.kb.se/relator/narrator";

const GENRE_SCHEMES: &[&str] = &[
    "https://id.kb.se/term/saogf/",
    "https://id.kb.se/term/barngf/",
];

/// Genre terms that name a format, which the record's medium already says.
const FORMAT_GENRES: &[&str] = &["ljudböcker", "talböcker", "e-böcker"];

const QUERY_MAX_CHARS: usize = 200;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: &'static str,
    pub provider_id: String,
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
    pub isbn: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub pages: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub duration: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesEntry>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub language_code: String,
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

static SPACES: LazyLock<Regex> = LazyLock::new(|| regex(r"\s+"));
static LIFE_DATES: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)^(?:[fd]\.\s*|född\s+|död\s+|ca\s+)?\d{3,4}\??(?:\s*-\s*(?:\d{3,4}\??)?)?\.?$")
});
static YEAR: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|\D)(\d{4})(?:\D|$)"));
static NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"\d+(?:[.,]\d+)?"));
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| regex(r"\[[^\]]*\]"));
static PAGE_UNIT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\d\s*\]?\s*(?:s|sid|sidor|sider|p|pp|pages?)(?:\.|\b|$)"));
static HOURS: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)(\d+(?:[.,]\d+)?)\s*(?:tim(?:mar|me)?|h|hr|hrs|hours?)(?:\.|\b|$)")
});
static MINUTES: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(\d+)\s*min(?:uter|utes?)?(?:\.|\b|$)"));
static SECONDS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(\d+)\s*(?:sek(?:under)?|sec(?:onds?)?)(?:\.|\b|$)"));
static CLOCK: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?-u:\b)(\d{1,3}):([0-5]\d):([0-5]\d)(?-u:\b)"));
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"<[^>]+>"));
// Summaries from the book trade end with their source: "[Bokinfo]", "[Elib]".
static SOURCE_TAG: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*\[[^\[\]\n]{1,40}\]\s*$"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| regex(r"\n\s*\n\s*"));

/// The medium a record is, from its work's categories, or None for anything else (films, music,
/// software, maps) and for chapters catalogued on their own.
pub fn media_of(instance: &Instance) -> Option<Media> {
    if instance.category.iter().any(|term| term.is(COMPONENT_PART)) {
        return None;
    }
    let terms: Vec<&Term> = instance
        .work
        .iter()
        .flat_map(|work| work.category.iter().chain(work.genre_form.iter()))
        .collect();
    if terms
        .iter()
        .any(|term| AUDIOBOOK_CATEGORIES.iter().any(|id| term.is(id)))
    {
        Some(Media::Audiobook)
    } else if terms.iter().any(|term| term.is(TEXT_CONTENT)) {
        Some(Media::Book)
    } else {
        None
    }
}

pub fn map_instance(instance: &Instance, media: Media, preferred_isbn: &str) -> BookMetadata {
    let work = instance.work.first();
    let (mut title, mut subtitle) = title_of(&instance.titles);
    if title.is_empty() {
        (title, subtitle) = work.map(|w| title_of(&w.titles)).unwrap_or_default();
    }
    let contributions: Vec<&Contribution> = work
        .into_iter()
        .flat_map(|w| w.contribution.iter())
        .chain(instance.contribution.iter())
        .collect();
    let (publisher, published_year) = publication(instance);
    let provider_id = record_id(instance);
    let (isbn13, isbn10) = pick_isbns(&isbn_values(instance), preferred_isbn);

    BookMetadata {
        media_type: media.as_str(),
        url: if provider_id.is_empty() {
            String::new()
        } else {
            format!("{SITE}/{provider_id}")
        },
        provider_id,
        title,
        subtitle,
        authors: people(&contributions, AUTHOR),
        narrators: people(&contributions, NARRATOR),
        publisher,
        published_year,
        description: description(instance),
        cover: cover(instance),
        isbn: if isbn13.is_empty() {
            isbn10.clone()
        } else {
            isbn13.clone()
        },
        isbn13,
        isbn10,
        pages: match media {
            Media::Book => pages(instance),
            Media::Audiobook => 0,
        },
        duration: match media {
            Media::Book => 0,
            Media::Audiobook => duration(instance),
        },
        genres: work.map(genres).unwrap_or_default(),
        tags: work.map(tags).unwrap_or_default(),
        series: series(instance),
        language_code: work.map(language).unwrap_or_default(),
    }
}

/// The record id in "https://libris.kb.se/<id>#it", or "" when it is not one.
pub fn record_id(instance: &Instance) -> String {
    let raw = instance.id.first_text();
    let path = raw.split('#').next().unwrap_or_default();
    let id = path.rsplit('/').next().unwrap_or_default();
    if is_record_id(id) {
        id.to_string()
    } else {
        String::new()
    }
}

pub fn is_record_id(id: &str) -> bool {
    (1..=MAX_ID_LENGTH).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

fn title_of(titles: &Many<Title>) -> (String, String) {
    let Some(title) = titles
        .iter()
        .find(|t| t.kind.has("Title"))
        .or_else(|| titles.first())
    else {
        return Default::default();
    };
    let mut main = title.main.first_text().to_string();
    if main.is_empty() {
        main = title_by_lang(title);
    }
    for part in title.parts.iter() {
        let words: Vec<&str> = [part.number.first_text(), part.name.first_text()]
            .into_iter()
            .filter(|word| !word.is_empty())
            .collect();
        if !words.is_empty() {
            main = format!("{main}. {}", words.join(", "));
        }
    }
    let subtitle = match title.subtitle.first_text() {
        "" => title.remainder.first_text(),
        subtitle => subtitle,
    };
    split_title(&main, subtitle)
}

/// A title in the script it was published in, ahead of a transliteration ("ja-Latn-t-ja").
fn title_by_lang(title: &Title) -> String {
    let Some(by_lang) = title.main_by_lang.first() else {
        return String::new();
    };
    by_lang
        .iter()
        .filter(|(lang, _)| !lang.contains("-t-"))
        .chain(by_lang.iter())
        .map(|(_, values)| values.first_text())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Separates what older records keep in one field: "Title : subtitle / statement of
/// responsibility".
pub fn split_title(main: &str, subtitle: &str) -> (String, String) {
    let main = main.split(" / ").next().unwrap_or_default();
    let (main, subtitle) = match (subtitle.trim().is_empty(), main.split_once(" : ")) {
        (true, Some((main, rest))) => (main, rest),
        _ => (main, subtitle),
    };
    let subtitle = clean_title(subtitle);
    let bracketed = subtitle.starts_with('[') && subtitle.ends_with(']');
    (
        clean_title(main),
        if bracketed { String::new() } else { subtitle },
    )
}

/// Drops the punctuation ISBD leaves at the end of a title (" /", " :", " ;").
pub fn clean_title(raw: &str) -> String {
    let title = raw.split(" / ").next().unwrap_or_default();
    let title = SPACES.replace_all(title.trim(), " ");
    title
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '/' | ':' | ';' | '=' | ','))
        .to_string()
}

/// People with the role, primary contributors first, as "Given Family".
fn people(contributions: &[&Contribution], role: &str) -> Vec<String> {
    let mut ordered: Vec<&Contribution> = contributions.to_vec();
    ordered.sort_by_key(|c| !c.kind.has("PrimaryContribution"));
    let names: Vec<String> = ordered
        .iter()
        .filter(|c| {
            c.role.iter().any(|r| r.is(role))
                // A main entry without a role is the author.
                || (role == AUTHOR && c.role.is_empty() && c.kind.has("PrimaryContribution"))
        })
        .flat_map(|c| c.agent.iter())
        .map(agent_name)
        .collect();
    unique(&names)
}

fn agent_name(agent: &Agent) -> String {
    let given = agent.given_name.first_text();
    let family = agent.family_name.first_text();
    let name = if !given.is_empty() || !family.is_empty() {
        format!("{given} {family}")
    } else if agent.kind.is_empty() || agent.kind.has("Person") {
        display_name(agent.name.first_text())
    } else {
        agent.name.first_text().to_string()
    };
    SPACES
        .replace_all(name.trim(), " ")
        .trim_end_matches([',', ';', ':'])
        .to_string()
}

/// "Lindgren, Astrid, 1907-2002" reads as "Astrid Lindgren".
pub fn display_name(raw: &str) -> String {
    let parts: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty() && !LIFE_DATES.is_match(part))
        .collect();
    match parts.as_slice() {
        [] => String::new(),
        [only] => only.to_string(),
        [family, given, rest @ ..] => [*given, *family]
            .into_iter()
            .chain(rest.iter().copied())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The edition's own publisher and year; other publication entries are reprints or originals.
fn publication(instance: &Instance) -> (String, String) {
    let mut entries: Vec<_> = instance.publication.iter().collect();
    entries.sort_by_key(|p| !p.kind.has("PrimaryPublication"));
    let publisher = entries
        .iter()
        .flat_map(|p| p.agent.iter())
        .map(|agent| match agent.label.first_text() {
            "" => agent.name.first_text(),
            label => label,
        })
        .map(|name| {
            name.trim_matches(|c: char| {
                c.is_whitespace() || matches!(c, '[' | ']' | ',' | ':' | ';')
            })
        })
        .find(|name| !name.is_empty())
        .unwrap_or_default()
        .to_string();
    let year = entries
        .iter()
        .flat_map(|p| p.year.iter().chain(p.date.iter()))
        .find_map(year_of)
        .unwrap_or_default();
    (publisher, year)
}

/// A four-digit year from "2020", "[2020]" or "c2015". Years before printing are another
/// calendar's ("1363" is a Persian year), not this edition's.
pub fn year_of(value: &serde_json::Value) -> Option<String> {
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        _ => return None,
    };
    YEAR.captures_iter(&text)
        .map(|captures| captures[1].to_string())
        .find(|year| {
            year.parse::<u32>()
                .is_ok_and(|y| (1450..=2100).contains(&y))
        })
}

fn description(instance: &Instance) -> String {
    let mut labels: Vec<&String> = instance
        .summary
        .iter()
        .flat_map(|s| s.label.iter())
        .collect();
    if labels.is_empty() {
        labels = instance
            .work
            .iter()
            .flat_map(|w| w.summary.iter())
            .flat_map(|s| s.label.iter())
            .collect();
    }
    let paragraphs: Vec<String> = labels.iter().map(|label| plain_text(label)).collect();
    unique(&paragraphs).join("\n\n")
}

pub fn plain_text(raw: &str) -> String {
    let text = raw.replace("\r\n", "\n");
    let text = HTML_TAG.replace_all(&text, "");
    let text = SOURCE_TAG.replace(text.trim(), "");
    BLANK_LINES.replace_all(text.trim(), "\n\n").to_string()
}

/// Digital editions link their cover from the record ("Omslagsbild"). Print editions' covers
/// are only served behind signed links that expire within days, so they are left out.
fn cover(instance: &Instance) -> String {
    instance
        .documents
        .iter()
        .filter(|d| {
            d.cataloguers_note.has("digipic")
                || d.public_note
                    .iter()
                    .any(|note| note.eq_ignore_ascii_case("omslagsbild"))
        })
        .flat_map(|d| d.uri.iter())
        .find(|uri| uri.starts_with("https://"))
        .cloned()
        .unwrap_or_default()
}

fn isbn_values(instance: &Instance) -> Vec<&str> {
    instance
        .identifiers
        .iter()
        .filter(|id| id.kind.has("ISBN"))
        .flat_map(|id| id.value.iter())
        .map(String::as_str)
        .collect()
}

pub fn lists_isbn(instance: &Instance, isbn: &str) -> bool {
    list_contains_isbn(&isbn_values(instance), isbn)
}

fn pages(instance: &Instance) -> u32 {
    instance
        .extent
        .iter()
        .flat_map(|e| e.label.iter())
        .map(|label| page_count(label))
        .find(|pages| *pages > 0)
        .unwrap_or(0)
}

/// "135 sidor", "126, [1] s.", "1 onlineresurs (175 sidor)". Unnumbered pages in brackets count
/// only when there is no numbered run.
pub fn page_count(label: &str) -> u32 {
    if !PAGE_UNIT.is_match(label) {
        return 0;
    }
    let numbers = |text: &str| -> Option<u32> {
        NUMBER
            .find_iter(text)
            .filter_map(|n| n.as_str().parse::<u32>().ok())
            .filter(|n| (1..=100_000).contains(n))
            .max()
    };
    let numbered = BRACKETED.replace_all(label, " ");
    numbers(&numbered)
        .or_else(|| {
            let unnumbered: Vec<&str> = BRACKETED.find_iter(label).map(|m| m.as_str()).collect();
            numbers(&unnumbered.join(" "))
        })
        .unwrap_or(0)
}

fn duration(instance: &Instance) -> u32 {
    let extents = instance.extent.iter().flat_map(|e| e.label.iter());
    let notes = instance
        .notes
        .iter()
        .flat_map(|n| n.label.iter())
        .filter(|note| note.to_lowercase().contains("speltid"));
    extents
        .chain(notes)
        .map(|label| duration_seconds(label))
        .find(|seconds| *seconds > 0)
        .unwrap_or(0)
}

/// "3 tim., 26 min.", "13283 sek.", "4 CD (4,5 tim.)", "05:43:12".
pub fn duration_seconds(label: &str) -> u32 {
    if let Some(c) = CLOCK.captures(label) {
        let part = |i: usize| c[i].parse::<u32>().unwrap_or(0);
        return part(1) * 3600 + part(2) * 60 + part(3);
    }
    let hours = HOURS
        .captures(label)
        .and_then(|c| c[1].replace(',', ".").parse::<f64>().ok())
        .unwrap_or(0.0);
    (hours * 3600.0).round() as u32
        + whole_number(&MINUTES, label) * 60
        + whole_number(&SECONDS, label)
}

fn whole_number(re: &Regex, label: &str) -> u32 {
    re.captures(label)
        .and_then(|c| c[1].parse::<u32>().ok())
        .unwrap_or(0)
}

fn genres(work: &Work) -> Vec<String> {
    let labels: Vec<String> = work
        .category
        .iter()
        .chain(work.genre_form.iter())
        .filter(|term| {
            term.id
                .iter()
                .any(|id| GENRE_SCHEMES.iter().any(|scheme| id.starts_with(scheme)))
        })
        .map(|term| term.label().to_string())
        .filter(|label| !FORMAT_GENRES.contains(&label.to_lowercase().as_str()))
        .collect();
    unique(&labels)
}

fn tags(work: &Work) -> Vec<String> {
    let labels: Vec<String> = work
        .subject
        .iter()
        .map(|term| term.label().to_string())
        .collect();
    unique(&labels)
}

fn series(instance: &Instance) -> Vec<SeriesEntry> {
    let mut out: Vec<SeriesEntry> = Vec::new();
    for membership in instance.series.iter() {
        let mut name = membership.statement.first_text().to_string();
        if name.is_empty() {
            name = membership
                .in_series
                .iter()
                .flat_map(|s| {
                    s.titles
                        .iter()
                        .chain(s.work.iter().flat_map(|w| w.titles.iter()))
                })
                .map(|t| t.main.first_text())
                .find(|title| !title.is_empty())
                .unwrap_or_default()
                .to_string();
        }
        let name = clean_title(&name);
        if name.is_empty() {
            continue;
        }
        let sequence = NUMBER
            .find(membership.enumeration.first_text())
            .map(|n| n.as_str().replace(',', "."))
            .unwrap_or_default();
        // Libris often lists the same series twice: as printed, and as a link to its record.
        match out
            .iter_mut()
            .find(|entry| entry.series.to_lowercase() == name.to_lowercase())
        {
            Some(entry) if entry.sequence.is_empty() => entry.sequence = sequence,
            Some(_) => {}
            None => out.push(SeriesEntry {
                series: name,
                sequence,
            }),
        }
    }
    out
}

/// A two-letter code: the term's own tag, its link to the ISO code, or the MARC code mapped.
fn language(work: &Work) -> String {
    let Some(term) = work.language.first() else {
        return String::new();
    };
    let tag = term.lang_tag.first_text();
    if tag.len() == 2 {
        return tag.to_lowercase();
    }
    if let Some(code) = term
        .same_as
        .iter()
        .flat_map(|link| link.id.iter())
        .filter_map(|id| id.split_once("/i18n/lang/").map(|(_, code)| code))
        .find(|code| code.len() == 2)
    {
        return code.to_lowercase();
    }
    let code = term.code.first_text().to_lowercase();
    match code.len() {
        2 => code,
        _ => marc_language(&code).unwrap_or_default().to_string(),
    }
}

fn marc_language(code: &str) -> Option<&'static str> {
    Some(match code {
        "swe" => "sv",
        "eng" => "en",
        "fin" => "fi",
        "nor" => "no",
        "nob" => "nb",
        "nno" => "nn",
        "dan" => "da",
        "ice" => "is",
        "fao" => "fo",
        "sme" => "se",
        "ger" => "de",
        "fre" => "fr",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "dut" => "nl",
        "rus" => "ru",
        "pol" => "pl",
        "ukr" => "uk",
        "bel" => "be",
        "cze" => "cs",
        "slo" => "sk",
        "slv" => "sl",
        "hrv" => "hr",
        "srp" => "sr",
        "bos" => "bs",
        "bul" => "bg",
        "mac" => "mk",
        "rum" => "ro",
        "hun" => "hu",
        "est" => "et",
        "lav" => "lv",
        "lit" => "lt",
        "gre" => "el",
        "alb" => "sq",
        "tur" => "tr",
        "ara" => "ar",
        "per" => "fa",
        "heb" => "he",
        "kur" => "ku",
        "som" => "so",
        "tir" => "ti",
        "amh" => "am",
        "arm" => "hy",
        "geo" => "ka",
        "hin" => "hi",
        "urd" => "ur",
        "ben" => "bn",
        "tha" => "th",
        "vie" => "vi",
        "chi" => "zh",
        "jpn" => "ja",
        "kor" => "ko",
        "ind" => "id",
        "may" => "ms",
        "tgl" => "tl",
        "swa" => "sw",
        "lat" => "la",
        "epo" => "eo",
        "cat" => "ca",
        "baq" => "eu",
        "glg" => "gl",
        "wel" => "cy",
        "gle" => "ga",
        "yid" => "yi",
        _ => return None,
    })
}

/// Free-text words for Libris, which reads `+ - | * " ( )` as operators: anything that is not a
/// letter or digit becomes a space, except an apostrophe or period inside a word ("O'Brien").
/// Lowercase, so a title word such as "OR" is not taken for one either.
pub fn search_terms(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let inner = i > 0
            && chars[i - 1].is_alphanumeric()
            && chars.get(i + 1).is_some_and(|next| next.is_alphanumeric());
        if c.is_alphanumeric() || (inner && matches!(c, '\'' | '\u{2019}' | '.')) {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    let mut terms = String::new();
    for word in out.split_whitespace() {
        if terms.chars().count() + word.chars().count() + 1 > QUERY_MAX_CHARS {
            break;
        }
        if !terms.is_empty() {
            terms.push(' ');
        }
        terms.push_str(word);
    }
    terms
}

/// The searches to try in turn: the title with the author's name, without the subtitle, then
/// without the author. Libris needs every word to match somewhere in a record, so a subtitle word
/// the catalogue lacks or a misspelt name finds nothing, or only records that are not the book.
pub fn search_attempts(title: &str, author: &str) -> Vec<String> {
    let main = main_title(title);
    let mut attempts = vec![
        search_terms(&format!("{title} {author}")),
        search_terms(&format!("{main} {author}")),
        search_terms(main),
    ];
    attempts.dedup();
    attempts.retain(|terms| !terms.is_empty());
    attempts
}

fn main_title(title: &str) -> &str {
    let cut = [":", "(", "[", " - "]
        .into_iter()
        .filter_map(|separator| title.find(separator))
        .filter(|index| *index > 0)
        .min();
    match cut {
        Some(index) => title[..index].trim(),
        None => title.trim(),
    }
}

/// Libris ranks by relevance over the whole record, so an edition with the author's name typed
/// into its title outranks the book itself. The asked-for title comes first, then the asked-for
/// author's records, otherwise Libris's order.
pub fn rank(matches: &mut [BookMetadata], title: &str, author: &str) {
    matches.sort_by_key(|book| {
        (
            !has_title(book, title),
            !author_matches(&book.authors, author),
        )
    });
}

/// True when the record's title, with or without its subtitle, is the asked-for title or its
/// main part.
pub fn has_title(book: &BookMetadata, title: &str) -> bool {
    let wanted = [normalize(title), normalize(main_title(title))];
    [
        normalize(&book.title),
        normalize(&format!("{} {}", book.title, book.subtitle)),
    ]
    .iter()
    .any(|own| wanted.contains(own))
}

fn normalize(text: &str) -> String {
    name_words(text).join(" ")
}

/// True when every word of the asked-for author is in one of the names, in any order.
pub fn author_matches(authors: &[String], author: &str) -> bool {
    let wanted = name_words(author);
    if wanted.is_empty() {
        return true;
    }
    authors.iter().any(|name| {
        let have: HashSet<String> = name_words(name).into_iter().collect();
        wanted.iter().all(|word| have.contains(word))
    })
}

fn name_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn unique(list: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    list.iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && seen.insert(value.to_lowercase()))
        .map(str::to_string)
        .collect()
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

/// The ISBN-10 of a 978 ISBN-13; a 979 one has none.
fn to_isbn10(isbn13: &str) -> String {
    let Some(body) = isbn13
        .strip_prefix("978")
        .filter(|rest| rest.len() == 10)
        .map(|rest| &rest[..9])
    else {
        return String::new();
    };
    let sum: u32 = body
        .chars()
        .enumerate()
        .map(|(i, c)| c.to_digit(10).unwrap_or(0) * (10 - i as u32))
        .sum();
    match (11 - sum % 11) % 11 {
        10 => format!("{body}X"),
        check => format!("{body}{check}"),
    }
}

/// Both forms of one ISBN, since older records list only the ISBN-10.
pub fn isbn_forms(isbn: &str) -> Vec<String> {
    let isbn13 = to_isbn13(isbn);
    let isbn10 = to_isbn10(&isbn13);
    [isbn13, isbn10]
        .into_iter()
        .filter(|form| !form.is_empty())
        .collect()
}

pub fn list_contains_isbn<S: AsRef<str>>(list: &[S], target: &str) -> bool {
    let want = to_isbn13(target);
    !want.is_empty()
        && list.iter().any(|candidate| {
            let isbn = normalize_isbn(candidate.as_ref());
            !isbn.is_empty() && to_isbn13(&isbn) == want
        })
}

/// Prefers the ISBN the client searched for. The ISBN-10 is the chosen ISBN-13's own rather than
/// one from the list: records list other bindings' numbers side by side, and a record read by id
/// often lists the ISBN-13 alone.
fn pick_isbns(list: &[&str], preferred: &str) -> (String, String) {
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
    let isbn10 = to_isbn10(&isbn13);
    (isbn13, isbn10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(json: &str) -> Instance {
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(rename = "mainEntity")]
            main_entity: Instance,
        }
        serde_json::from_str::<Body>(json)
            .expect("fixture parses")
            .main_entity
    }

    fn items(json: &str) -> Vec<Instance> {
        #[derive(serde::Deserialize)]
        struct Body {
            items: Vec<Instance>,
        }
        serde_json::from_str::<Body>(json)
            .expect("fixture parses")
            .items
    }

    fn item<'a>(list: &'a [Instance], id: &str) -> &'a Instance {
        list.iter()
            .find(|i| record_id(i) == id)
            .expect("fixture has the record")
    }

    const PRINT: &str = include_str!("../testdata/record-v61jx8fbsd06xtgv.json");
    const AUDIO: &str = include_str!("../testdata/record-7jf33fwr5rfk3b87.json");
    const BOOKS: &str = include_str!("../testdata/search-pippi-books.json");
    const AUDIOBOOKS: &str = include_str!("../testdata/search-pippi-audiobooks.json");
    const ISBN: &str = include_str!("../testdata/search-isbn-9789129657982.json");

    #[test]
    fn maps_a_printed_book() {
        let instance = record(PRINT);
        assert_eq!(media_of(&instance), Some(Media::Book));
        let b = map_instance(&instance, Media::Book, "");
        assert_eq!(
            (b.provider_id.as_str(), b.title.as_str(), b.media_type),
            ("v61jx8fbsd06xtgv", "Pippi Långstrump", "book")
        );
        assert_eq!(b.authors, ["Astrid Lindgren"]);
        assert!(b.narrators.is_empty());
        assert_eq!(
            (b.publisher.as_str(), b.published_year.as_str(), b.pages),
            ("Rabén & Sjögren", "2020", 135)
        );
        assert_eq!(
            (b.isbn13.as_str(), b.isbn10.as_str(), b.isbn.as_str()),
            ("9789129723632", "9129723639", "9789129723632")
        );
        assert_eq!(b.language_code, "sv");
        assert_eq!(
            b.genres,
            [
                "Skönlitteratur",
                "Barn- och ungdomslitteratur",
                "Kapitelböcker"
            ]
        );
        assert!(b.tags.contains(&"Busig".to_string()), "{:?}", b.tags);
        assert!(b
            .description
            .starts_with("Pippi Långstrump är den allra första boken"));
        assert!(b.cover.is_empty());
        assert_eq!(b.url, "https://libris.kb.se/v61jx8fbsd06xtgv");
        assert_eq!(b.duration, 0);
    }

    #[test]
    fn maps_an_audiobook() {
        let instance = record(AUDIO);
        assert_eq!(media_of(&instance), Some(Media::Audiobook));
        let b = map_instance(&instance, Media::Audiobook, "");
        assert_eq!((b.media_type, b.duration, b.pages), ("audiobook", 13283, 0));
        assert_eq!(
            (b.authors.as_slice(), b.narrators.as_slice()),
            (
                ["Astrid Lindgren".to_string()].as_slice(),
                ["Astrid Lindgren".to_string()].as_slice()
            )
        );
        assert_eq!(
            b.cover,
            "https://bookbites-prod-se.covers.bookbites.com/9789198119237"
        );
        assert_eq!(b.genres, ["Barn- och ungdomslitteratur"]);
        assert!(b.description.ends_with("tillsammans med Tommy och Annika."));
        assert_eq!(
            (b.isbn13.as_str(), b.isbn10.as_str()),
            ("9789198119237", "9198119230")
        );
        assert_eq!(
            (b.publisher.as_str(), b.published_year.as_str()),
            ("Astrid Lindgren AB", "2019")
        );
    }

    #[test]
    fn tells_books_from_audiobooks() {
        for instance in items(BOOKS) {
            assert_eq!(media_of(&instance), Some(Media::Book), "{:?}", instance.id);
        }
        for instance in items(AUDIOBOOKS) {
            assert_eq!(
                media_of(&instance),
                Some(Media::Audiobook),
                "{:?}",
                instance.id
            );
        }
        let chapter: Instance = serde_json::from_str(
            r#"{"category": {"@id": "https://id.kb.se/term/saobf/ComponentPart"},
                "instanceOf": {"category": [{"@id": "https://id.kb.se/term/rda/Text"}]}}"#,
        )
        .unwrap();
        let radio: Instance = serde_json::from_str(
            r#"{"instanceOf": {"category": [{"@id": "https://id.kb.se/term/saogf/Radioprogram"},
                {"@id": "https://id.kb.se/term/rda/SpokenWord"}]}}"#,
        )
        .unwrap();
        assert_eq!((media_of(&chapter), media_of(&radio)), (None, None));
    }

    #[test]
    fn reads_audiobook_durations_and_narrators() {
        let list = items(AUDIOBOOKS);
        let daisy = map_instance(item(&list, "q8247v8259bv1sj"), Media::Audiobook, "");
        assert_eq!(daisy.duration, 2 * 3600 + 34 * 60);
        assert_eq!(daisy.authors, ["Astrid Lindgren"]);
        assert_eq!(daisy.narrators, ["Inga Håkansson"]);
        let cassette = map_instance(item(&list, "sb44vjj44rb8r23"), Media::Audiobook, "");
        assert_eq!(cassette.duration, 4 * 3600 + 15 * 60);
        assert_eq!(cassette.narrators, ["Astrid Lindgren"]);
    }

    #[test]
    fn cleans_titles_and_reads_series() {
        let list = items(BOOKS);
        let typed_in = map_instance(item(&list, "wf7kt9071bvj902"), Media::Book, "");
        assert_eq!(typed_in.title, "Pippi Långstrump i Söderhavet");
        let isbn_hits = items(ISBN);
        let reprint = map_instance(item(&isbn_hits, "gzrq1kqs0c18tgv"), Media::Book, "");
        assert_eq!(
            reprint.series,
            [SeriesEntry {
                series: "Astrid Lindgrens samlingsbibliotek".into(),
                sequence: String::new()
            }]
        );
        assert_eq!(reprint.pages, 248);
        assert_eq!(reprint.authors, ["Astrid Lindgren"]);
        let cases = [
            ("Pippi Långstrump /", "", ("Pippi Långstrump", "")),
            (
                "Pippi Långstrump : måla och pyssla",
                "",
                ("Pippi Långstrump", "måla och pyssla"),
            ),
            ("Emil i Lönneberga ;", "", ("Emil i Lönneberga", "")),
            (
                "Pippi Långstrump på Kurrekurreduttön",
                "[bilderbok]",
                ("Pippi Långstrump på Kurrekurreduttön", ""),
            ),
            (
                "Bröderna Lejonhjärta",
                "pjäs i två akter",
                ("Bröderna Lejonhjärta", "pjäs i två akter"),
            ),
            ("Vad är det? / text: Ann Ek", "", ("Vad är det?", "")),
        ];
        for (main, subtitle, (title, sub)) in cases {
            assert_eq!(
                split_title(main, subtitle),
                (title.to_string(), sub.to_string()),
                "{main}"
            );
        }
    }

    #[test]
    fn reads_titles_in_their_own_script() {
        let instance: Instance = serde_json::from_str(
            r#"{"@id": "https://libris.kb.se/wj17vbljzj29vjr#it",
                "hasTitle": [{"@type": "Title", "mainTitleByLang":
                  {"el": "αδελφοί λεοντόκαρδοι", "el-Latn-t-el": "Adelfoi leontokardoi"}}],
                "instanceOf": {"language": {"code": "gre"}}}"#,
        )
        .unwrap();
        let b = map_instance(&instance, Media::Book, "");
        assert_eq!(
            (b.title.as_str(), b.language_code.as_str()),
            ("αδελφοί λεοντόκαρδοι", "el")
        );
    }

    #[test]
    fn writes_names_the_way_books_do() {
        for (raw, want) in [
            ("Lindgren, Astrid, 1907-2002", "Astrid Lindgren"),
            ("Lagerlöf, Selma, 1858-1940.", "Selma Lagerlöf"),
            ("Ekman, Kerstin, f. 1933", "Kerstin Ekman"),
            ("King, Martin Luther, Jr.", "Martin Luther King Jr."),
            ("Rabén & Sjögren", "Rabén & Sjögren"),
        ] {
            assert_eq!(display_name(raw), want, "{raw}");
        }
        let contribution: Contribution = serde_json::from_str(
            r#"{"@type": "PrimaryContribution", "agent": {"@type": "Person",
                "name": "Lindgren, Astrid, 1907-2002"}}"#,
        )
        .unwrap();
        assert_eq!(people(&[&contribution], AUTHOR), ["Astrid Lindgren"]);
    }

    #[test]
    fn reads_page_counts_and_durations() {
        for (label, want) in [
            ("135 sidor", 135),
            ("126, [1] s.", 126),
            ("1 onlineresurs (175 sidor)", 175),
            ("[32] s.", 32),
            ("176s", 176),
            ("xii, 340 s.", 340),
            ("2 tim., 34 min.", 0),
            ("1 CD-R", 0),
        ] {
            assert_eq!(page_count(label), want, "{label}");
        }
        for (label, want) in [
            ("3 tim., 26 min.", 3 * 3600 + 26 * 60),
            ("13283 sek.", 13283),
            ("3 timmar, 24 minuter", 3 * 3600 + 24 * 60),
            ("2 CD (ca 3 tim.)", 3 * 3600),
            ("4 CD (4,5 tim.)", 16200),
            ("1 ljudfil (05:43:12)", 5 * 3600 + 43 * 60 + 12),
            ("Speltid: 59 min.", 59 * 60),
            ("135 sidor", 0),
        ] {
            assert_eq!(duration_seconds(label), want, "{label}");
        }
    }

    #[test]
    fn reads_years_of_this_calendar() {
        use serde_json::json;
        assert_eq!(year_of(&json!("[2020]")), Some("2020".into()));
        assert_eq!(year_of(&json!("c2015")), Some("2015".into()));
        assert_eq!(year_of(&json!(1973)), Some("1973".into()));
        assert_eq!(year_of(&json!("1363")), None);
    }

    #[test]
    fn strips_the_source_tag_from_summaries() {
        assert_eq!(
            plain_text("Första stycket.\r\n\r\n\r\nAndra <b>stycket</b>.[Bokinfo]"),
            "Första stycket.\n\nAndra stycket."
        );
    }

    #[test]
    fn builds_safe_search_terms() {
        assert_eq!(
            search_terms("Pippi Långstrump: går ombord"),
            "pippi långstrump går ombord"
        );
        assert_eq!(
            search_terms("-Hamlet OR (Macbeth) | \"x*\""),
            "hamlet or macbeth x"
        );
        assert_eq!(search_terms("O'Brien's Jean-Paul"), "o'brien's jean paul");
        assert_eq!(
            search_attempts("Pippi Långstrump: går ombord", "Astrid Lindgren"),
            [
                "pippi långstrump går ombord astrid lindgren",
                "pippi långstrump astrid lindgren",
                "pippi långstrump"
            ]
        );
        assert_eq!(search_attempts("Mio, min Mio", ""), ["mio min mio"]);
    }

    #[test]
    fn ranks_the_asked_for_title_first() {
        let book = |title: &str, author: &str| BookMetadata {
            title: title.into(),
            authors: vec![author.into()],
            ..BookMetadata::default()
        };
        let mut matches = vec![
            book("Pippi Långstrump i Söderhavet", "Astrid Lindgren"),
            book("Pippi Långstrump", "Ulla Lundqvist"),
            book("Pippi Långstrump", "Astrid Lindgren"),
        ];
        rank(&mut matches, "Pippi Långstrump", "Lindgren");
        let order: Vec<(&str, &str)> = matches
            .iter()
            .map(|b| (b.title.as_str(), b.authors[0].as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("Pippi Långstrump", "Astrid Lindgren"),
                ("Pippi Långstrump", "Ulla Lundqvist"),
                ("Pippi Långstrump i Söderhavet", "Astrid Lindgren")
            ]
        );
    }

    #[test]
    fn isbn_handling() {
        for (input, want) in [
            ("978-91-29-65798-2", "9789129657982"),
            ("91-29-65798-9", "9129657989"),
            ("978-91-29-65798-3", ""),
            ("ASIN:B08G9PRS1K", ""),
        ] {
            assert_eq!(normalize_isbn(input), want, "{input}");
        }
        assert_eq!(isbn_forms("9129657989"), ["9789129657982", "9129657989"]);
        assert_eq!(isbn_forms("9791234567896"), ["9791234567896"]);
        assert_eq!(to_isbn10("9789189035027"), "918903502X");
        assert!(list_contains_isbn(&["9129657989"], "9789129657982"));
        assert_eq!(
            pick_isbns(&["9129657989", "9789129657982", "9789129711059"], ""),
            ("9789129657982".to_string(), "9129657989".to_string())
        );
        assert_eq!(
            pick_isbns(
                &["9789129711028", "9789129711059", "9129711053"],
                "9789129711059"
            ),
            ("9789129711059".to_string(), "9129711053".to_string())
        );
    }

    #[test]
    fn maps_language_codes() {
        let work: Work = serde_json::from_str(
            r#"{"language": [{"code": "per", "sameAs": [{"@id": "https://id.kb.se/i18n/lang/fa"}]}]}"#,
        )
        .unwrap();
        assert_eq!(language(&work), "fa");
        let work: Work = serde_json::from_str(r#"{"language": {"code": "ger"}}"#).unwrap();
        assert_eq!(language(&work), "de");
        let work: Work = serde_json::from_str(r#"{"language": {"code": "mul"}}"#).unwrap();
        assert_eq!(language(&work), "");
    }
}
