# MangaBaka metadata provider

BookOrbit metadata from [MangaBaka](https://mangabaka.org), the manga, manhwa, manhua and light
novel database that merges AniList, MyAnimeList, MangaUpdates, Kitsu, Anime-Planet and Shikimori
(BookOrbit issue #580). It runs beside BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde and regex. The image is a static musl binary on
`scratch`; rustls carries its own root certificates, so there is nothing else in it.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  mangabaka:
    image: ghcr.io/bookorbit/mangabaka-provider:0.1.0
    restart: unless-stopped
```

Then add `http://mangabaka:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. To build it yourself: `docker build -t mangabaka-provider .`, or with a Rust toolchain,
`cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `MANGABAKA_API` | `https://api.mangabaka.org` | |
| `SEARCH_INTERVAL_MS` | `2000` | Gap between searches sent to MangaBaka |
| `LOOKUP_INTERVAL_MS` | `340` | Gap between other requests sent to MangaBaka |

MangaBaka needs no API key. It allows 30 searches and 180 other requests a minute per IP; the
default gaps stay inside both. A caller that would wait more than 15 seconds for its turn gets
`429` with `Retry-After` instead.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `mangabaka`, media `book` and `comic`, lookup, a 2-second `rateLimit`, a URL template for record links, portrait covers |
| `GET /search?query=&author=&mediaType=&limit=` | `limit` defaults to 5, at most 20 |
| `GET /book/{id}` | A MangaBaka series id, or one volume of it (`1692v3`). A series merged into another answers with the one it now lives in |

Status codes: `400` no query, `401` bad token, `404` unknown id, `429` with `Retry-After` when
MangaBaka throttles or the queue is long, `502` when MangaBaka fails.

## Mapping

- **One record per series.** MangaBaka describes a series, not a volume. A volume in the query
  ("Berserk Vol. 3", "Berserk v03", "One Piece #5") is taken off before searching and returned as
  the series position, so a library of volumes gets the right number on each. A bare trailing
  number is left in the title, since it is as often part of one ("Mob Psycho 100").
- **Medium:** a record's `mediaType` is `comic`, or `book` for a series MangaBaka files as a novel,
  so a client can tell a light novel from its manga.
- **Ids** are MangaBaka's series id, with `v` and the volume for a volume (`1692v3`), so looking
  one up again answers that volume. Records carry the series' page as `url`.
- **Title** is the series' official English name, written as a shelf would ("ONE PIECE" is "One
  Piece", "SPY×FAMILY" is "Spy x Family"), with the volume when one was asked for: "Berserk, Vol. 3".
  The series name is the same without the volume.
- **Matching:** the asked-for author's series first, then a series named exactly as asked, then
  MangaBaka's order. MangaBaka's search is fuzzy, so a series none of whose names shares a word with
  the query ("June" for "Dune") is left out.
- **Authors** are MangaBaka's story credits, falling back to the artists, given name first
  ("MIURA Kentaro" is "Kentaro Miura"). An `author` in the query matches names in any order and
  ignoring separators ("Chugong" for "Chu-Gong").
- **Publisher** is the English-language publisher when there is one, else the original, and the
  **language** goes with it: `en` with an English publisher, else the original language.
- **Published** is the series' start, so it is given for the series and for volume 1 only, never
  as a later volume's date.
- **Rating** is MangaBaka's 100-point score on a 5-point scale. There is no vote count.
- **Description** is plain text; MangaBaka's Markdown is stripped.
- **Genres** read as words ("school_life" is "School Life"); MangaBaka's tags go to `tags`.
- **Cover** is the full-size original, with its width and height.
- Only active series are offered.

Known gaps: no ISBN, page count or per-volume data, since MangaBaka has none, so a later volume
has no publication date.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The fixtures in `testdata/` are real MangaBaka answers captured on 2026-10-22, cut down to the
fields the provider reads. The merged record is the real 1692 with its state changed.

## Why Rust

This provider was first written in Go as well, with identical answers, and both were measured
under the same load: a stand-in for MangaBaka serving its real 171 KB search answers, 4,000 mixed
searches and lookups at 1, 8 and 32 concurrent clients, three rounds each, on linux/arm64 with 11
vCPUs (2026-10-22). Medians:

| | Go | Rust |
|---|---:|---:|
| Idle | 5.4 MiB | 2.3 MiB |
| Steady use | 12 MiB | 4 MiB |
| 32 clients at once | 21.5 MiB | 7.8 MiB |
| Peak | 57 MiB | 12 MiB |
| Requests a second at 32 clients | 1,485 | 3,065 |
| CPU for the whole run | 19.5 s | 10.5 s |
| Image | 6.2 MB | 3.8 MB (2.8 MB now) |

Go's `GOMEMLIMIT=10MiB` brought its settled memory to 14 MiB but not its peak, and cost speed. In
real use MangaBaka's 30 searches a minute keep either build near its steady figure, so Go would
have done the job too; Rust does it in less.

The Rust build was later switched to optimise for size (`opt-level = "z"`, regex without its
speed-up engines), which took the image to 2.8 MB; the figures above are from the earlier build.
