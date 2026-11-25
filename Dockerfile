# syntax=docker/dockerfile:1

# =============================================================================
# Stage 1: Build Vue 3 + TypeScript Web Console (web/dist)
# =============================================================================
FROM node:20-bookworm-slim AS web-builder
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# =============================================================================
# Stage 2: Build Rust hm-core Monolith Binary (with in-process Silero VAD)
# =============================================================================
FROM rust:1.100-bookworm AS core-builder
WORKDIR /src
ARG HM_CORE_BUILD=unknown
ENV HM_CORE_BUILD=${HM_CORE_BUILD}
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release --locked -p hm-core

# =============================================================================
# Stage 3: Minimal Production Runtime Image for NAS / Home Server
# =============================================================================
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=core-builder /src/target/release/hm-core /usr/local/bin/hm-core
COPY --from=web-builder /src/web/dist /app/web/dist

# Run as a dedicated non-root user (1000:1001 matches NAS bind-mount ownership)
# and lock /data to 0750 rather than world-writable 0777.
RUN groupadd -g 1001 -f hm \
    && useradd -u 1000 -g 1001 -M -s /usr/sbin/nologin hm \
    && chmod 0755 /usr/local/bin/hm-core \
    && chmod -R a+rX /app/web/dist \
    && mkdir -p /data/models \
    && chown -R 1000:1001 /data \
    && chmod 0750 /data /data/models

USER 1000:1001

ENV TZ=Asia/Shanghai \
    HM_DATA_DIR=/data \
    HM_WEB_DIR=/app/web/dist \
    HM_BIND=0.0.0.0:8930 \
    HM_GATEWAY_BIND=0.0.0.0:8932 \
    HM_GATEWAY_ALLOW_LAN=1 \
    RUST_LOG=hm_core=info

EXPOSE 8930 8932 8933/udp
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8930/api/v0/status >/dev/null \
     && curl -fsS http://127.0.0.1:8930/ | grep -qi "<!doctype html" || exit 1

ENTRYPOINT ["/usr/local/bin/hm-core"]
