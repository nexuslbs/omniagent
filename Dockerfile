# OmniAgent — Development Dockerfile
# Source code is mounted as a volume; binary built on host with `cargo build --release`.
# No docker cp needed — just build then `docker compose restart omniagent`.

FROM rust:1.96.0

# Install Docker CLI
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl-dev \
    libpq-dev \
    && rm -rf /var/lib/apt/lists/*

# Install Docker CLI from official Docker repo
RUN install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
    && chmod a+r /etc/apt/keyrings/docker.asc \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian \
    $(. /etc/os-release && echo \"$VERSION_CODENAME\") stable" | tee /etc/apt/sources.list.d/docker.list > /dev/null \
    && apt-get update \
    && apt-get install -y docker-ce-cli docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Run the pre-built release binary from the mounted target/ directory.
# Build on the host: cd /opt/workspace/omniagent && cargo build --release
# Then restart: docker compose restart omniagent
CMD ["./target/release/omniagent"]
