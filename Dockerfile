# Multi-stage Dockerfile for zesty-backup
# Build stage
FROM rust:1.70-slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifest files
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build the application
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN useradd -m -u 1000 zesty && \
    mkdir -p /app/backups /app/logs /app/config && \
    chown -R zesty:zesty /app

# Copy binary from builder
COPY --from=builder /app/target/release/zesty-backup /usr/local/bin/zesty-backup

# Set working directory
WORKDIR /app

# Switch to non-root user
USER zesty

# Set environment variables
ENV RUST_LOG=info
ENV CONFIG_PATH=/app/config/config.toml

# Create volumes
VOLUME ["/app/backups", "/app/logs", "/app/config"]

# Default command
ENTRYPOINT ["zesty-backup"]
CMD ["--help"]

