# syntax=docker/dockerfile:1
# OmniAgent — production multi-stage build
# Builds the Rust binary, then copies it into a minimal runtime image with Docker CLI.

# Stage 1: Build the Rust binary
FROM rust:1.96.0 AS builder
WORKDIR /build

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock* ./

# Create minimal src to cache deps
RUN mkdir -p src plugins .sqlx && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release 2>/dev/null || true

# Copy the rest of the source and build
COPY . .
ENV SQLX_OFFLINE=true
RUN cargo build --release

# Stage 2: Docker CLI binary
FROM docker:cli AS docker-cli

# Stage 3: Runtime — slim image matching builder glibc
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update -qq && \
    apt-get install -y -qq ca-certificates curl git python3 && \
    rm -rf /var/lib/apt/lists/*

# Copy Docker CLI (compose v2 is built into the docker binary)
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker

# Copy the omniagent binary
COPY --from=builder /build/target/release/omniagent /usr/local/bin/omniagent

EXPOSE 8080
CMD ["omniagent"]
