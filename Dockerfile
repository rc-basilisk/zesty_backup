# Build stage
FROM rust:1-slim-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock config.example.toml ./
COPY src ./src
RUN cargo build --release --locked

# Runtime stage
FROM debian:bookworm-slim

# ca-certificates for HTTPS; zstd so archives can also be inspected with system tools
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates zstd \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -m -u 1000 zesty \
    && mkdir -p /app/backups /app/logs /app/config \
    && chown -R zesty:zesty /app

COPY --from=builder /app/target/release/zesty-backup /usr/local/bin/zesty-backup

WORKDIR /app
USER zesty

ENV ZESTY_BACKUP_CONFIG=/app/config/config.toml

VOLUME ["/app/backups", "/app/logs", "/app/config"]

ENTRYPOINT ["zesty-backup"]
CMD ["--help"]
