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
ARG HM_CORE_BUILD=unknown
LABEL org.opencontainers.image.title="Home Mate Core" \
      org.opencontainers.image.description="Home Mate self-hosted AI voice assistant core brain & Vue 3 management console (with built-in Silero VAD v4)" \
      org.opencontainers.image.url="https://mymate.homes" \
      org.opencontainers.image.source="https://github.com/yes999zc/home-mate" \
      org.opencontainers.image.version="${HM_CORE_BUILD}" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl tzdata \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=core-builder /src/target/release/hm-core /usr/local/bin/hm-core
COPY --from=web-builder /src/web/dist /app/web/dist
COPY LICENSE LICENSE-PROTOCOL-APACHE-2.0 LICENSING.md THIRD_PARTY_NOTICES.md /usr/share/doc/home-mate/

# Bundle the audited MIT-licensed Silero VAD v4 model (643,854 B) at
# /app/models/silero_vad.onnx so fresh NAS Docker deployments immediately use
# neural VAD without manual file placement. Users can still override it via
# /data/models/silero_vad.onnx or HM_SILERO_VAD=off.
RUN mkdir -p /app/models \
    && curl -fsSL https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx \
       -o /app/models/silero_vad.onnx \
    && echo "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6  /app/models/silero_vad.onnx" \
       | sha256sum -c - \
    && chmod 0644 /app/models/silero_vad.onnx

# Run as a dedicated non-root user (1000:1001 matches NAS bind-mount ownership)
# and lock /data to 0750 rather than world-writable 0777.
RUN groupadd -g 1001 -f hm \
    && useradd -u 1000 -g 1001 -M -s /usr/sbin/nologin hm \
    && chmod 0755 /usr/local/bin/hm-core \
    && chmod -R a+rX /app/web/dist /app/models /usr/share/doc/home-mate \
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
