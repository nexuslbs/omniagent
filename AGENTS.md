# OmniAgent — Internal Architecture

This document describes how OmniAgent works internally: the processing pipeline, data model, LLM integration, tool calling, and profile system. It is aimed at developers contributing to or debugging the agent.

## Overview

OmniAgent is a Rust async agent that reads pending messages from PostgreSQL, passes them to an LLM (with optional tool calling), and stores responses back into the database. It runs as a single binary with three concurrent subsystems:

- **Agent supervisor** — polls channels, spawns per-channel handlers
- **HTTP server** — health checks, stop/resume channel endpoints
- **Message cleanup** — daily deletion of old messages

## Data Model

### Channels

Each channel represents a communication thread (Telegram chat, cron job, API client, etc.):

| Column | Type | Description |
|--------|------|-------------|
| `id` | BIGSERIAL | Primary key |
| `name` | TEXT | Human-readable name |
| `platform` | TEXT | Platform type (telegram, cron, api) |
| `external_id` | TEXT | Platform-specific ID |
| `cause` | TEXT | 'user' or 'cron' |
| `current_profile` | TEXT | Active profile name (default: 'default') |
| `current_model` | TEXT? | Per-channel model override (overrides profile) |
| `current_provider` | TEXT? | Per-channel provider override |

### Messages

Messages form the core data model. Each message has a type discriminator and tracks its provenance:

| Column | Type | Description |
|--------|------|-------------|
| `id` | BIGSERIAL | Primary key |
| `channel_id` | BIGINT | FK to channels |
| `role` | TEXT | 'user', 'agent', 'system', 'tool' |
| `content` | TEXT | Message body |
| `status` | TEXT | pending, processing, completed, failed, skipped |
| `thread_id` | BIGINT? | Groups messages into conversations |
| `thread_sequence` | INT | Order within thread |
| `msg_type` | TEXT | Discriminator: 'message', 'reasoning', 'tool_call', 'tool_result' |
| `msg_subtype` | TEXT? | Optional subtype (tool name, etc.) |
| `iteration_count` | INT | Which agent turn in the thread |
| `profile` | TEXT | Profile used when processing |
| `provider` | TEXT? | LLM provider used |
| `model` | TEXT? | LLM model used |
| `processing_time_ms` | INT? | Time taken to process the prompt |
| `metadata` | JSONB | Usage info and other metadata |

The `msg_type` column allows splitting a single LLM turn into multiple records: reasoning/thinking blocks are stored as `msg_type='reasoning'`, the final text as `msg_type='message'`, and tool calls/results as `msg_type='tool_call'` / `msg_type='tool_result'`.

### Profiles

Profiles define the LLM configuration, allowed tools, and data directory for a channel:

| Column | Type | Description |
|--------|------|-------------|
| `id` | BIGSERIAL | Primary key |
| `name` | TEXT | Unique profile name |
| `model` | TEXT? | Default model |
| `provider` | TEXT? | Default provider |
| `base_url` | TEXT? | API URL override |
| `api_key` | TEXT? | API key override |
| `max_tokens` | INT? | Max tokens for this profile |
| `temperature` | FLOAT? | Temperature for this profile |
| `allowed_tools` | JSONB | List of allowed MCP tool names |
| `created_at` | TIMESTAMPTZ | When created |
| `updated_at` | TIMESTAMPTZ | When updated |

Profile resolution (for model/provider priority):
1. `msg.provider` / `msg.model` — set when message is created (channel's `current_model`/`current_provider` at message-insert time) — highest priority
2. Profile's own `model`/`provider` — fallback if message doesn't specify one
3. Env vars `LLM_PROVIDER` / `LLM_MODEL` — with hardcoded fallbacks `"openai"` / `"gpt-4"` if unset

## Processing Pipeline

```
User message (status=pending)
  │
  ▼
channel_handler (per-channel tokio task, polls every 1s)
  │
  ├─ Check cancellation & channel stop
  ├─ Fetch pending messages for channel
  │
  ▼
process_message
  │
  ├─ 1. Mark message → 'processing'
  ├─ 2. Check iteration limit (MAX_ITERATIONS, default 60)
  │      If exceeded → skip, continue
  ├─ 3. Resolve profile/model/provider from channel
  ├─ 4. Build message history + tool definitions
  │
  ▼
  ┌─ LLM call (with tools if configured)
  │    │
  │    ├─ Response has tool_calls?
  │    │   YES → Execute MCP tools, feed results back, loop
  │    │   NO  → Final text response received
  │    │
  │    └─ Max N LLM turns (capped at 20)
  │
  ▼
  ├─ 5. Save reasoning block (if present) → msg_type='reasoning'
  ├─ 6. Save agent response → msg_type='message'
  ├─ 7. Set processing_time_ms on original prompt
  └─ 8. Mark original message → 'completed'
```

### Iteration Limit

The `MAX_ITERATIONS` setting (default 60) controls two things:
- **Per-thread agent turns**: Counting `msg_type='message'` where `role='agent'`. Once this count reaches `MAX_ITERATIONS`, new user messages in that thread are skipped.
- **LLM tool-calling loops per message**: The tool-calling loop within `process_message` is capped at `min(MAX_ITERATIONS, 20)` to prevent runaway tool calls.

## Question Classifier

OmniAgent includes a heuristic query classifier (`context_builder::classify_query`) that categorizes user messages into five types and determines whether retrieval should run:

| Class | Description | Retrieval? | Examples |
|-------|-------------|-----------|----------|
| `Greeting` | Simple greetings / acknowledgments | No | "hi", "thanks", "ok", "👍" |
| `Command` | Instructions to perform an action | No | "/help", "write a file" |
| `FollowUp` | Brief follow-ups referencing previous context | No | "continue", "what about the other one" |
| `Factual` | Questions about system, project, or data | Yes | "What is X?", "How does Y work?" |
| `ExternalQuery` | Questions needing real-time/external data | Yes | "Show me the weather forecast" |

Retrieval is also triggered for messages >100 characters (complex tasks).

## Hybrid Retrieval

OmniAgent uses a tiered retrieval system controlled by a profile-level `retrieval_aggressiveness` setting (0-3):

| Level | Name | Retrieval Sources |
|-------|------|-------------------|
| 0 | Off | No retrieval (only recent thread context) |
| 1 | Conservative | ILIKE text search in messages + wiki text search |
| 2 | Balanced | Everything in Level 1 + pgvector semantic message search + Qdrant wiki search |
| 3 | Aggressive | Everything in Level 2 with higher limits |

### Retrieval Sources

1. **ILIKE text search** (`search_messages_text`) — keyword matching in messages table
2. **Wiki text search** (`search_wiki_text`) — walkdir-based content search in wiki markdown files
3. **pgvector semantic search** (`search_messages_semantic`) — cosine similarity via pgvector `<=>` operator against message embeddings (Level 2+)
4. **Qdrant wiki search** (`search_wiki_qdrant`) — vector similarity search in Qdrant wiki collection (Level 2+)

All retrieval results are assembled as `Low` priority context blocks via the ContextBuilder, trimmed by budget.

## Context Assembly

OmniAgent uses a `ContextBuilder` pipeline (see `src/context_builder.rs`) that assembles the LLM prompt from ordered blocks with priority-based trimming.

### Context Blocks (in priority order)

| Priority | Block | Description |
|----------|-------|-------------|
| NeverTrim | System/profile instructions | Agent identity, tool rules, grounding policy — never trimmed |
| NeverTrim | MEMORY.md | User-authored persistent memory, always included, size-capped |
| High | Recent thread messages | Recency window of current conversation thread (up to 10 messages) |
| High | Pinned user messages | Explicitly marked important messages |
| Normal | Active tool definitions | JSON Schema for all tools allowed by the profile |
| Low | Retrieved past messages | ILIKE text search results from past conversations |
| Low | Retrieved wiki snippets | Text search results from profile wiki files |

### Token Budgeting

- Total context budget: 4,000 characters (configurable)
- Output token reserve: 2,000 characters (subtracted from context budget)
- When total exceeds context budget, blocks are trimmed in priority order:
  - Per-block character caps are applied first (truncation)
  - If still over budget, entire low-priority blocks are dropped
- Never-trim blocks are always included in full

### Context Assembly Metadata

On every final agent response (and reasoning block), the `messages.metadata` JSONB column captures:

```json
{
  "context": {
    "selected_message_ids": [123, 124],
    "wiki_files": ["path/to/file.md"],
    "block_counts": {
      "recent_thread_messages": 1200,
      "retrieved_past_messages": 800
    },
    "dropped_blocks": ["low_priority_wiki"],
    "total_chars": 3500
  },
  "grounding": {
    "policy_applied": true
  }
}
```

## Grounding Policy

The grounding policy is embedded in every system prompt (constant `GROUNDING_POLICY` in `prompt_builder.rs`):

1. **Prefer retrieved evidence** over prior assumptions — cite evidence explicitly when available
2. **State uncertainty** — if uncertain about a factual/project-specific claim, say so clearly
3. **Provide grounding references** — for factual claims, reference message IDs, wiki file paths, or tool call IDs
4. **Trigger retrieval** — if insufficient evidence, ask a clarifying question or trigger search/retrieval before answering

## MCP (Model Context Protocol) Tools

Tools are invoked via OpenAI-compatible function calling format. The LLM receives a `tools` array in the request body, and can respond with `tool_calls` in the message.

### Tool Registry

Tools live in `src/mcp/tools/`. Each tool has:
- **name** — unique identifier (e.g. `filesystem_read`)
- **description** — explains when to use the tool
- **input_schema** — JSON Schema for parameters
- **handler** — synchronous closure that executes the tool

### Built-in Tools

| Tool | Description |
|------|-------------|
| `filesystem_read` | Read file contents (restricted to data dir) |
| `filesystem_write` | Write/overwrite a file |
| `filesystem_list` | List directory entries |
| `filesystem_search` | Glob search for files |
| `filesystem_info` | File/directory metadata |
| `fetch` | HTTP GET (research, API calls) |
| `search_messages` | ILIKE text search in messages table |
| `search_wiki` | Text search in profile wiki files |

### Path Restriction

All filesystem tools enforce that accessed paths must be within the data directory (`OMNI_DATA_DIR`, default `/opt/data`). Paths are canonicalized before the check.

## Message Lifecycle

```
status: pending → processing → completed
                      ↓
                   failed (on LLM error)
                      ↓
                   skipped (on iteration limit)
```

Each thread follows a sequence like:
```
User msg (thread_seq=0, iteration=0)
  Agent reasoning (thread_seq=1, msg_type='reasoning', iteration=1)
  Agent message (thread_seq=1, msg_type='message', iteration=1)
  User msg (thread_seq=2, iteration=1)
  Agent message (thread_seq=3, msg_type='message', iteration=2)
  ...
```

## Configuration

All configuration is via environment variables:

| Env Var | Default | Description |
|---------|---------|-------------|
| `OMNI_DATA_DIR` | `/opt/data` | Base data directory for profiles and tools |
| `LLM_MODEL` | `deepseek-v4-flash` | Default model |
| `LLM_PROVIDER` | `opencode-go` | Provider name |
| `LLM_BASE_URL` | provider default | API URL |
| `LLM_API_KEY` | — | API key |
| `MAX_TOKENS` | 4096 | Max response tokens |
| `LLM_MAX_TOKENS` | 8192 | LLM client max tokens |
| `TEMPERATURE` | 0.7 | Sampling temperature |
| `MAX_ITERATIONS` | 60 | Max agent turns per thread |
| `DATABASE_URL` | — | PostgreSQL connection string |
| `QDRANT_URL` | `http://localhost:6333` | Qdrant vector DB URL |
| `HOST` | `0.0.0.0` | HTTP server bind |
| `PORT` | 8080 | HTTP server port |
| `DELETE_AFTER_DAYS` | 30 | Message retention |

## Data Directory Structure

```
$OMNI_DATA_DIR/
  profiles/
    default/
      memories/        # MEMORY.md, SOUL.md
      skills/          # reusable skill definitions
      wiki/            # wiki content
  tools/               # shared MCP tool definitions
```

## Module Map

```
src/
  main.rs            ─ Entry point, initialization
  config.rs          ─ Base config (DB, Qdrant, server)
  agent/
    mod.rs           ─ Agent supervisor, channel handler, process_message
  llm/
    mod.rs           ─ LLM client, provider abstraction, function calling
  db/
    migrations.rs    ─ Schema migrations
    queries.rs       ─ All SQL queries
    schema.rs        ─ Schema documentation
    mod.rs           ─ DB connection pool
  models/
    channel.rs       ─ Channel, ChannelStop structs
    message.rs       ─ Message, MessageNew, MessageStatus
    profile.rs       ─ ProfileRow, ProfileNew
    mod.rs           ─ Module exports
  mcp/
    mod.rs           ─ McpRegistry, AppContext, tool execution
    tools/
      mod.rs         ─ Tool declarations
      filesystem.rs  ─ Filesystem MCP tools
      fetch.rs       ─ HTTP fetch tool
      search.rs      ─ Message & wiki search tools
  profile/
    mod.rs           ─ Profile struct, ProfileRegistry
  platform/
    mod.rs           ─ Platform trait, Telegram stub
  server/
    mod.rs           ─ HTTP endpoints (health, stop)
```

## Concurrency Model

- **Agent supervisor**: Single task, polls channels every 5s
- **Channel handlers**: One tokio task per channel, polls every 1s
- **HTTP server**: Axum on separate task
- **Message cleanup**: Background task runs daily
- **Vectorization workers**: Background tasks for embedding messages + wiki
- **Graceful shutdown**: tokio::select! over all tasks + Ctrl+C

The agent uses `CancellationToken` per channel — calling the `/stop/{channel_id}` HTTP endpoint cancels that channel's handler.

## Backup Container

A standalone `backup` container (in `backup/` directory) provides S3 data durability independent of the agent.

### Dockerfile

```dockerfile
FROM alpine:latest
RUN apk add --no-cache rclone dcron bash tini
```

Uses `tini` as PID 1 to handle signals and zombie reaping. The entrypoint generates an rclone config from S3 environment variables and starts crond (Dillon's cron daemon, foreground mode with `-f -l 2 -L /dev/stdout`).

### Scripts (`backup/scripts/`)

| Script | Installed as | Function |
|--------|-------------|----------|
| `backup.sh` | `/usr/bin/backup` | Syncs `/opt/data/` to `S3_BUCKET/S3_PATH/data/` via rclone |
| `checkpoint.sh` | `/usr/bin/checkpoint` | Syncs `/opt/data/` to `S3_BUCKET/S3_PATH/checkpoint/YYYYMMDD/` |
| `restore_backup.sh` | `/usr/bin/restore_backup` | Syncs from `S3_BUCKET/S3_PATH/data/` to `/opt/data/` |
| `restore_checkpoint.sh` | `/usr/bin/restore_checkpoint` | Syncs from a specific checkpoint to `/opt/data/` |
| `entrypoint.sh` | `/entrypoint.sh` | Generates rclone config, installs crontab, starts crond |

### rclone Configuration

The entrypoint writes an rclone config at `/etc/rclone/rclone.conf` with a remote named `s3-backup`:

```
[s3-backup]
type = s3
provider = Other
access_key_id = ${S3_ACCESS_KEY}
secret_access_key = ${S3_SECRET_KEY}
endpoint = ${S3_ENDPOINT}
region = ${S3_REGION}
```

All scripts reference this config via `RCLONE_CONFIG=/etc/rclone/rclone.conf`.

### Environment (`backup.env`, NOT git-versioned)

| Variable | Default | Description |
|----------|---------|-------------|
| `S3_ENDPOINT` | — | S3-compatible endpoint URL |
| `S3_REGION` | — | S3 region |
| `S3_BUCKET` | — | S3 bucket name |
| `S3_PATH` | `omni` | Path prefix in the bucket |
| `S3_ACCESS_KEY` | — | S3 access key ID |
| `S3_SECRET_KEY` | — | S3 secret access key |
| `CRON_BACKUP` | `"0 5 * * *"` | Cron schedule for daily backups (empty = disabled) |
| `CRON_CHECKPOINT` | `"0 3 * * 0"` | Cron schedule for weekly checkpoints (empty = disabled) |

### Cron Integration

The entrypoint dynamically generates the crontab from `CRON_BACKUP` and `CRON_CHECKPOINT`. Each cron command sets `RCLONE_CONFIG` and logs to `/var/log/backup.log` or `/var/log/checkpoint.log`. If a variable is empty, that schedule is omitted from the crontab.

### Agent-Agnostic Design

The backup container does not depend on the omniagent service. It only needs:
- `./data:/opt/data:rw` volume mount
- `backup.env` with valid S3 credentials
- Network access to the S3 endpoint

This allows restoring data onto a fresh machine before the agent is even built. Commands can be run imperatively:

```bash
# On a fresh machine with data/ empty:
docker compose run --rm backup restore_backup
docker compose up -d
```
