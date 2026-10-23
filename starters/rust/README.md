# Rust starter

A complete BookOrbit metadata provider that serves one made-up book. Copy this folder, then:

1. In `src/main.rs`, change `info()`: your provider's `name`, and a `key` you will never change.
2. In `src/source.rs`, replace `ExampleSource` with calls to your source. For HTTP, add `reqwest`
   with its `rustls-tls` feature.

```bash
cargo run
```

```bash
cargo test
```

```bash
docker build -t my-provider .
```

`PORT` sets the port (default `8080`). Set `AUTH_TOKEN` and `/search` and `/book/{id}` require that
exact `Authorization` header. The
[building guide](https://github.com/bookorbit/bookorbit/blob/main/docs/BUILDING_METADATA_PROVIDERS.md)
covers the rest.
