# J-Novel Club metadata provider

BookOrbit metadata from [J-Novel Club](https://j-novel.club), the English publisher of Japanese light
novels and manga (BookOrbit issue #1022). Every record is one volume: its own title, cover, release
date and description, with the series and its place in it. It runs beside BookOrbit as a small
container.

Rust: axum, tokio, reqwest with rustls, serde and regex. The image is a static musl binary on
`scratch`; rustls carries its own root certificates, so there is nothing else in it.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  jnovelclub:
    image: ghcr.io/bookorbit/jnovelclub-provider:0.1.0
    restart: unless-stopped
```

Then add `http://jnovelclub:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t jnovelclub-provider .`, or with a Rust
toolchain, `cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `JNC_API` | `https://labs.j-novel.club/app/v2` | |
| `USER_AGENT` | `bookorbit-jnovelclub-provider/<version>` | |
| `REQUEST_INTERVAL_MS` | `500` | Gap between requests sent to J-Novel Club |
| `SERIES_REFRESH_MINUTES` | `360` | How old the series list may get before it is fetched again |
| `VOLUME_CACHE_MINUTES` | `60` | How long a series' volume list is kept |

J-Novel Club needs no API key and publishes no rate limit. Its API has no text search, so the
provider keeps J-Novel Club's whole series list (about 500 series, two requests) in memory, loads it
at start and refreshes it in the background; titles are matched against it locally. A search then
reads the volume lists of at most three series, each kept for an hour, so a library of one series'
volumes costs one request. A caller that would wait more than 15 seconds for its turn gets `429`
with `Retry-After` instead.

What it sends to J-Novel Club: series and volume ids only. The titles and authors BookOrbit asks
about never leave the container.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `jnovelclub`, media `book` and `comic`, lookup, a 1-second `rateLimit`, portrait covers |
| `GET /search?query=&author=&mediaType=&limit=` | `limit` defaults to 5, at most 20 |
| `GET /book/{id}` | A J-Novel Club volume id, such as `VOL-SD5FU4Q` |

Status codes: `400` no query, `401` bad token, `404` unknown id, `429` with `Retry-After` when
J-Novel Club throttles or the queue is long, `502` when J-Novel Club fails. Audiobook searches and
ISBN-only searches answer no matches.

## Mapping

- **One record per volume.** The title is J-Novel Club's ("Ascendance of a Bookworm: Part 3 Volume
  2"); the series is J-Novel Club's series, with the volume's position in it as the sequence. That
  position is what the site counts by: the Bookworm manga's Part 3 Volume 1 is 20, not the 101
  J-Novel Club numbers it internally.
- **Finding the series.** An exact series title first, then a manga adaptation ("Ascendance of a
  Bookworm (Manga)") of the same name, then a series title followed by more words (a part, a volume
  subtitle), then a shortened or reordered title. Up to three series are read, so a search answers
  the novel, its manga and a spin-off side by side. A manga volume's `mediaType` is `comic`, a
  novel's `book`, so a client can tell the two apart.
- **Finding the volume.** A volume number in the query ("Vol. 3", "v03", "#3", or a bare number
  after the series title) is matched against J-Novel Club's volume number, the number in the volume
  title and its position. Words after the series title ("Part 3", a subtitle) must be in the volume
  title, numbers exactly. A volume titled exactly as asked always wins. Without a number, the
  series' first volume.
- **Authors** are the writers credited. Illustrators, translators, editors and letterers are left
  out; a volume with no writer credited takes its artists. An `author` in the query moves the
  volumes that credit that writer or artist first, matching names in any order.
- **Publisher** is J-Novel Club and the language English, for every record.
- **Date** is the ebook release, or the print release for a volume J-Novel Club only printed.
- **Cover** is the 1200-pixel JPEG from J-Novel Club's image CDN, the size its own pages use (the
  API links a 480-pixel WebP), with the size it is served at. The CDN never enlarges, so older
  volumes stored at 400 pixels stay at 400.
- **Genres** are the series tags that name a genre; imprints, tropes and shop labels ("available in
  print") are left out.
- **Description** is J-Novel Club's own, already plain text.
- **Link** is the series page at the volume's anchor. There is no `bookUrlTemplate`: the site has no
  page addressed by a volume id.
- **Lookup** reads the volume and its series (two requests), or nothing when a recent search
  already read that series.

Known gaps: no ISBN, page count or rating, since the API has none. A series J-Novel Club adds
appears after the next series-list refresh, and a newly listed volume after its series' volume
list expires. A short title shared by several series ("Arifureta 3") prefers the shortest series
title. The 1200-pixel cover size is the CDN's own convention, not an API field; if the CDN stops
serving it, covers break until the provider is updated.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The fixtures in `testdata/` are real J-Novel Club answers captured on 2026-10-22, cut down to the
fields the provider reads; the two series-list pages are a selection of the real list, and em dashes
in descriptions are written as spaced hyphens. The tests serve them from a stand-in for the API, so
they need no network.
