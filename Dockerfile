# syntax=docker/dockerfile:1
# OmniAgent: production multi-stage build
# Builds the Rust binary, then copies it into a runtime image with
# Docker CLI and all plugin compilation toolchains (Rust, Node.js, Python).

# Stage 1: Build the Rust binary
FROM rust:1.96.0 AS builder
WORKDIR /build

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock* ./

# Create minimal src to cache deps
RUN mkdir -p src plugins .sqlx && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release 2>/dev/null || true

# Install dev components needed for lint checks
RUN rustup component add rustfmt clippy

# Copy the rest of the source and build
COPY . .
ENV SQLX_OFFLINE=true
# build.py auto-discovers all workspace members from Cargo.toml and
# builds everything — omniagent, db-migrations, and all plugin binaries
# (platforms + tools). No hardcoded package lists.
RUN python3 scripts/build.py

# Run lint checks and unit tests. These always re-execute when source
# changes (Docker layer after COPY . .). Matches CI pretest steps so
# `docker build` catches failures that CI would catch on the runner.
# --all / --workspace / --all-targets are REQUIRED: without them cargo
# only validates the root package and every plugin crate (tools +
# platforms) would ship un-linted. These flags match deploy.py's
# run_pretests() so the image build gates on the same checks as CI.
RUN cargo fmt --all --check && \
    RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --release && \
    cargo clippy --workspace --all-targets --release -- -D warnings && \
    cargo test --workspace --release

# Stage 2: Docker CLI binary
FROM docker:cli AS docker-cli

# Stage 3: Runtime with build toolchains for on-demand plugin compilation
FROM debian:trixie-slim

# Install runtime dependencies + system libraries needed for
# on-demand compilation of Rust plugins (openssl-sys req by reqwest):
#   - pkg-config, libssl-dev: required by openssl-sys
#   - ca-certificates, curl, git: for cloning remote plugin repos
#   - python3, python3-pip: for Python plugins
#   - nodejs: for JavaScript/Node.js MCP servers
RUN apt-get update -qq && \
    apt-get install -y -qq \
      ca-certificates \
      curl \
      git \
      procps \
      pkg-config \
      libssl-dev \
      python3 \
      python3-pip \
      nodejs && \
    rm -rf /var/lib/apt/lists/* && \
    git config --global --add safe.directory '*'

# Copy Rust toolchain from builder for on-demand compilation of
# remote Rust plugins. Copy the toolchain (compiler + stdlib) from
# rustup and the cargo/rustc binaries from cargo's bin directory.
# Skip cargo's registry/ and git/ caches to keep image size smaller;
# cargo populates them on demand when compiling plugins.
# Official rust images use /usr/local/cargo and /usr/local/rustup.
COPY --from=builder /usr/local/rustup /usr/local/rustup
COPY --from=builder /usr/local/cargo/bin /usr/local/cargo/bin
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH="/usr/local/cargo/bin:${PATH}"

# Copy Docker CLI and compose plugin
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /usr/local/libexec/docker/cli-plugins/docker-compose /usr/local/libexec/docker/cli-plugins/docker-compose

# Copy the omniagent binary, db-migrations, and all plugin binaries.
# Globs auto-catch new tools (mcp-server-*) and platforms (*-platform)
# without requiring Dockerfile changes when plugins are added or removed.
COPY --from=builder /build/target/release/omniagent /usr/local/bin/omniagent
COPY --from=builder /build/target/release/mcp-server-* /usr/local/bin/
COPY --from=builder /build/target/release/*-platform /usr/local/bin/
COPY --from=builder /build/target/release/db-migrations /usr/local/bin/db-migrations

# Copy plugin config files (plugin.json, mcp-config.json) so built-in
# plugins are discoverable at /app/plugins/. The .dockerignore already
# strips target/ and node_modules/ from the build context.
COPY --from=builder /build/plugins /app/plugins

EXPOSE 8080
CMD ["omniagent"]
