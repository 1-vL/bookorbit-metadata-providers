FROM rust:1.90-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The crate registry and the build folder outlive the build, so a rebuild recompiles only what
# changed. The binary is copied out because a cache mount is not part of the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=rust-starter-target,sharing=locked \
    cargo build --release --locked && cp target/release/provider /provider

# A static musl binary on an empty image. If you add reqwest, use its `rustls-tls` feature: it
# carries its own root certificates, so this image still needs nothing else.
FROM scratch
COPY --from=build /provider /provider
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/provider"]
