# =============================================================================
# homeCore — Core Server
# Alpine Linux — minimal, static-friendly runtime
# =============================================================================
#
# Build:
#   docker build -t homecore:latest .
#
# Run:
#   docker run -d \
#     -p 8080:8080 -p 1883:1883 \
#     -v ./config/homecore.toml:/opt/homecore/config/homecore.toml:ro \
#     -v homecore-data:/opt/homecore/data \
#     -v homecore-rules:/opt/homecore/rules \
#     -v homecore-logs:/opt/homecore/logs \
#     homecore:latest
#
# Volumes:
#   /opt/homecore/config   homecore.toml, modes.toml, profiles/
#   /opt/homecore/data     state.redb, history.db
#   /opt/homecore/rules    automation rule TOML files (hot-reloaded)
#   /opt/homecore/logs     rolling log files
#
# Ports:
#   8080   REST + WebSocket API
#   1883   Embedded MQTT broker
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1 — Build
# -----------------------------------------------------------------------------
FROM rust:alpine AS builder

RUN apk upgrade --no-cache && apk add --no-cache musl-dev openssl-dev pkgconfig

WORKDIR /build

# Fetch dependencies before copying source for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY src/ ./src/
COPY plugins/plugin-sdk-rs/ ./plugins/plugin-sdk-rs/
COPY plugins/examples/ ./plugins/examples/

RUN cargo build --release --bin homecore

# -----------------------------------------------------------------------------
# Stage 2 — Runtime
# -----------------------------------------------------------------------------
FROM alpine:3

# `apk upgrade` first pulls CVE patches for packages baked into the
# alpine:3 base since the upstream image was last rebuilt. Defense
# in depth — without this, `apk add --no-cache` only refreshes the
# named packages, leaving busybox/musl/etc. on the base's frozen
# versions.
RUN apk upgrade --no-cache && \
    apk add --no-cache \
        ca-certificates \
        libssl3 \
        tzdata \
        curl

# Non-root service user
RUN adduser -D -h /opt/homecore homecore

# Binary
COPY --from=builder /build/target/release/homecore /usr/local/bin/homecore
RUN chmod 755 /usr/local/bin/homecore

# Directory layout (data + logs are volumes; config and rules may be volumes or bind-mounts)
RUN mkdir -p \
        /opt/homecore/config/profiles \
        /opt/homecore/data \
        /opt/homecore/logs \
        /opt/homecore/rules/examples

# Baked-in defaults — overridden by mounted config at runtime
COPY config/homecore.toml.example  /opt/homecore/config/homecore.toml.example
COPY config/profiles/              /opt/homecore/config/profiles/
COPY rules/examples/               /opt/homecore/rules/examples/

RUN chown -R homecore:homecore /opt/homecore

USER homecore
WORKDIR /opt/homecore

VOLUME ["/opt/homecore/config", "/opt/homecore/data", "/opt/homecore/rules", "/opt/homecore/logs"]

EXPOSE 8080 1883

ENV HOMECORE_HOME=/opt/homecore \
    RUST_LOG=info

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -f http://localhost:8080/api/v1/health || exit 1

ENTRYPOINT ["homecore", "--home", "/opt/homecore"]
