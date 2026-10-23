# Libris metadata provider

BookOrbit metadata from [Libris](https://libris.kb.se), the Swedish national library catalogue run
by the National Library of Sweden (KB): printed books, e-books and audiobooks held by Swedish
libraries, the national bibliography among them (BookOrbit issue #1782). It runs beside BookOrbit
as a small container.

Rust: axum, tokio, reqwest with rustls, serde and regex. The image is a static musl binary on
`scratch`; rustls carries its own root certificates, so there is nothing else in it.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  libris:
    image: ghcr.io/bookorbit/libris-provider:0.1.0
    restart: unless-stopped
```

Then add `http://libris:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t libris-provider .`, or with a Rust toolchain,
`cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `LIBRIS_API` | `https://libris.kb.se` | |
| `REQUEST_INTERVAL_MS` | `1000` | Gap between requests sent to Libris |
| `USER_AGENT` | `bookorbit-libris-provider/0.1.0 (+https://github.com/bookorbit/bookorbit-metadata-providers)` | Sent with every request to Libris |

Libris needs no API key. KB publishes no rate limit, but asks clients to name themselves and may
block one that strains the service, so the provider starts at most one request a second whatever
its clients ask for, and declares the same `rateLimit`. A caller that would wait more than 15
seconds for its turn gets `429` with `Retry-After` instead.

The title, author and ISBN BookOrbit asks about go to libris.kb.se as search words; nothing else
leaves the provider. Its log has the path and status of each request, never the query.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `libris`, media `book` and `audiobook`, lookup, a one-second `rateLimit`, a URL template for record pages |
| `GET /search?query=&author=&mediaType=&isbn=&limit=` | `limit` defaults to 10, at most 20 |
| `GET /book/{id}` | A Libris record id, such as `v61jx8fbsd06xtgv` |

Status codes: `400` no query and no valid ISBN, `401` bad token, `404` unknown id, `429` with
`Retry-After` when Libris throttles or the queue is long, `502` when Libris fails.

## Mapping

- **One record per edition.** Each match is one printing, e-book or audiobook, with its own id,
  ISBN, publisher and year. The id is the Libris record id, and `https://libris.kb.se/{id}` is its
  public page.
- **Search** uses the Libris XL search API (`find.jsonld`), which needs every word to match
  somewhere in a record. It sends the title with the author's name. When no edition with that
  title comes back, it searches again without the subtitle, then without the author, and otherwise
  answers what the first search found. Editions with the asked-for title come first, then the
  asked-for author's, otherwise Libris's order. Chapters and articles catalogued on their own are
  left out.
- **ISBN search** looks for both forms of the ISBN and keeps only editions that list it. When none
  is in the asked-for medium and a title was given too, it searches by title instead.
- **Lookup** reads the record itself (`/{id}/data.json`) rather than the search index.
- **Books and audiobooks.** A record is an audiobook when its work carries the Ljudböcker or
  Talböcker genre term, or the literary-text type that only sound recordings have; it is a book
  when its content is text and it is neither. Book searches answer printed books and e-books,
  audiobook searches only audiobooks. Films, music, radio programmes, podcasts and software are
  never offered.
- **Authors** are the author role, main entry first, written "Astrid Lindgren". A name stored as
  "Lindgren, Astrid, 1907-2002" is turned round and its life dates dropped. Illustrators and
  translators are left out. **Narrators** are the narrator role.
- **Titles** lose what older records keep in the title field: a statement of responsibility after
  " / ", a subtitle after " : ", trailing ISBD punctuation. Titles in another script are given in
  that script, not transliterated.
- **Publisher and year** are the edition's own publication. A year outside 1450-2100, such as a
  Persian calendar year, is left out.
- **ISBN** is the one searched for, or the edition's first, as ISBN-13 with its ISBN-10.
- **Pages** come from the extent ("126, [1] s." is 126). **Duration**, for audiobooks, from the
  extent or the playing time note ("3 tim., 26 min.", "13283 sek.").
- **Language** is a two-letter code (`swe` is `sv`).
- **Genres** are the Swedish genre and form terms, without format terms such as "Ljudböcker";
  subjects go to `tags`. Both are in Swedish, as Libris writes them.
- **Series** is the edition's series statement, with its number when it has one.
- **Description** is the edition's summary, with the supplier's tag ("[Bokinfo]") taken off.
- **Cover** comes only from lookups of e-books and audiobooks whose records link one, which is rare,
  so `cover` is not among the declared fields.

Known gaps: no covers in search answers and none for printed books, since Libris serves its own
cover images only behind signed links that expire within days, and its search API leaves out the
cover links e-book records carry. Series include publishers' series ("R & S pocket") as well as
story series, which Libris does not tell apart. No ratings. Reprints often share an ISBN and some
editions are catalogued twice, so a search can show one book more than once. Streamed e-books and
audiobooks often have no page count or duration. Talking books for print-disabled readers (DAISY)
count as audiobooks.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The fixtures in `testdata/` are real Libris answers captured on 2026-10-22, cut down to the fields
the provider reads: book and audiobook searches for Pippi Långstrump, an ISBN search, an empty
search, two records read by id (a printed book and an audiobook), and the answer for an unknown
record. Tests serve them from a stand-in for Libris, so they need no network.
