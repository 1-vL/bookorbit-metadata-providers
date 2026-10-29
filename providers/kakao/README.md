# BookOrbit Kakao Book Metadata Provider

A Rust-based BookOrbit external metadata provider using Kakao's Daum Book Search API (`GET https://dapi.kakao.com/v3/search/book`).

## Run

```yaml
services:
  kakao:
    build: .
    restart: unless-stopped
    environment:
      KAKAO_REST_API_KEY: ${KAKAO_REST_API_KEY}
      # Optional provider-side auth token:
      # AUTH_TOKEN: ${BOOKORBIT_PROVIDER_TOKEN}
```

Set `KAKAO_REST_API_KEY` to a valid Kakao REST API key. Add `http://kakao:8080` in BookOrbit under **Settings > Metadata > Providers > External providers**.

Build and run locally:

```bash
cargo run --release
cargo test
cargo clippy --all-targets -- -D warnings
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8080` | HTTP server port |
| `KAKAO_REST_API_KEY` | Required | Kakao REST API key; sent as `Authorization: KakaoAK <key>` |
| `AUTH_TOKEN` | Unset | Optional token required in the provider endpoint's `Authorization` header |
| `USER_AGENT` | `bookorbit-kakao-provider/<version>` | Outbound HTTP User-Agent |

Never expose the Kakao REST API key to BookOrbit clients or log it.

## Endpoints

| Route | Description |
|---|---|
| `GET /provider/info` | Provider metadata and supported fields |
| `GET /search?query=&limit=&author=&target=&sort=&page=` | Search books |
| `GET /search?isbn=` | Search by ISBN (`target=isbn` is applied automatically) |
| `GET /book/{id}` | Retrieve a previously searched result from the in-memory cache |

`limit` defaults to 5 and is capped at 20. `target` accepts `title`, `isbn`, `publisher` or `person`; `sort` accepts `accuracy` or `latest`; `page` is clamped to Kakao's supported range of 1–50. Kakao's `size` is set from the provider result limit.

## Metadata mapping

| BookOrbit field | Kakao field | Notes |
|---|---|---|
| `title` | `title` | Book title |
| `description` | `contents` | Description |
| `url` | `url` | Daum book detail URL |
| `authors` | `authors` | Authors only; translators remain separate in the upstream response |
| `publisher` | `publisher` | Publisher name |
| `publishedDate` | `datetime` | ISO 8601 timestamp converted to `YYYY-MM-DD` |
| `publishedYear` | `datetime` | First four digits of the publication date |
| `cover` | `thumbnail` | Kakao cover thumbnail URL |
| `languageCode` | — | Set to `ko` as a provider-level default |
| `providerId` | Derived | SHA-256 hex digest (64 hexadecimal characters) of ISBN, URL or fallback metadata; no provider prefix |

Series and genres are not populated because the documented Kakao response does not provide reliable fields for them. Price, sale price, translators and status are available upstream but are not part of the current BookOrbit metadata struct.

## Lookup behavior and limitations

Kakao's documented Book Search API does not expose a dedicated lookup-by-ID endpoint. The provider derives a stable ID from available metadata and caches returned records in memory so `/book/{id}` can resolve results returned during the current process lifetime. A lookup may return 404 after a restart or if the record has not been searched since startup. This is an explicit limitation, not a Kakao API capability.

The provider makes one Kakao request per search. It accepts `page` but does not automatically fetch multiple pages. Author filtering is applied to the returned page because Kakao does not offer a separate `author` request parameter; `target=person` is the upstream search-field option.

Kakao's `status` values are display text and may change. ISBN may contain ISBN-10, ISBN-13, both or neither. Duplicate editions may be returned as separate results. API quotas and availability are controlled by Kakao.

## Tests

Tests cover metadata mapping, identifier format and basic request parameter behavior. Run them with `cargo test`. The source should be compiled and integration-tested against the BookOrbit version being deployed, because its external-provider contract is the authoritative schema.
