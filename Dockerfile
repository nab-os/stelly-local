# syntax=docker/dockerfile:1.7
#
# A music folder as a Stelly provider. Mount the folder at /music.
#
#   docker build -t stelly-local .

ARG RUST_VERSION=1.96.0
ARG DEBIAN_SUITE=trixie

FROM rust:${RUST_VERSION}-${DEBIAN_SUITE} AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    set -eu; \
    cargo build --release --locked; \
    mkdir -p /out; \
    cp target/release/stelly-local /out/

FROM debian:${DEBIAN_SUITE}-slim
# curl for the healthcheck.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10003 --user-group --create-home --home-dir /home/local \
        --shell /usr/sbin/nologin local
COPY --from=builder /out/stelly-local /usr/local/bin/stelly-local

# The index of what each file said, worth keeping across restarts.
ENV HOME=/home/local XDG_CACHE_HOME=/cache PUBLIC_URL=
RUN mkdir -p /cache && chown local:local /cache
VOLUME /cache
USER local
EXPOSE 7710

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7710/v1/health || exit 1

# Inside a container loopback is unreachable from outside it, so the bind is
# 0.0.0.0, which wants STELLY_PROVIDER_TOKEN set; publish the port on
# 127.0.0.1 only. PUBLIC_URL is how devices reach it.
ENTRYPOINT ["/bin/sh", "-c", "exec stelly-local serve /music --bind 0.0.0.0:7710 ${PUBLIC_URL:+--public-url $PUBLIC_URL}"]
