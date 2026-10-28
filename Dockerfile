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
FROM ghcr.io/inxm-ai/enterprise-mcp-bridge:latest@sha256:9edffe288ea111bfd59bf29b9d23285070090b271a36e7eeaba652f7a349cdc3
# Chromium for the render stage of `extract` and the crawlers (stygian-browser
# drives it over CDP). Debian's package follows the security updates of the
# bridge's base release; fonts-liberation gives pages a usable default font.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends chromium fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
ENV STYGIAN_CHROME_PATH=/usr/bin/chromium
COPY --from=builder /build/target/release/mcp-websearch /usr/local/bin/mcp-websearch
RUN chmod +x /usr/local/bin/mcp-websearch
# Chromium renders arbitrary pages with its own sandbox on, and that sandbox
# only runs for a non-root user, so the whole server runs as one. The chart
# pins the same UID (infra/helm/templates/deployment.yaml).
RUN useradd --uid 10001 --user-group --create-home --shell /usr/sbin/nologin websearch
USER 10001:10001
