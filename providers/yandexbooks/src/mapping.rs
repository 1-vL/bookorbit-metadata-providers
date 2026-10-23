use std::cmp::Reverse;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::page::answers;
use crate::yandex::{Kind, Title};

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub narrators: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pages: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration: i64,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_date: String,
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

/// Store shelves rather than genres: they say where the title is sold, not what it is.
const STORE_TOPICS: [&str; 3] = ["audio-ru", "besplatno-ru", "kniga-kino"];
/// Yandex dates a release at midnight in Moscow, three hours ahead of UTC all year.
const MOSCOW_OFFSET_SECONDS: i64 = 3 * 3600;

/// One title on a search page, before its own page is read.
#[derive(Debug, Clone)]
pub struct Hit {
    pub title: Title,
    pub name: String,
    pub authors: Vec<String>,
}

/// The books, audiobooks and comics on a search page, in the site's order. Authors, readers'
/// shelves, series and serials are left out.
pub fn search_hits(html: &str) -> Vec<Hit> {
    let Some(page) = answers(html)
        .into_iter()
        .find(|(key, value)| key == "search" && value.get("page").is_some())
        .and_then(|(_, value)| value.get("page").and_then(Value::as_array).cloned())
    else {
        return Vec::new();
    };
    page.iter()
        .filter_map(|item| {
            let kind = Kind::from_search_type(item.get("__typename")?.as_str()?)?;
            let book = item.get("book")?;
            let uuid = text(book, "initUuid").or_else(|| text(book, "uuid"))?;
            crate::yandex::valid_uuid(&uuid).then(|| Hit {
                title: Title { kind, uuid },
                name: text(book, "name").unwrap_or_default(),
                authors: names(book.get("authors")),
            })
        })
        .collect()
}

/// The hits in the order to fetch them: by how much of the author's name they share, then the
/// medium asked for (BookOrbit asks for `book` for comics too), then an exact title, then the
/// site's order. Counting shared words keeps "Сергей Недоруб" below "Сергей Лукьяненко", and the
/// author comes first because the right book in another medium beats a stranger's: Yandex has
/// Акунин's "Азазель" only as an audiobook, and another "Азазель" as an ebook.
pub fn rank(hits: Vec<Hit>, query: &str, author: &str, media_type: &str) -> Vec<Hit> {
    let wanted_audio = media_type == "audiobook";
    let author_words = words(author);
    let query = fold(query);
    let mut ranked: Vec<(usize, Hit)> = hits.into_iter().enumerate().collect();
    ranked.sort_by_key(|(index, hit)| {
        let audio = hit.title.kind == Kind::Audiobook;
        let other_medium = audio != wanted_audio;
        let shared = words(&hit.authors.join(" "))
            .intersection(&author_words)
            .count();
        let other_title = fold(&hit.name) != query;
        (Reverse(shared), other_medium, other_title, *index)
    });
    ranked.into_iter().map(|(_, hit)| hit).collect()
}

/// A title's own page as an answer. `None` when the page carries no record of it.
pub fn map_title(html: &str, title: &Title) -> Option<BookMetadata> {
    let answers = answers(html);
    let (record_key, field) = match title.kind {
        Kind::Book => ("oldBook", "book"),
        Kind::Audiobook => ("oldAudioBook", "audiobook"),
        Kind::Comic => ("oldComicBook", "comicbook"),
    };
    let record = answers
        .iter()
        .filter(|(key, _)| key == record_key)
        .filter_map(|(_, value)| value.get(field))
        .find(|record| record.is_object())?;
    let name = text(record, "title")?;

    let authors = match title.kind {
        // The book record's `authors` is one joined string; its objects list them one by one.
        Kind::Book => names(record.get("authors_objects")),
        _ => names(record.get("authors")),
    };
    let (published_date, published_year) = record
        .get("publication_date")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(moscow_date)
        .map(|(y, m, d)| (format!("{y:04}-{m:02}-{d:02}"), y.to_string()))
        .unwrap_or_default();
    let series = series(record, &answers, title, &authors);

    Some(BookMetadata {
        media_type: title.kind.media_type().to_string(),
        provider_id: title.provider_id(),
        title: name,
        authors,
        narrators: names(record.get("narrators")),
        publisher: names(record.get("publishers"))
            .into_iter()
            .next()
            .unwrap_or_default(),
        published_year,
        published_date,
        // Read raw: `text` would fold the paragraph breaks away.
        description: ["annotation", "editor_annotation"]
            .iter()
            .filter_map(|key| record.get(*key).and_then(Value::as_str))
            .map(clean_text)
            .find(|text| !text.is_empty())
            .unwrap_or_default(),
        cover: record
            .get("cover")
            .and_then(|cover| text(cover, "large"))
            .filter(|url| url.starts_with("https://"))
            .unwrap_or_default(),
        genres: genres(record.get("topics")),
        series,
        language_code: text(record, "language").unwrap_or_default(),
        pages: ["paper_pages", "pages_count"]
            .iter()
            .find_map(|key| record.get(*key).and_then(Value::as_i64))
            .filter(|pages| *pages > 0)
            .unwrap_or_default(),
        duration: record
            .get("duration")
            .and_then(Value::as_i64)
            .filter(|seconds| *seconds > 0)
            .unwrap_or_default(),
        url: title.url(),
    })
}

/// The first series the title belongs to, numbered when its page shows where it falls. A
/// publisher's collection, such as "РЕШ: Белая книга", is a series on Yandex too; it is told
/// apart by its other parts being mostly by other authors, and left out.
fn series(
    record: &Value,
    answers: &[(String, Value)],
    title: &Title,
    authors: &[String],
) -> Vec<Series> {
    let Some(name) = record
        .get("series_list")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find_map(|series| text(series, "title")))
    else {
        return Vec::new();
    };
    let parts: Vec<&Value> = answers
        .iter()
        .filter_map(|(_, value)| {
            value
                .pointer("/book/relations/inSeries/page")
                .and_then(Value::as_array)
        })
        .flatten()
        .collect();

    let own: BTreeSet<String> = authors.iter().map(|name| fold(name)).collect();
    let by_the_same_author = parts
        .iter()
        .filter(|part| {
            names(part.pointer("/item/book/authors"))
                .iter()
                .any(|name| own.contains(&fold(name)))
        })
        .count();
    if by_the_same_author * 2 < parts.len() {
        return Vec::new();
    }

    let siblings: Vec<(String, i64)> = parts
        .iter()
        .filter_map(|part| {
            let uuid = part
                .pointer("/item/book/initUuid")
                .and_then(Value::as_str)?;
            Some((uuid.to_string(), part.get("position")?.as_i64()?))
        })
        .collect();
    let sequence = own_position(&title.uuid, &siblings)
        .map(|n| n.to_string())
        .unwrap_or_default();
    vec![Series {
        series: name,
        sequence,
    }]
}

/// The page lists the series' other parts with their numbers, but not the title's own. It is the
/// one number missing from the run, or the next one when none is missing; with two or more
/// missing it cannot be told, so it is left out.
fn own_position(uuid: &str, siblings: &[(String, i64)]) -> Option<i64> {
    if let Some((_, position)) = siblings.iter().find(|(other, _)| other == uuid) {
        return Some(*position);
    }
    let taken: BTreeSet<i64> = siblings
        .iter()
        .map(|(_, position)| *position)
        .filter(|n| *n > 0)
        .collect();
    let last = *taken.last()?;
    let missing: Vec<i64> = (1..=last).filter(|n| !taken.contains(n)).collect();
    match missing.as_slice() {
        [] => Some(last + 1),
        [only] => Some(*only),
        _ => None,
    }
}

fn genres(topics: Option<&Value>) -> Vec<String> {
    let mut seen = Vec::new();
    for topic in topics.and_then(Value::as_array).into_iter().flatten() {
        if text(topic, "slug").is_some_and(|slug| STORE_TOPICS.contains(&slug.as_str())) {
            continue;
        }
        if let Some(name) = text(topic, "title") {
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
    seen
}

/// The names in a list of people or publishers, each once, in order.
fn names(list: Option<&Value>) -> Vec<String> {
    let mut seen = Vec::new();
    for person in list.and_then(Value::as_array).into_iter().flatten() {
        if person.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(name) = text(person, "name") {
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
    }
    seen
}

/// A trimmed, non-empty string field.
fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
}

/// Plain text, paragraphs kept: annotations are text, but some carry `<br>` or `<p>` from the
/// publisher's feed.
fn clean_text(raw: &str) -> String {
    let mut plain = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        plain.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let is_tag = after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/');
        match after.find('>').filter(|_| is_tag) {
            Some(end) => {
                let tag = after[..end].trim_start_matches('/').to_ascii_lowercase();
                if tag.starts_with("br") || tag.starts_with('p') || tag.starts_with("div") {
                    plain.push('\n');
                }
                rest = &after[end + 1..];
            }
            None => {
                plain.push('<');
                rest = after;
            }
        }
    }
    plain.push_str(rest);

    let mut paragraphs: Vec<String> = Vec::new();
    for line in plain.replace("\r\n", "\n").split('\n') {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            paragraphs.push(line);
        }
    }
    paragraphs.join("\n\n")
}

/// Lower case, `ё` read as `е`, punctuation as spaces: how a reader would compare two names.
fn fold(value: &str) -> String {
    value
        .to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn words(value: &str) -> BTreeSet<String> {
    fold(value)
        .split(' ')
        .filter(|word| word.chars().count() > 1)
        .map(str::to_string)
        .collect()
}

/// The calendar date in Moscow of a Unix time.
fn moscow_date(seconds: i64) -> (i64, u32, u32) {
    civil_from_days((seconds + MOSCOW_OFFSET_SECONDS).div_euclid(86_400))
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_MASTER: &str = include_str!("../testdata/search-master-i-margarita.html");
    const SEARCH_DOZOR: &str = include_str!("../testdata/search-nochnoy-dozor.html");
    const BOOK_MASTER: &str = include_str!("../testdata/book-MWxAop5b.html");
    const BOOK_DOZOR: &str = include_str!("../testdata/book-FVvJyn5e.html");
    const AUDIO_MASTER: &str = include_str!("../testdata/audiobook-VTVT8GzC.html");
    const AUDIO_DOZOR: &str = include_str!("../testdata/audiobook-yolWzcli.html");
    const COMIC_MASTER: &str = include_str!("../testdata/comicbook-yjrd9dHo.html");
    const BOOK_LAVR: &str = include_str!("../testdata/book-MJwmY8DF.html");

    fn title(id: &str) -> Title {
        Title::parse(id).unwrap()
    }

    fn hit(id: &str, name: &str, author: &str) -> Hit {
        Hit {
            title: title(id),
            name: name.into(),
            authors: vec![author.into()],
        }
    }

    fn ids(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|hit| hit.title.provider_id()).collect()
    }

    #[test]
    fn search_page_lists_books_audiobooks_and_comics_only() {
        let hits = search_hits(SEARCH_MASTER);
        // 7 books, 10 audiobooks and a comic; the six readers' shelves are skipped.
        assert_eq!(hits.len(), 18, "{:?}", ids(&hits));
        assert_eq!(hits[0].title.provider_id(), "book-MWxAop5b");
        assert_eq!(hits[0].name, "Мастер и Маргарита");
        assert_eq!(hits[0].authors, ["Михаил Булгаков"]);
        assert_eq!(hits[2].title.provider_id(), "audiobook-VTVT8GzC");
        assert!(ids(&hits).contains(&"comicbook-yjrd9dHo".to_string()));
        assert!(search_hits(include_str!("../testdata/search-empty.html")).is_empty());
        assert!(search_hits("<html></html>").is_empty());
    }

    #[test]
    fn rank_puts_the_medium_asked_for_first() {
        let books = rank(
            search_hits(SEARCH_MASTER),
            "Мастер и Маргарита",
            "Михаил Булгаков",
            "book",
        );
        assert_eq!(
            ids(&books[..3]),
            ["book-MWxAop5b", "book-TKUkSwtS", "book-ds3Ah2rB"]
        );
        let audio = rank(
            search_hits(SEARCH_MASTER),
            "Мастер и Маргарита",
            "Михаил Булгаков",
            "audiobook",
        );
        assert!(audio[..10]
            .iter()
            .all(|hit| hit.title.kind == Kind::Audiobook));
        assert_eq!(audio[0].title.provider_id(), "audiobook-VTVT8GzC");
    }

    #[test]
    fn rank_prefers_the_author_then_the_exact_title() {
        // The author's other books follow in the site's order, then other authors' books, with
        // a namesake ahead of strangers.
        let ranked = rank(
            search_hits(SEARCH_DOZOR),
            "Ночной дозор",
            "Лукьяненко Сергей",
            "book",
        );
        assert_eq!(ranked[0].title.provider_id(), "book-FVvJyn5e");
        assert_eq!(ranked[1].title.provider_id(), "book-OW4cj7ML");
        let books: Vec<&Hit> = ranked
            .iter()
            .filter(|hit| hit.title.kind == Kind::Book)
            .collect();
        let first_other = books
            .iter()
            .position(|hit| !hit.authors.iter().any(|a| a.contains("Лукьяненко")))
            .unwrap();
        assert!(books[first_other..]
            .iter()
            .all(|hit| !hit.authors.iter().any(|a| a.contains("Лукьяненко"))));
        assert_eq!(books[first_other].authors, ["Сергей Недоруб"]);
        assert_eq!(books.len(), 13);
    }

    #[test]
    fn rank_puts_the_author_before_the_medium() {
        // The site's order for "Азазель", which has Акунин's novel only as an audiobook.
        let hits = vec![
            hit("audiobook-aaaaaaaa", "Азазель", "Борис Акунин"),
            hit("book-bbbbbbbb", "Азазель", "Юсуф Зейдан"),
            hit("book-cccccccc", "Просьба Азазеля", "Гай Юлий Орловский"),
        ];
        let ranked = rank(hits, "Азазель", "Борис Акунин", "book");
        assert_eq!(
            ids(&ranked),
            ["audiobook-aaaaaaaa", "book-bbbbbbbb", "book-cccccccc"]
        );
    }

    #[test]
    fn book_page_maps_every_field() {
        let book = map_title(BOOK_MASTER, &title("book-MWxAop5b")).unwrap();
        assert_eq!(book.media_type, "book");
        assert_eq!(book.provider_id, "book-MWxAop5b");
        assert_eq!(book.title, "Мастер и Маргарита");
        assert_eq!(book.authors, ["Михаил Булгаков"]);
        assert_eq!(book.publisher, "Мультимедийное издательство Стрельбицкого");
        assert_eq!(book.language_code, "ru");
        assert_eq!(book.pages, 249);
        assert!(book.description.starts_with("«Мастер и Маргарита»"));
        assert_eq!(
            book.genres,
            [
                "Русская литература",
                "Проза XX века",
                "Сверхъестественное",
                "Художественная литература"
            ]
        );
        assert!(book
            .cover
            .starts_with("https://api.bookmate.ru/assets/books-covers/0a/d0/MWxAop5b-ipad.jpeg"));
        assert_eq!(book.url, "https://books.yandex.ru/books/MWxAop5b");
        // This edition states no publication date, and the series list is empty.
        assert_eq!((book.published_year.as_str(), book.series.len()), ("", 0));
    }

    #[test]
    fn series_number_is_the_one_missing_from_the_siblings() {
        let book = map_title(BOOK_DOZOR, &title("book-FVvJyn5e")).unwrap();
        assert_eq!(
            book.series,
            [Series {
                series: "Дозоры".into(),
                sequence: "1".into()
            }]
        );
        assert_eq!(
            (book.published_date.as_str(), book.published_year.as_str()),
            ("2022-12-14", "2022")
        );
        assert_eq!(book.publisher, "Neoclassic");
    }

    #[test]
    fn a_publishers_collection_is_not_a_series() {
        // "РЕШ: Белая книга" lists twenty books, one of them by Водолазкин.
        let book = map_title(BOOK_LAVR, &title("book-MJwmY8DF")).unwrap();
        assert_eq!(book.title, "Лавр");
        assert!(book.series.is_empty(), "{:?}", book.series);
        assert_eq!(
            (book.publisher.as_str(), book.published_year.as_str()),
            ("Редакция Елены Шубиной", "2024")
        );
    }

    #[test]
    fn descriptions_keep_their_paragraphs() {
        let book = map_title(BOOK_DOZOR, &title("book-FVvJyn5e")).unwrap();
        assert!(book
            .description
            .starts_with("Пять лет назад Антон Городецкий узнал"));
        assert!(book.description.contains("\n\n"), "{}", book.description);
        assert!(!book.description.contains("\n\n\n"));
    }

    #[test]
    fn audiobook_page_has_narrators_and_duration() {
        let audio = map_title(AUDIO_MASTER, &title("audiobook-VTVT8GzC")).unwrap();
        assert_eq!(audio.media_type, "audiobook");
        assert_eq!(audio.authors, ["Михаил Булгаков"]);
        assert_eq!(audio.narrators.len(), 6, "{:?}", audio.narrators);
        assert_eq!(audio.narrators[0], "Максим Суханов");
        assert_eq!(audio.duration, 64914);
        assert_eq!(audio.published_date, "2022-11-22");
        assert_eq!(audio.publisher, "ВИМБО");
        assert!(!audio
            .genres
            .iter()
            .any(|g| g == "Аудио" || g == "Бесплатно"));
        assert!(audio.cover.contains("audiobooks-covers"));

        let dozor = map_title(AUDIO_DOZOR, &title("audiobook-yolWzcli")).unwrap();
        assert_eq!(dozor.narrators, ["Александр Кокшаров"]);
        assert_eq!(
            dozor.series,
            [Series {
                series: "Дозоры".into(),
                sequence: "1".into()
            }]
        );
    }

    #[test]
    fn comic_page_is_a_comic() {
        let comic = map_title(COMIC_MASTER, &title("comicbook-yjrd9dHo")).unwrap();
        assert_eq!(
            (comic.media_type.as_str(), comic.title.as_str(), comic.pages),
            ("comic", "Мастер и Маргарита. Графический роман", 146)
        );
        assert_eq!(comic.publisher, "АСТ нонфикшн");
        assert_eq!(comic.url, "https://books.yandex.ru/comicbooks/yjrd9dHo");
    }

    #[test]
    fn a_page_without_the_record_maps_to_nothing() {
        assert!(map_title("<html></html>", &title("book-MWxAop5b")).is_none());
        // An audiobook id read from a book page finds no audiobook record.
        assert!(map_title(BOOK_MASTER, &title("audiobook-MWxAop5b")).is_none());
    }

    #[test]
    fn own_position_fills_the_single_gap() {
        let at = |positions: &[i64]| {
            let siblings: Vec<(String, i64)> =
                positions.iter().map(|n| (format!("x{n}"), *n)).collect();
            own_position("me", &siblings)
        };
        assert_eq!(at(&[2, 3, 4]), Some(1));
        assert_eq!(at(&[1, 2, 4]), Some(3));
        assert_eq!(at(&[1, 2, 3]), Some(4));
        assert_eq!(at(&[2, 4]), None);
        assert_eq!(at(&[]), None);
        assert_eq!(
            own_position("me", &[("me".into(), 7), ("x1".into(), 1)]),
            Some(7)
        );
    }

    #[test]
    fn clean_text_keeps_paragraphs_and_drops_tags() {
        assert_eq!(
            clean_text("Первый  абзац.<br/>Второй <b>абзац</b>.\r\n\r\n\r\nТретий 1 < 2"),
            "Первый абзац.\n\nВторой абзац.\n\nТретий 1 < 2"
        );
    }

    #[test]
    fn dates_are_moscow_days() {
        assert_eq!(moscow_date(1_669_064_400), (2022, 11, 22));
        assert_eq!(moscow_date(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
