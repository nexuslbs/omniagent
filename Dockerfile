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
RUN cargo build --release && \
    cargo build --release -p db-migrations && \
    cargo build --release -p mcp-server-cron \
        -p mcp-server-kanban -p mcp-server-search \
        -p mcp-server-memory -p mcp-server-metrics \
        -p mcp-server-query -p mcp-server-plugin-manager \
        -p mcp-server-subtasks -p mcp-server-hindsight \
        -p mcp-server-prompt -p mcp-server-actions \
        -p mcp-server-fetch -p mcp-server-filesystem \
        -p mcp-server-git -p mcp-server-skills && \
    # Build integration test binaries so they're available in the runtime image
    cargo test --release --test api_tests --no-run 2>&1 | tail -1 && \
    cargo test --release --test plugin_tests --no-run 2>&1 | tail -1 && \
    # Copy test binaries to clean names (strip hash suffix)
    cp $(ls -t /build/target/release/deps/api_tests-* 2>/dev/null | grep -v '\.d$' | head -1) /build/api_tests && \
    cp $(ls -t /build/target/release/deps/plugin_tests-* 2>/dev/null | grep -v '\.d$' | head -1) /build/plugin_tests

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

# Copy the omniagent binary and all workspace member MCP server binaries
COPY --from=builder /build/target/release/omniagent /usr/local/bin/omniagent
COPY --from=builder /build/target/release/mcp-server-* /usr/local/bin/
COPY --from=builder /build/target/release/db-migrations /usr/local/bin/db-migrations
# Copy integration test binaries (built with clean names in builder stage)
COPY --from=builder /build/api_tests /usr/local/bin/api_tests
COPY --from=builder /build/plugin_tests /usr/local/bin/plugin_tests

EXPOSE 8080
CMD ["omniagent"]
