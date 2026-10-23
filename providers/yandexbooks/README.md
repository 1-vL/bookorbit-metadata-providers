# Yandex Books metadata provider

BookOrbit metadata from [Yandex Books](https://books.yandex.ru) (Яндекс Книги, formerly Bookmate),
a large Russian-language store of ebooks, audiobooks and comics. It runs beside BookOrbit as a small
container.

Rust: axum, tokio, reqwest with rustls, serde. The image is a static musl binary on `scratch`,
about 2.2 MB. Yandex Books pages carry the site's own data as JSON, so the provider needs no HTML
parser.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  yandexbooks:
    image: ghcr.io/bookorbit/yandexbooks-provider:0.1.0
    restart: unless-stopped
```

Then add `http://yandexbooks:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. No account or key is needed. To build it yourself:
`docker build -t yandexbooks-provider .`, or with a Rust toolchain, `cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `YANDEX_BOOKS_SITE` | `https://books.yandex.ru` | Where pages are fetched from; links in answers always point at books.yandex.ru |
| `USER_AGENT` | `bookorbit-yandexbooks-provider/0.1.0` | |
| `DEADLINE_MS` | `8000` | How long a search or lookup may take. A search that runs out answers with the books it has, marked partial. Raise it if you raise the provider's timeout in BookOrbit (10 seconds by default) |

Yandex takes two to three seconds to render each page. The provider keeps at most two requests in
flight and starts them at least a second apart, so it never sends more than one a second, whatever
its clients ask for. When Yandex answers `429` or shows its captcha, the provider sends it nothing
more for the `Retry-After` it gave, or five minutes after a captcha, and answers `429` itself
meanwhile. It never tries to solve a captcha.

The provider reads only pages that the site's robots.txt allows: search results under
`/search/all/`, and title pages under `/books/`, `/audiobooks/` and `/comicbooks/`. It never calls
the site's API, which robots.txt disallows; the pages carry the same data.

What it sends to books.yandex.ru: the title of each book BookOrbit asks about, and the ids it looks
up. The author is used only to rank the results and is never sent. It logs request paths and
failures, never titles.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `yandexbooks`, books, audiobooks and comics, lookup, one call at a time 500 ms apart, icon and colour |
| `GET /search?query=&author=&mediaType=&limit=` | `limit` defaults to 3, at most 3 |
| `GET /book/{id}` | An id such as `book-MWxAop5b`, `audiobook-VTVT8GzC` or `comicbook-yjrd9dHo`. A link to the title's page on books.yandex.ru, URL-encoded, works too |

A search reads the results page, then each match's own page, so one search is up to four requests
and takes four to seven seconds.

Status codes: `400` no query, `401` bad token, `404` unknown id, `429` with `Retry-After` when
Yandex throttles or asks for a captcha, `502` when it fails or a page carries no record. A search
where some title pages could not be fetched or read still answers `200` with `"partial": true`; one
where none could answers `502`, never an empty list.

## Mapping

- **Search** sends the title alone and keeps books, audiobooks and comics; authors, readers'
  shelves and serials in the results are skipped. Matches are ranked by how much of the author's
  name they share, then by the medium asked for, then an exact title, then the site's order. A
  search with only an ISBN answers empty without asking the site.
- **Ids** name the kind of title and the site's own id, because books, audiobooks and comics are
  numbered separately: `book-MWxAop5b`, `audiobook-VTVT8GzC`, `comicbook-yjrd9dHo`. Each answer
  carries its page as `url`.
- **Title, authors, narrators, series and genres** stay in Russian as the site has them.
- **Genres** are the site's topics, without its store shelves ("Аудио", "Бесплатно", "Книга +
  кино").
- **Publisher** is the edition's first publisher. **Dates** are the edition's publication date as
  a Moscow calendar day, with its year.
- **Pages** come from books and comics, **duration** in seconds and **narrators** from audiobooks.
- **Series** is the first series the title belongs to. Its number is not on the title's own page,
  which lists the series' other parts with theirs; it is the one number missing from that list, or
  the next one when none is missing. When two or more are missing it is left out. A publisher's
  collection, such as "РЕШ: Белая книга", is also a series on Yandex; it is recognised by its other
  parts being mostly by other authors, and left out.
- **Description** is the annotation as plain text, paragraphs kept, else the editor's one-line
  summary.
- **Cover** is the site's large cover: portrait for books and comics, square for audiobooks.

Known gaps:

- No ISBNs and no ratings: the site shows neither. Its readers' reactions are emoji counts, not a
  score.
- Many editions, often public-domain classics, state no publisher and no date. The provider does
  not fill them from the rights holder or the "original year" the site keeps, which are often not
  the publisher (for "Пикник на обочине", the authors' heirs) or not the year (1881 for
  "Преступление и наказание").
- Series numbers follow the site's order, which is occasionally wrong: its comic "Метро 2033" lists
  "Часть 3, 4" as part 1.
- Some books are on Yandex only in another medium, such as Акунин's "Азазель", only as an audiobook.
  A book search then answers the audiobook, which BookOrbit ranks as another medium.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The tests run offline against a stand-in for the site. The fixtures in `testdata/` are real Yandex
Books pages captured on 2026-10-23, cut down to the data events the provider reads, in the pages'
own framing, with readers' names removed from the search results.
