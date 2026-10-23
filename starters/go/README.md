# Go starter

A complete BookOrbit metadata provider that serves one made-up book, using nothing but Go's standard
library. Copy this folder, then:

1. In `main.go`, change `info()`: your provider's `name`, and a `key` you will never change.
2. In `source.go`, replace `exampleSource` with calls to your source.

```bash
go run .
```

```bash
go test ./...
```

```bash
docker build -t my-provider .
```

`PORT` sets the port (default `8080`). Set `AUTH_TOKEN` and `/search` and `/book/{id}` require that
exact `Authorization` header. The
[building guide](https://github.com/bookorbit/bookorbit/blob/main/docs/BUILDING_METADATA_PROVIDERS.md)
covers the rest.
