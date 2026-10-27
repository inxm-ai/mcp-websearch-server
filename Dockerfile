# Alpine's Rust image targets musl by default, so the built binary is static
# and runs unmodified inside the bridge base regardless of its own libc —
# unlike a glibc build, it carries no dependency on that image's OS.
ARG RUST_BUILDER_IMAGE=rust:1-alpine

FROM ${RUST_BUILDER_IMAGE} AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
# Cargo.lock is committed (binary crate) — the build installs exactly what
# CI tested.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# The bridge this server is built and tested on: the Python 3.11 build
# (plain `latest`), pinned by digest. PR CI checks the image on exactly this
# digest, and Dependabot opens a PR when `latest` moves, so the check runs on
# the new bridge before it deploys. The reference stays literal because
# Dependabot doesn't read it through ARG variables.
FROM ghcr.io/inxm-ai/enterprise-mcp-bridge:latest@sha256:99531015fc8023906c6dd2c9c0d2c2b632c1373a913680ed978189b776cf82e3
COPY --from=builder /build/target/release/mcp-websearch /usr/local/bin/mcp-websearch
RUN chmod +x /usr/local/bin/mcp-websearch
