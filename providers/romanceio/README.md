# Romance.io metadata provider

BookOrbit metadata from [Romance.io](https://www.romance.io), including community topics,
community ratings and the site's steam/spice level. It runs beside BookOrbit as a small container.

The spice value is returned as the first genre in the form
`Spice: 3/5 - Open door`. BookOrbit does not currently have a dedicated spice field, so this keeps
the value available to field rules, filters and book details without changing the provider contract.

## Run

```yaml
services:
  romanceio:
    image: ghcr.io/bookorbit/romanceio-provider:0.1.0
    restart: unless-stopped
```

Add an external metadata provider in BookOrbit with the address `http://romanceio:8080`.

Every request is read-only. A search sends the title and author to Romance.io, then reads up to five
book pages by default to collect topics and spice data. Calls are serialized and start at least one
second apart. Romance.io may place direct requests behind a Cloudflare challenge. This provider does
not bypass challenges or store browser cookies; it returns an upstream failure so BookOrbit can use
its cache instead of treating the failure as an empty search.

## Settings

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8080` | Listening port |
| `AUTH_TOKEN` | empty | Exact value required in the `Authorization` header when set |
| `USER_AGENT` | BookOrbit provider identifier | User agent sent to Romance.io |

`ROMANCEIO_SITE` is available only for tests and local troubleshooting against a compatible mirror.

## Endpoints

- `GET /provider/info`
- `GET /search?query=Pride%20and%20Prejudice&author=Jane%20Austen&limit=5`
- `GET /book/5484ecd47a5936fb0405756c`

The permanent provider id is Romance.io's 24-character book id. Lookup uses the stable
`/books/{id}` redirect, so ids remain valid when a title or author slug changes.

## Mapping

| Romance.io | BookOrbit |
|---|---|
| Book id | Provider id |
| Title, authors and blurb | Title, authors and description |
| Cover image | Cover |
| Published date | Published date and year |
| Page count and language | Page count and language |
| Series and book number | Series name and index |
| Community topics | Genres |
| Steam/spice level and label | `Spice: N/5 - label` genre |
| Community score and vote count | Community rating and count |

Romance.io records works rather than individual ISBN editions, so ISBN, publisher and edition-specific
format data are not returned. Audiobooks mentioned as a topic remain the same work record and are not
offered as separate audiobook matches.

## Verify

Tests use trimmed real response shapes and do not call Romance.io.

```bash
docker run --rm -v "$PWD":/src -v mb-cargo-registry:/usr/local/cargo/registry -w /src \
  rust:1.90-alpine sh -c 'apk add --no-cache musl-dev >/dev/null; rustup component add rustfmt clippy >/dev/null; cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test'
```
