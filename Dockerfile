# syntax=docker/dockerfile:1
# OmniAgent: production multi-stage build
# Builds the Rust binary, then copies it into a runtime image with
# Docker CLI and all plugin compilation toolchains (Rust, Node.js, Python).

# Stage 1: Build the Rust binary
FROM rust:1.96.0 AS builder
WORKDIR /build

# ── Dependency caching ─────────────────────────────────────────────
# Copy ALL workspace manifests + lockfile FIRST so the dep-compile layer
# below only invalidates when Cargo.toml/Cargo.lock change — not on every
# source edit. Without every member manifest present, `cargo build` fails
# instantly and NOTHING gets cached (each build recompiles all deps).
COPY Cargo.toml Cargo.lock* ./
COPY db-migrations/Cargo.toml ./db-migrations/
COPY plugins/tools/util/Cargo.toml ./plugins/tools/util/
COPY plugins/tools/cron/Cargo.toml ./plugins/tools/cron/
COPY plugins/tools/docker/Cargo.toml ./plugins/tools/docker/
COPY plugins/tools/kanban/Cargo.toml ./plugins/tools/kanban/
COPY plugins/tools/search/Cargo.toml ./plugins/tools/search/
COPY plugins/tools/memory/Cargo.toml ./plugins/tools/memory/
COPY plugins/tools/plugin-manager/Cargo.toml ./plugins/tools/plugin-manager/
COPY plugins/tools/subtasks/Cargo.toml ./plugins/tools/subtasks/
COPY plugins/tools/prompt/Cargo.toml ./plugins/tools/prompt/
COPY plugins/tools/notes/Cargo.toml ./plugins/tools/notes/
COPY plugins/tools/fetch/Cargo.toml ./plugins/tools/fetch/
COPY plugins/tools/filesystem/Cargo.toml ./plugins/tools/filesystem/
COPY plugins/tools/git/Cargo.toml ./plugins/tools/git/
COPY plugins/tools/ssh/Cargo.toml ./plugins/tools/ssh/
COPY plugins/tools/skills/Cargo.toml ./plugins/tools/skills/
COPY plugins/platforms/mattermost/Cargo.toml ./plugins/platforms/mattermost/

# Stub every workspace member (BOTH bin and lib targets — the root
# declares [lib], and plugins depend on mcp-server-util / db-migrations
# as libs, so a main.rs-only stub fails manifest resolution and caches
# nothing) so cargo compiles all dependencies once here. Real sources
# replace the stubs in the COPY . . step below; surviving stubs (lib-only
# crates that got a main.rs stub, bin-only crates that got a lib.rs stub)
# carry a //STUB marker and are deleted before the real build.
RUN mkdir -p src .sqlx && \
    echo "fn main() {} //STUB" > src/main.rs && \
    echo "//STUB" > src/lib.rs && \
    for d in db-migrations plugins/tools/* plugins/platforms/*; do \
      mkdir -p "$d/src"; \
      echo "fn main() {} //STUB" > "$d/src/main.rs"; \
      echo "//STUB" > "$d/src/lib.rs"; \
    done; \
    cargo build --release --workspace 2>/dev/null || true

# Install dev components needed for lint checks
RUN rustup component add rustfmt clippy

# Copy the rest of the source and build
COPY . .
ENV SQLX_OFFLINE=true
# Remove any stub files that survived COPY . . (lib-only crates whose
# real source has no main.rs, bin-only crates with no lib.rs) so they
# don't compile as phantom targets. Scoped to source dirs only — never
# touch target/ (compiled deps from the stub build must be preserved).
# Also touch every real source: COPY . . preserves host file mtimes,
# which predate the stub-build artifacts, so cargo's mtime freshness
# check would otherwise consider the STUB rlibs up-to-date and skip
# recompiling the real sources (plugins then link an empty omniagent
# lib — "unresolved import omniagent::db"). Touching forces cargo to
# recompile the workspace crates while still reusing cached deps.
RUN grep -rl "//STUB" --include="*.rs" src db-migrations plugins 2>/dev/null | xargs -r rm && \
    find src db-migrations plugins -name "*.rs" -exec touch {} + && \
    echo "stub cleanup + source touch done"
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
#   - python3-psycopg2: for integration tests (tests.py) that read DB rows
#     directly (workflow tests GROUP 22) — Debian package because PEP 668
#     blocks bare pip on python3.13
#   - python3-yaml: integration tests read config yml files (settings.yml)
#   - nodejs: for JavaScript/Node.js MCP servers
#   - npm: Debian's nodejs package does NOT ship npm; required by the
#     install API's NodeJS dependency step (npm ci/install) for remote
#     NodeJS MCP servers (reference servers: everything, filesystem, git).
#     legacy-peer-deps is set globally because several MCP reference
#     servers (e.g. @modelcontextprotocol/server-everything) ship zod v4
#     peer dependencies that strict npm resolution rejects (observed
#     HTTP 500 on install); the flag makes installs robust on reinstall.
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
      python3-psycopg2 \
      python3-yaml \
      nodejs \
      npm && \
    rm -rf /var/lib/apt/lists/* && \
    npm config set legacy-peer-deps true && \
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

# Emit the API reference (route table) into the image so the running
# agent can always locate it without the source (see skill omniagent-api).
# Source file: api-reference.md at the repo root (docs/ is excluded from the
# build context). Canonical path: /opt/omni/docs/api.md; /app/docs/api.md
# fallback survives a fresh /opt/omni volume mount.
COPY api-reference.md /opt/omni/docs/api.md
COPY api-reference.md /app/docs/api.md

EXPOSE 8080
CMD ["omniagent"]
