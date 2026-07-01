# OmniAgent — Dockerfile
# Builds the Rust binary inside the container on startup.
# The source code is mounted at /app:rw from the omniagent repo.
# Rebuild: docker compose restart omniagent (depends_on ensures postgres is healthy first)

FROM rust:1.96.0

# Install Docker CLI from official Docker repo
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian \
    $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | tee /etc/apt/sources.list.d/docker.list > /dev/null \
    && apt-get update \
    && apt-get install -y docker-ce-cli docker-compose-plugin nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install sqlx-cli for compile-time query verification against the live database
RUN cargo install sqlx-cli --version 0.9.0

WORKDIR /app

# Build and run — builds inside the container on the compose network.
# Uses the pre-generated .sqlx offline query cache (generated via
# `cargo sqlx prepare` on the host or inside the running container)
# so SQLX_OFFLINE can remain true for fast, database-free builds.
# Regenerate the cache with: cargo sqlx prepare -- --lib
CMD ["bash", "-c", "apt-get update -qq 2>/dev/null && apt-get install -y -qq nodejs 2>&1 | tail -1; cargo build --release && exec ./target/release/omniagent"]
