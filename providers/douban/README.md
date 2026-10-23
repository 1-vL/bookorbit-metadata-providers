# Douban metadata provider

BookOrbit metadata from [Douban Books](https://book.douban.com) (豆瓣读书), the largest
Chinese-language book catalogue: titles, subtitles, authors, publishers, ISBNs, page counts,
ratings and blurbs, as Douban has them (BookOrbit issue #1875). It runs beside
BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde, regex and html-escape. The image is a static musl
binary on `scratch`.

Douban has no public API for this. The provider reads the same pages a reader's browser does, one
at a time; check that this use fits [Douban's terms](https://www.douban.com/about/agreement) for you.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  douban:
    image: ghcr.io/bookorbit/douban-provider:0.1.0
    restart: unless-stopped
```

Then add `http://douban:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t douban-provider .`, or with a Rust toolchain,
`cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `DOUBAN_COOKIE` | unset | Your own logged-in douban.com `Cookie` header, sent with every request (see below) |
| `REQUEST_INTERVAL_MS` | `500` | Gap between requests to Douban. Values under 500 are raised to 500 |
| `DOUBAN_SITE` | `https://book.douban.com` | Where requests go. Record links always point at book.douban.com |

The provider keeps at most two requests to Douban in flight and starts them at least 500 ms apart,
whatever its clients ask for. A caller that would wait more than 8 seconds for its turn gets `429`
with `Retry-After` instead.

### When Douban pushes back

Douban challenges traffic it finds heavy: it redirects to a captcha on `sec.douban.com` or a login
page, or answers `403` or `418`. The provider never tries to get past a challenge. It answers `429`
with `Retry-After: 600`, then sends Douban nothing for ten minutes, answering `429` with the time
left; never an empty list, which BookOrbit would remember as "not on Douban". A `429` from Douban
itself pauses the provider for as long as Douban asks.

If your address is challenged often, you can give the provider your own Douban session: log in to
douban.com in a browser, copy the `Cookie` request header of any book.douban.com page from the
developer tools, and set it as `DOUBAN_COOKIE`. Treat it like a password: anyone holding it is
logged in as you. Keep it in an env file rather than a Compose file you share, and replace it when
you log out or it expires. Douban may still challenge a logged-in session.

## What it sends to Douban

The title BookOrbit searches for, the ISBN, and Douban subject ids, with the user agent
`bookorbit-douban-provider/<version>` and, when set, your cookie. The author is never sent; it is
only used to order Douban's suggestions. The provider logs request paths and status codes, never a
title or an ISBN.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `douban`, books only, lookup, one search at a time 500 ms apart, icon and colour |
| `GET /search?query=&author=&mediaType=&isbn=&limit=` | `limit` defaults to 5, at most 10 |
| `GET /book/{id}` | A Douban subject id, such as `2567698` |

Status codes: `400` no query and no valid ISBN, `401` bad token, `404` unknown id, `429` with
`Retry-After` when Douban throttles or challenges the provider or the queue is long, `502` when
Douban fails. A search where some pages could not be fetched still answers `200` with
`"partial": true`. Audiobook searches answer an empty list without asking Douban.

## How it searches

- **By title:** Douban's search box suggestions (`/j/subject_suggest`) give up to a handful of
  editions, best first. The provider fetches each one's subject page, two at a time, for the full
  record. Suggestions by the requested author move to the front; names match whatever Douban
  writes around them ("加西亚·马尔克斯" matches "[哥伦比亚]加西亚•马尔克斯", "Liu Cixin" matches
  "Cixin Liu").
- **By ISBN:** `book.douban.com/isbn/{isbn}/` redirects to the subject that carries it, or answers
  404. A found ISBN answers with that one book; an unknown one falls back to the title, if there is
  one.

## Mapping

- **One record per Douban subject**, which is one edition. Titles, names and blurbs stay as Douban
  writes them, in Chinese for Chinese editions.
- **Authors** are the `作者` row. Translators (`译者`) are never authors. Douban's country or
  dynasty prefix ("[哥伦比亚]", "（美）", "【以】"), a trailing original-language name
  ("（Yuval Noah Harari）") and a role word ("著") are taken off the name, so every edition of an
  author files under one name.
- **Subtitle** is the page's subtitle line, or the `副标题` row. **Original title** (`原作名`) is
  given as `originalTitle`.
- **Publisher** is the `出版社` row; for a co-publication, the first house. The `出品方` (producer,
  such as 新经典文化) is not the publisher.
- **Published:** `出版年` gives the year; a full date such as `2014-11-11` is also `publishedDate`.
  Douban mostly records a year and month ("2008-1"), which gives the year only.
- **Series:** none. Douban's `丛书` row is the publisher's imprint line (such as
  "科幻世界·中国科幻基石丛书"), which has no position and is not story order, so it is given as a tag
  rather than filling a library's series field.
- **Rating** is Douban's 10-point average halved to a 5-point scale, with the number of ratings. A
  book with too few ratings for Douban to show one has none.
- **Description** is the full content blurb (`内容简介`), unfolded when Douban shortens it behind
  "(展开全部)", in plain paragraphs. The author biography is never used.
- **Language:** Douban records none, so `languageCode` comes from the title's script: Chinese
  characters `zh`, kana `ja`, hangul `ko`. A title in Latin script gets none.
- **Tags** are the book's top Douban tags, without the ones that are its title or a person's name,
  then its `丛书`. Douban has no genre classification, so nothing is given as genres.
- **Cover:** none, see below.

## Known gaps

- **No covers.** Douban's image servers (`img*.doubanio.com`) answer `418` to every request that
  does not carry a Douban page as its referer, BookOrbit's downloads and its search thumbnails
  included, so records leave the cover out rather than send a link that cannot load.
- No story series, genres, narrators or audiobook data; Douban has none of them for a book.
- Douban's suggestions return only a few editions of a much-reprinted book; the rest are reachable
  by ISBN.
- Some Douban entries carry made-up ISBNs (Douban's own test entries among them), so an ISBN
  search can land on a junk subject. The answer is Douban's own.
- Douban changes its page markup now and then. A page the provider can no longer read answers
  `502`, not an empty record.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The fixtures in `testdata/` are real Douban answers captured on 2026-10-22: two suggestion lists,
six subject pages, an ISBN redirect and the not-found page. The subject pages are cut down to the
parts the provider reads, and the English edition's blurb is cut after its first two paragraphs,
which go on into a long excerpt of the novel. The challenge answers in the server tests (a captcha
redirect, a block page, `403`, `418`) are written by hand: the provider was never pushed hard
enough to see a real one.
