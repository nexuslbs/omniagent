# OmniAgent — AGENTS.md

## Guidelines

### SQL Queries: Always use sql_forge!()
**Every SQL query MUST use `sql_forge!()`.** No raw `sqlx::query`, `sqlx::query_as`, or `sqlx::query_scalar` except where documented below:

**Exceptions (must use `sqlx::query()` runtime):**
- **`src/db/migrations.rs`** — DDL (CREATE TABLE, ALTER TABLE, etc.) changes the schema at runtime. `sql_forge!()` validates columns against the live DB at compile time, creating a chicken-and-egg problem when the migration adds columns that the same migration file later references. Use `sqlx::query("SQL").execute(pool)` for all migration DDL and seed INSERTs.
- **pgvector `<=>` operator** — The `vector` type from pgvector is not in sqlx's hardcoded compile-time type registry. This affects `sqlx::query_as!` and `sql_forge!()` equally. Use `sqlx::query_as::<_, DbStruct>()` (runtime) with a comment explaining why.
- **Dynamic SQL** — Variable column sets or fully dynamic queries must use `sqlx::query(sqlx::AssertSqlSafe(sql))` with appropriate safety measures.

Dynamic SQL (variable column sets) should be decomposed into individual static `sql_forge!()` UPDATEs per field rather than building SQL strings at runtime.

**Type discipline:** Always match Rust types to the actual PostgreSQL column types:
- `INT4` (INTEGER) → `i32` or `Option<i32>`
- `INT8` (BIGINT) → `i64` or `Option<i64>`
- `TEXT` / `VARCHAR` → `String` or `Option<String>`
- `TIMESTAMPTZ` → `chrono::DateTime<Utc>` or `Option<...>`
- `JSONB` → `serde_json::Value` or `String` (with `.to_string()` for jsonb casts)

Never cast in Rust (`as i32`, `as i64`) when sql_forge can infer the correct type — use the right sql_forge scalar type instead.

### Column Aliases: No sqlx Proprietary Suffixes
**NEVER use sqlx-proprietary `?` / `!` suffixes in column aliases** (`AS "column?"`, `AS "column!"`).

These suffixes are handled by `sqlx::query_as!` (compile-time) but **NOT** by `sqlx::FromRow` (runtime). At runtime, `FromRow` looks for column names matching the Rust field names exactly, so `AS "created_at!"` produces a column named `created_at!` in the result — which `FromRow` can't find when looking for `created_at`.

**Correct approach:**
- Use `Option<T>` in the DB struct for expression columns with unknown nullability (COALESCE, TO_CHAR, etc.)
- Strip the suffix from the SQL alias so the column name matches the Rust field
- Convert to the domain type in `TryFrom` with `.unwrap_or_default()` / `as_deref().unwrap_or("")` (safe since COALESCE guarantees non-null)

The `.sqlx/` offline cache must be regenerated whenever the DB schema changes:
```bash
cargo sqlx prepare -- --bin omniagent
```

### Error Handling
- Use `anyhow::Result` for fallible functions
- Use `tracing` (info/warn/error) for logging, never `println!`

### Module Structure
- `src/db/types.rs` — All DB queries
- `src/agent/mod.rs` — Agent loop, message processing
- `src/mcp/tools/` — Individual tool implementations
- `src/prompt_builder.rs` — System prompt assembly
- `src/context_builder.rs` — Context retrieval assembly

### UI Modal Behavior

**Modal close-on-outside-click rules:**
1. **Form modals** (inputs, selects, textareas — user data entry): MUST NOT close when clicking outside. User must use Cancel/Close buttons. Examples: Create Schedule, Create Task, Edit Task, Install Plugin.
2. **Information/confirmation modals** (read-only display, simple confirmations): MAY close on outside click. Examples: status popups, simple confirm dialogs.

All modals should provide explicit close buttons (✕ close button + Cancel/Confirm buttons in footer).

### Tool Development
- Each MCP tool gets its own file in `src/mcp/tools/<name>.rs`
- Register in `default_registry()` in `src/mcp/mod.rs`
- Add to default profile's `allowed_tools` if it should be available by default
- Tool descriptions must include: ACTION PREFIX + USE CASE + NEGATIVE SPACE

### Subtask Tool (`manage_subtasks`)
- Located at `src/mcp/tools/subtasks.rs`
- Backend module at `src/subtask/mod.rs` — uses `sql_forge!()` for all DB operations
- DB table: `thread_subtasks` (columns: `id`, `thread_id`, `description`, `status`, `priority`, `created_at`, `updated_at`)
- Foreign key: `thread_id` → `threads(id)` with `ON DELETE CASCADE`
- **Actions**: `add` (insert + return full state), `list` (all for thread), `update` (status + description), `delete` (by id), `get_counts` (aggregate)
- **Current subtask** (`get_current_subtask`): queries the first `pending` row ordered by `priority DESC, created_at ASC`
- Prompt injection in `src/prompt_builder.rs:format_subtask_section()` — only injected when subtasks exist and at least one is non-cancelled
- **Override pattern**: Delete all subtasks for a thread (`DELETE FROM thread_subtasks WHERE thread_id = ?`), then add new ones

### Thread Summaries
- Summaries are stored in the `summaries` table (channel_id, next_thread_id, content, created_at)
- A summary is generated every `2*SUMMARY_WINDOW` completed seq-0 (thread-root) messages per channel
- The window slides by `SUMMARY_WINDOW`, so summaries overlap by half a window
- The last summary for a channel is always included in LLM context as a High-priority block
- Summary generation uses a separate LLM call with `SUMMARY_TOKENS` max tokens (default 4096)
- Old summaries are deleted alongside old messages via the daily cleanup task
- Config env vars: `SUMMARY_WINDOW` (default 10), `SUMMARY_TOKENS` (default 4096), `DELETE_AFTER_DAYS` (default 30)

### Provider/Model Stamping and Validation

**Stamping at creation time** — When any seq-0 message is created (user message, cron job, kanban ready-task), `provider` and `model` are resolved and stamped on the **thread** using this chain:
1. Channel `current_provider` / `current_model`
2. Profile `provider` / `model`
3. `LLM_PROVIDER` / `LLM_MODEL` env vars
4. Built-in defaults: `opencode-go` / `deepseek-v4-flash`

**Validation at execution time** — In `process_thread()` (src/agent/mod.rs), before calling the LLM, four checks run:
1. `thread.profile` is non-empty → `no-profile` error
2. Profile exists in `ProfileRegistry` → `invalid-profile` error (Note: ProfileRegistry.get() falls back to default profile, so this only fails if there's no default either)
3. `thread.provider` is `None` or empty → `no-provider` error
4. `thread.model` is `None` or empty → `no-model` error

Failed validation inserts a `msg_type='error'` message into the thread and marks the thread as `failed`.

### Thread Architecture

Messages are organized into **threads** — a thread represents a single user request (or cron/kanban trigger) and its LLM processing loop.

- `threads` table (Postgres): `id`, `status`, `cause`, `channel_id`, `profile`, `provider`, `model`, token usage, timestamps
- `messages` table: `id`, `thread_id` (FK), `role`, `content`, `thread_sequence`, `msg_type`, `msg_subtype`
- Profile/model/provider live on the **thread**, not individual messages

**Thread statuses:** `created` → `pending` → `processing` → `completed`/`failed`/`skipped`/`interrupted`

**Channel handlers** (one per channel, spawned by supervisor loop):
- Poll for `pending` threads every 1 second
- **Sequential** within a channel: threads are processed one at a time per channel (ordered by `created_at`)
- **Parallel** across channels: each channel handler runs as an independent tokio task

### Testing Guide

#### Test 1: All causes in a single channel
```sql
-- Insert threads with different causes in the same channel
INSERT INTO threads (status, cause, channel_id, profile, provider, model)
VALUES ('created', 'user', <ch_id>, 'default', 'opencode-go', 'deepseek-v4-flash');
-- repeat for 'cron' and 'kanban' causes

-- Add cause messages
INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
VALUES (<thread_id>, 'cause', 'your prompt', 0, 'message');  -- or 'cron', 'kanban'

-- Set pending
UPDATE threads SET status = 'pending' WHERE channel_id = <ch_id> AND status = 'created';
```

**Expected:** Threads processed **sequentially** (one after another) in the same channel. All complete with cause and msg_type preserved.

#### Test 2: Different channels (parallelism)
```sql
-- Insert threads in DIFFERENT channels
INSERT INTO threads (...) VALUES ('created', 'user',  <ch_a>, ...);
INSERT INTO threads (...) VALUES ('created', 'cron',  <ch_b>, ...);
INSERT INTO threads (...) VALUES ('created', 'kanban', <ch_c>, ...);
UPDATE threads SET status = 'pending' WHERE channel_id IN (<ch_a>, <ch_b>, <ch_c>);
```

**Expected:** Threads started at the **same second** — parallel processing across channels. Each channel's handler runs independently.

#### Test 3: Stop and Resume
```bash
# Stop a channel mid-processing
curl http://localhost:8080/stop/<channel_id>

# Check thread status — should be 'skipped'
SELECT id, status, cause, started_at IS NOT NULL as started, ended_at IS NOT NULL as ended
FROM threads WHERE channel_id = <channel_id> ORDER BY id DESC LIMIT 5;

# Resume the channel
curl http://localhost:8080/resume/<channel_id>

# New message executes immediately after resume
INSERT INTO threads (...) VALUES ('created', 'user', <ch_id>, ...);
```

**Expected:** Stopped threads get `skipped` status with `ended_at` set. After resume, new messages are picked up immediately by the next supervisor cycle.

#### Test 4: Failure cases
```sql
-- Empty provider → should fail with clear error
INSERT INTO threads (...) VALUES ('created', 'user', <ch>, 'default', '', 'deepseek-v4-flash');

-- Empty model → should fail with clear error
INSERT INTO threads (...) VALUES ('created', 'user', <ch>, 'default', 'opencode-go', '');

-- Nonexistent profile → falls back to default profile (intentional feature)
INSERT INTO threads (...) VALUES ('created', 'user', <ch>, 'nonexistent', 'opencode-go', 'deepseek-v4-flash');
```

**Expected:** Empty provider/model fail with clear error msg in thread. Nonexistent profile falls back to default.

#### Monitoring
```bash
# Watch processing
docker compose logs -f omniagent | grep -E "Processing|completed|summary"

# Query threads state
docker compose exec postgres psql -U omniagent -d omniagent -c "
SELECT t.id, t.status, t.cause, c.name as ch,
       (SELECT count(*) FROM messages m WHERE m.thread_id = t.id) as msg_count
FROM threads t JOIN channels c ON t.channel_id = c.id
WHERE t.channel_id = <ch_id> ORDER BY t.id;"
```
