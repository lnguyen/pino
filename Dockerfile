# Pino proxy — multi-stage Rust build. The dashboard HTML is embedded into the
# binary (include_str!), so the runtime image is just the static-ish binary +
# CA certs. No Node, no npm.

FROM rust:1-slim AS builder
WORKDIR /app
# bundled SQLite (rusqlite) and zstd compile C sources → need a C toolchain.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release --bin pino-proxy

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -u 1000 -m app \
    && mkdir -p /data && chown -R app /data
COPY --from=builder /app/target/release/pino-proxy /usr/local/bin/pino-proxy

# Metrics DB lives on a mounted volume so it survives restarts.
ENV PORT=8898 \
    BIND_HOST=0.0.0.0 \
    AUTO_CACHE=1 \
    METRICS=1 \
    DASHBOARD=1 \
    DB_PATH=/data/metrics.db
VOLUME ["/data"]
USER app

EXPOSE 8898

# Self-probe: the binary supports a --healthcheck mode (no curl/node needed).
HEALTHCHECK --interval=15s --timeout=3s --retries=3 \
  CMD ["pino-proxy", "--healthcheck"]

CMD ["pino-proxy"]
