# OmniAgent

Next-generation agent system built with Rust, PostgreSQL + pgvector, and MCP tool support.

## Quick Start

### Requirements

- Docker & Docker Compose
- An LLM API key (OpenCode Go, OpenAI, or Anthropic)

### Setup

1. Clone the repo:
   ```bash
   git clone https://github.com/nexuslbs/omniagent.git
   cd omniagent
   ```

2. Copy the environment template and configure:
   ```bash
   cp .env.example .env
   ```
   Edit `.env` and set at minimum:
   - `LLM_API_KEY` — your LLM provider API key
   - `DATABASE_URL` — PostgreSQL connection string (default: `postgres://omniagent:omniagent@postgres:5432/omniagent`)

3. Start the stack:
   ```bash
   docker compose up -d
   ```

This starts:
- **PostgreSQL 16 + pgvector** — message storage with vector embeddings
- **Qdrant** — vector similarity search (optional, for semantic search)
- **OmniAgent** — the agent itself, on port 8080

### Verify

```bash
curl http://localhost:8080/health
# → ok
```

## Sending Messages

Messages are inserted directly into the database. The agent polls for `pending` messages every second.

```sql
INSERT INTO messages (channel_id, thread_id, thread_sequence, role, content, status, msg_type, iteration_count, profile)
VALUES (1, 1, 0, 'user', 'Your prompt here', 'pending', 'message', 0, 'default');
```

### Channel Setup

Channels represent communication endpoints. Create one before sending messages:

```sql
INSERT INTO channels (name, platform, external_id, cause, current_profile)
VALUES ('my-channel', 'api', 'my-channel-1', 'user', 'default');
```

Each channel can set a custom profile, provider, and model:
```sql
UPDATE channels SET current_profile = 'research', current_provider = 'anthropic', current_model = 'claude-sonnet-4' WHERE id = 1;
```

## Profiles

Profiles bundle model configuration, provider, and allowed tools. A `default` profile is created on first startup.

Profile fields:
- **provider** — LLM provider (e.g., `opencode-go`, `openai`, `anthropic`)
- **model** — LLM model name (e.g., `deepseek-v4-flash`)
- **allowed_tools** — which MCP tools the agent can use

### Creating a Profile

```sql
INSERT INTO profiles (name, provider, model, allowed_tools)
VALUES (
  'research',
  'anthropic',
  'claude-sonnet-4',
  '["filesystem_read", "filesystem_write", "fetch", "search_messages", "search_wiki"]',
  '/opt/data/profiles/research'
);
```

### Profile vs Channel Priority

The effective model and provider are resolved as:
1. **Channel** `current_model` / `current_provider` (highest)
2. **Profile** `model` / `provider`
3. Environment defaults
4. Built-in fallbacks

If neither the channel nor the profile specifies a model, the prompt will fail with an error.

## Configuration Reference

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OMNI_DATA_DIR` | `/opt/data` | Profile and tools directory |
| `DATABASE_URL` | `postgres://omniagent:omniagent@postgres:5432/omniagent` | PostgreSQL connection string |
| `QDRANT_URL` | `http://localhost:6333` | Qdrant endpoint |
| `LLM_API_KEY` | — | API key for LLM provider |
| `LLM_PROVIDER` | `opencode-go` | Provider: opencode-go, openai, anthropic |
| `LLM_MODEL` | `deepseek-v4-flash` | Default LLM model |
| `LLM_BASE_URL` | *per provider* | API endpoint URL |
| `MAX_TOKENS` | `4096` | Max response tokens |
| `TEMPERATURE` | `0.7` | Sampling temperature |
| `MAX_ITERATIONS` | `60` | Max agent turns per thread |
| `HOST` | `0.0.0.0` | HTTP bind address |
| `PORT` | `8080` | HTTP port |
| `DELETE_AFTER_DAYS` | `30` | Message retention period |

## API Endpoints

### `GET /health`

Health check. Returns `ok` with status 200.

### `GET /stop/{channel_id}`

Stop processing for a channel. All pending messages are marked as `skipped`.

### `GET /resume/{channel_id}`

Resume processing for a stopped channel.

## Backup Container

The stack includes a standalone **backup** container for S3 data durability. It is agent-agnostic — does not require the agent to be running, making it suitable for setup on a new machine before the agent starts.

### Architecture

```yaml
services:
  backup:
    build: ./backup
    env_file: backup.env          # NOT git-versioned
    volumes:
      - ./data:/opt/data:rw
```

### Commands

Run inside the container (`docker compose exec backup <command>`):

| Command | Description |
|---------|-------------|
| `backup` | Syncs `/opt/data/` to `S3_BUCKET/S3_PATH/data/` |
| `checkpoint` | Syncs `/opt/data/` to `S3_BUCKET/S3_PATH/checkpoint/YYYYMMDD/` |
| `restore_backup` | Syncs from `S3_BUCKET/S3_PATH/data/` to `/opt/data/` |
| `restore_checkpoint YYYYMMDD` | Syncs from `S3_BUCKET/S3_PATH/checkpoint/YYYYMMDD/` to `/opt/data/` |

### Configuration (`backup.env`)

| Variable | Example | Description |
|----------|---------|-------------|
| `S3_ENDPOINT` | `https://s3.us-east-005.backblazeb2.com` | S3-compatible endpoint |
| `S3_REGION` | `us-east-005` | S3 region |
| `S3_BUCKET` | `my-bucket` | S3 bucket name |
| `S3_PATH` | `omni` | Path prefix within the bucket |
| `S3_ACCESS_KEY` | — | S3 access key ID |
| `S3_SECRET_KEY` | — | S3 secret access key |
| `CRON_BACKUP` | `"0 5 * * *"` | Backup schedule (empty = disabled) |
| `CRON_CHECKPOINT` | `"0 3 * * 0"` | Checkpoint schedule (empty = disabled) |

Both backup and checkpoint use `rclone sync` with rclone v1.74+.

## Data Directory Structure

Persistent data lives under `OMNI_DATA_DIR` (default `/opt/data`):

```
$OMNI_DATA_DIR/
  profiles/
    default/
      memories/         # Memory files (MEMORY.md, SOUL.md)
      skills/           # Reusable skills
      wiki/             # Wiki reference content
  tools/                # MCP tool definitions
```

## Architecture Diagram

```
┌──────────────┐     ┌────────────────┐     ┌────────────┐
│   Messages   │────>│   OmniAgent    │────>│    LLM     │
│ (PostgreSQL) │     │    (Rust)      │     │  Provider  │
└──────────────┘     │                │     └────────────┘
                     │  ┌──────────┐  │
┌──────────────┐     │  │   MCP    │  │
│   Qdrant     │<────│  │  Tools   │  │
│  (Vectors)   │     │  └──────────┘  │
└──────────────┘     └────────────────┘
```

Messages flow: **PG → Agent → LLM → (tool calls loop) → PG**

## Docker Compose

### Production

```yaml
services:
  postgres:
    image: pgvector/pgvector:pg16
    expose: ["5432"]

  qdrant:
    image: qdrant/qdrant:v1.18.2
    expose: ["6333"]

  omniagent:
    build: .
    depends_on: [postgres, qdrant]
    env_file: .env
    expose: ["8080"]
    volumes:
      - ./.env:/app/.env:ro
```

### Development

For local development outside Docker:
```bash
# Run postgres + qdrant, then:
cargo run
```

The binary reads `.env` automatically via `dotenvy`.

## Message Lifecycle

```
User inserts a message (status = pending)
  │
  ▼
Agent picks it up, marks as processing
  │
  ├─ LLM responds with text → saved as msg_type='message'
  ├─ LLM includes reasoning → saved as msg_type='reasoning' (separate row)
  └─ LLM calls tools → tool executed, result fed back, loop continues
  │
  ▼
Prompt marked as completed, processing_time_ms set
```

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| Messages stay `pending` | Channel stopped or agent not running | Check `GET /health`, resume channel |
| LLM call fails | API key missing or invalid | Check `LLM_API_KEY` in `.env` |
| Processing stuck at `processing` | Container restarted mid-call | On restart, pending/processing messages are marked as skipped |
| No model configured | Profile + channel both lack model | Set `current_model` on channel or `model` on profile |
| Tools returning errors | Path outside data directory | Ensure file paths are under `OMNI_DATA_DIR` |

## Internal Docs

For detailed internal architecture, see [AGENTS.md](AGENTS.md).
