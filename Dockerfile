# syntax=docker/dockerfile:1
# ==============================================================================
# AGOS Memory — multi-stage image (0007).
#
#   Builder: rust:1.98-bookworm  → installs musl-tools + musl target, builds the
#             static `x86_64-unknown-linux-musl` release binary (0006).
#   Runtime: alpine:3.20 (musl)  → runs the single static binary; BusyBox wget
#             powers the compose /healthz healthcheck.
#
# No OpenSSL, no system sqlite, no extra runtime deps: rustls + rusqlite-bundled
# are compiled in. ENTRYPOINT runs `serve` in HTTP (Streamable HTTP + JSON API)
# mode. The DB lives on a volume at /data.
# ==============================================================================

FROM rust:1.98-bookworm AS builder
WORKDIR /build

# One-time cross toolchain for the static musl build (0006).
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl

# Layer-cache the dependency graph before copying sources.
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
# A stub src lets `cargo build` resolve & compile the full dependency graph;
# the real sources are copied next and only agos-memory itself recompiles.
RUN mkdir -p src && printf '' > src/.keep \
    && cargo build --release --target x86_64-unknown-linux-musl \
         --locked --offline 2>/dev/null || true

COPY src/ ./src/
# The bench manifests are declared in Cargo.toml, so cargo validates their
# presence even for `cargo build` (they aren't compiled here).
COPY benches/ ./benches/
RUN cargo build --release --target x86_64-unknown-linux-musl --locked

# -----------------------------------------------------------------------------
# Runtime — alpine:3.20 (musl). Our binary is a musl-static link, so Alpine runs
# it with zero add-ons, and its BusyBox provides the `wget` the compose
# healthcheck needs. (Deviation from 0007's distroless/static-debian12: that
# base ships no shell/tools — not even a static BusyBox that can run there — so
# an in-container HTTP healthcheck is impossible; Alpine is the smallest musl
# base that supports one.)
# -----------------------------------------------------------------------------
FROM alpine:3.20

# Single static musl binary; nothing else to install.
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/agos-memory /usr/local/bin/agos-memory

# Writable store (single-process flock lives here too).
VOLUME /data
ENV AGOS_MEMORY_DB_PATH=/data/memory.db

EXPOSE 8710
# Default transport is HTTP (Streamable HTTP + JSON API). `serve --stdio` is the
# alternative. bind/token come from the runtime env (see docker-compose.yml):
#   AGOS_MEMORY_BIND=0.0.0.0:8710   AGOS_MEMORY_TOKEN=<required>
ENTRYPOINT ["agos-memory", "serve"]