# LubimyCzytac metadata provider

BookOrbit metadata from [lubimyczytac.pl](https://lubimyczytac.pl), the Polish book catalogue and
reading community. It runs beside BookOrbit as a small container.

It replaces the LubimyCzytac source that used to be built into BookOrbit and keeps its key,
`lubimyczytac`, and its id format, so the ids already stored on your books, and your field rules
that name LubimyCzytac, carry over once you add it. It fetches the same pages and reads them the
same way, so searches return the same books with the same details. It adds the site's rating, and
descriptions keep their line breaks.

Rust: axum, tokio, reqwest with rustls, serde, regex and scraper. The image is a static musl binary
on `scratch`, about 3.4 MB. `scraper` parses the pages into a DOM and reads them with the CSS
selectors the built-in source used: elements are matched by class in any order, text is joined
across nested tags, and the language is read from the `dd` that follows its `dt`, none of which
regular expressions over raw HTML do reliably.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  lubimyczytac:
    image: ghcr.io/bookorbit/lubimyczytac-provider:0.1.0
    restart: unless-stopped
```

Then add `http://lubimyczytac:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. No account or key is needed. To build it yourself:
`docker build -t lubimyczytac-provider .`, or with a Rust toolchain, `cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `LUBIMYCZYTAC_SITE` | `https://lubimyczytac.pl` | Where pages are fetched from; links in answers always point at lubimyczytac.pl |
| `USER_AGENT` | `bookorbit-lubimyczytac-provider/0.1.0` | |
| `DEADLINE_MS` | `8000` | How long a search or lookup may take. A search that runs out answers with the books it has, marked partial. Raise it if you raise the provider's timeout in BookOrbit (10 seconds by default) |

lubimyczytac.pl is a community site, so the provider sends it one request at a time, at least a
second apart, whatever its clients ask for. When the site answers `429`, the provider
sends it nothing more until its `Retry-After` has passed, and answers `429` itself meanwhile.

What it sends to lubimyczytac.pl: the title and the author of each book BookOrbit asks about, and the ids it looks up. It logs request paths and failures, never
titles.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `lubimyczytac`, books, audiobooks and comics, lookup, one call at a time 500 ms apart, a URL template for book links, portrait covers, icon and colour |
| `GET /search?query=&author=&mediaType=&limit=` | `limit` defaults to 5, at most 5 |
| `GET /book/{id}` | An id such as `240310/ostatnie-zyczenie`, URL-encoded or not. A bare `240310`, as older BookOrbit versions stored, works too and answers with the full id |

A search fetches the site's search page, then each result's book page, so one search is up to six
requests and takes about three seconds.

Status codes: `400` no query, `401` bad token, `404` unknown id, `429` with `Retry-After` when
lubimyczytac.pl throttles, `502` when it fails or a page cannot be read. A search where some book
pages could not be fetched or read still answers `200` with `"partial": true`; one where none could
answers `502`, never an empty list.

## Mapping

- **Search** uses the title as the phrase and passes the author along. A search with only an ISBN
  answers empty without asking the site, whose search matches titles, not ISBNs. The site's order is kept, one record per book, at most five.
- **Ids** are the path after `/ksiazka/`: the numeric id and the slug, such as
  `240310/ostatnie-zyczenie`, taken from the page's canonical address.
- **Title, authors, series and genres** stay in Polish as the site has them. Authors come from the
  page's structured data. The series line ("Cykl: Wiedźmin (tom 1)", or "Komisarz Tymon Hanter 4")
  gives the series and its position; "2,5" becomes "2.5".
- **Genres** are the category line split at commas ("fantasy, science fiction"), then the readers'
  tags, at most ten.
- **Language** is the page's "Język" mapped to a code (`polski` is `pl`, also `angielski`,
  `niemiecki`, `francuski`, `hiszpański`, `włoski`), else `pl`. Other languages pass through as the
  site writes them.
- **ISBN** is the page's own, without hyphens, as `isbn13` or `isbn10` by length. Numbers of other
  lengths, which the site shows for some old editions, are left out.
- **Dates** are the edition's: `publishedDate` when the page has a whole date, `publishedYear`
  always.
- **Rating** is the site's 10-point average halved to a 5-point scale, with the vote count.
- **Description** is plain text with the site's line breaks kept: a blank line between paragraphs,
  a single line break inside lists such as a table of contents.
- **Cover** is the page's cover image (352 x 500), else its `og:image`.

Known gaps:

- No search by ISBN: lubimyczytac.pl's search matches titles only. The built-in source searched
  the digits anyway and got unrelated books back.
- Languages other than the six above arrive as Polish words (`rosyjski`), as they did before.
- BookOrbit's `bookUrlTemplate` link encodes the `/` in an id as `%2F`, which lubimyczytac.pl does
  not serve. Every answer carries its page as `url`, which BookOrbit links instead, so this only
  affects ids it has no page for.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The tests run offline against a stand-in for the site. The fixtures in `testdata/` are real
lubimyczytac.pl pages captured on 2026-10-22 and cut down to the parts the provider reads, with
reader reviews removed; the tests also carry the HTML snippets the built-in source was tested with.
Against the live site, searches and lookups returned the same books with the same details as the
built-in source, except that descriptions keep their line breaks.
