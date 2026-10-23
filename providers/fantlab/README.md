# FantLab metadata provider

BookOrbit metadata from [fantlab.ru](https://fantlab.ru), the Russian-language science fiction and
fantasy database (BookOrbit issue #1709). It runs beside BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde, regex and html-escape. The image is a static musl
binary on `scratch`.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  fantlab:
    image: ghcr.io/bookorbit/fantlab-provider:0.1.0
    restart: unless-stopped
```

Then add `http://fantlab:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t fantlab-provider .`, or with a Rust toolchain,
`cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `FANTLAB_API` | `https://api.fantlab.ru` | |
| `FANTLAB_SITE` | `https://fantlab.ru` | Base for cover and page links |

fantlab.ru is a community site, so the provider keeps at most two requests in flight and starts
them at least 250 ms apart, whatever its clients ask for.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `fantlab`, books only, lookup, at most 2 requests in flight 250 ms apart, icon and colour |
| `GET /search?query=&author=&mediaType=&isbn=&language=&limit=` | `limit` defaults to 5, at most 10. `language` (such as `pl`) asks for the original title |
| `GET /book/{id}` | `3104` is a work, `e723` an edition |

Status codes: `400` no query, `401` bad token, `404` unknown id, `429` with `Retry-After` when
fantlab throttles, `502` when fantlab fails. A search where some matches could not be fetched still
answers `200` with `"partial": true`.

## Mapping

- **Title and authors** are Russian, since fantlab catalogues Russian editions. Passing `language`
  equal to the work's original language (`pl` for Lem) returns the original title and author names.
- **ISBN search** finds the edition, so title, authors, publisher, year, pages, cover and ISBNs are
  the printed book's; description, genres, series and rating come from its work.
- **Genres** are the classifier's genre group, **tags** its characteristics, setting, time and plot
  groups; a label counts once half the voters agree. Reader age and plot linearity are left out.
- **Series** is the nearest cycle, numbered by its place in the cycle's own reading order. Side
  stories, comics and other extras the cycle marks as such are not counted, nor are sub-cycles:
  Ночной Дозор is 1 of Дозоры, Шестой Дозор 6.
- **Rating** is fantlab's 10-point average halved to a 5-point scale, with the voter count.
- **Description** is plain text; BBCode is stripped and spoiler blocks are dropped.
- Reviews, articles, essays, cycles, screenplays, poems, excerpts and `other` (appendices, film
  papers) are never offered.

Known gaps: publisher, ISBN and page count come from an edition, so only an ISBN search has them;
no audiobook answers; English titles of Russian books are rarely found because fantlab stores
Russian names; co-authors fantlab files as one (Аркадий и Борис Стругацкие) stay one name.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The fixtures in `testdata/` are real fantlab responses captured on 2026-10-22. This provider was
first written in Go; the Rust version gives the same answers to the same requests.
