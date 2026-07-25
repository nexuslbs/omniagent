# syntax=docker/dockerfile:1
# OmniAgent: production multi-stage build
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
# build.py auto-discovers all workspace members from Cargo.toml and
# builds everything — omniagent, db-migrations, and all plugin binaries
# (platforms + tools). No hardcoded package lists.
RUN python3 scripts/build.py

# Stage 2: Docker CLI binary
FROM docker:cli AS docker-cli

# Stage 3: Runtime: slim image matching builder glibc
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update -qq && \
    apt-get install -y -qq ca-certificates curl git python3 && \
    rm -rf /var/lib/apt/lists/* && \
    git config --global --add safe.directory '*'

# Copy Docker CLI (compose v2 is built into the docker binary)
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker

# Copy the omniagent binary, db-migrations, and all plugin binaries.
# Globs auto-catch new tools (mcp-server-*) and platforms (*-platform)
# without requiring Dockerfile changes when plugins are added or removed.
COPY --from=builder /build/target/release/omniagent /usr/local/bin/omniagent
COPY --from=builder /build/target/release/mcp-server-* /usr/local/bin/
COPY --from=builder /build/target/release/*-platform /usr/local/bin/
COPY --from=builder /build/target/release/db-migrations /usr/local/bin/db-migrations

EXPOSE 8080
CMD ["omniagent"]
