# =============================================================================
# hc-roku — HomeCore Roku Plugin (External Control Protocol)
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t hc-roku:latest .
#
# Run:
#   docker run -d \
#     -v ./config/config.toml:/opt/hc-roku/config/config.toml:ro \
#     -v hc-roku-logs:/opt/hc-roku/logs \
#     hc-roku:latest
#
# Volumes:
#   /opt/hc-roku/config   config.toml (broker settings, optional device list)
#   /opt/hc-roku/logs     rolling log files
#
# Networking:
#   SSDP discovery is multicast on 239.255.255.250:1900 and does not cross
#   a Docker bridge network. Run with `--network host` for auto-discovery,
#   or keep bridge networking and list each Roku under `manual_hosts` /
#   `[[devices]]` — ECP itself is ordinary unicast HTTP on port 8060 and
#   works fine either way.
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:1.95-alpine3.23@sha256:606fd313a0f49743ee2a7bd49a0914bab7deedb12791f3a846a34a4711db7ed2 AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/

RUN cargo build --release --bin hc-roku

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3.23@sha256:5b10f432ef3da1b8d4c7eb6c487f2f5a8f096bc91145e68878dd4a5019afde11

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata

RUN adduser -D -h /opt/hc-roku hcroku

COPY --from=builder /build/target/release/hc-roku /usr/local/bin/hc-roku
RUN chmod 755 /usr/local/bin/hc-roku

RUN mkdir -p /opt/hc-roku/config /opt/hc-roku/logs

COPY config/config.toml.example /opt/hc-roku/config/config.toml.example

RUN chown -R hcroku:hcroku /opt/hc-roku

USER hcroku
WORKDIR /opt/hc-roku

VOLUME ["/opt/hc-roku/config", "/opt/hc-roku/logs"]

ENV RUST_LOG=info

ENTRYPOINT ["hc-roku"]
