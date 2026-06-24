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

### Lint Attributes: Use `#[expect(...)]` not `#[allow(...)]`
Prefer `#[expect(dead_code)]` over `#[allow(dead_code)]`. The `expect` attribute produces a compiler warning when the lint no longer applies (i.e., the dead code became used), making it self-cleaning — you know to remove it. `#[allow]` silently hides the lint forever, even after the suppression is no longer needed.

This applies to ALL lint types, not just `dead_code`: if you must suppress a lint, use `#[expect(lint_name)]` so the compiler tells you when the suppression is stale.

### Module Structure
- `src/db/types.rs` — All DB queries
- `src/agent/mod.rs` — Agent loop, message processing
- `src/mcp/tools/` — Individual tool implementations
- `src/prompt_builder.rs` — System prompt assembly
- `src/context_builder.rs` — Context retrieval assembly

### Function Signatures: Use Structs for 4+ Parameters

Functions with **4 or more parameters** (beyond `pool`, `cause`, etc.) should use a parameter struct instead of positional arguments. This makes call sites self-documenting and avoids cascading changes when adding fields.

**Pattern:** Define a struct immediately before the function. Use owned `String` types when callers have owned values, or lifetime-annotated `&'a str` references when borrowing from a shared context. Convert to `&str` inside the body with `.as_deref()` / `.as_str()` as needed.

**Examples:**
```rust
pub struct CreateThreadParams {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub task_id: Option<String>,
    pub schedule_task_id: Option<String>,
    pub planning_mode: String,
}
```

```rust
struct ActionContext<'a> {
    pool: &'a PgPool,
    data_dir: &'a str,
    mcp_registry: &'a McpRegistry,
    app_context: &'a AppContext,
    job: &'a CronJobDueRow,
    display_name: &'a str,
}
```

Existing examples in the codebase: `CreateThreadParams`, `CompleteThreadStats`, `CreateChannelParams`, `ThreadLookupParams`, `ClaimChannelParams`, `UpsertPluginParams`, `ServerConfig`, `PlanningPromptParams`, `ActionContext`, `ReportActionFailureParams`, `AgentContext`, `ThreadContextIdentifiers`, `ThreadContextConfig`, `MakeVectorizerConfig`, `CollectWikiFilesCtx`.

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

### Dynamic Enum Refresh (`refresh_url`)

Provider plugins can source model lists dynamically from an external API using `refresh_url` on `enum` config schema fields.

**Source locations:**
- **Core logic:** `src/plugin/mod.rs` — `fetch_enum_values()`, `refresh_plugin_models()`, `DYNAMIC_ENUM_CACHE`
- **API handler:** `src/server/plugins.rs` — `refresh_models_handler()`
- **Route registration:** `src/server/mod.rs:106` — `POST /api/plugins/{name}/refresh-models`

**Schema field type (`ConfigSchemaField`):**
```rust
pub struct ConfigSchemaField {
    pub key: String,
    pub label: String,
    pub field_type: FieldType,  // String, Secret, Boolean, Integer, Enum, MultiSelect
    pub refresh_url: Option<String>,  // URL for dynamic enum values
    pub allowed_values: Option<Vec<String>>,  // Static fallback list
    // ...
}
```

**Cache architecture:**
- `DYNAMIC_ENUM_CACHE` — `Lazy<Mutex<HashMap<String, DynamicEnumEntry>>>` where key is the refresh_url
- `DynamicEnumEntry` stores `values: Vec<String>` + `fetched_at: Instant`
- TTL: `DYNAMIC_ENUM_TTL` = 300 seconds (5 minutes)
- Cache is checked in `enrich_plugin()` when populating `allowed_values` for API responses

**Refresh flow (`refresh_plugin_models()`):**
1. Fetches plugin by name from `plugin_registry` table
2. Enriches to `PluginDetail` with parsed config_schema
3. Iterates schema fields looking for non-empty `refresh_url`
4. Resolves API key: `{PLUGIN_NAME}_API_KEY` → `LLM_API_KEY` env vars (uppercased, dashes → underscores)
5. Calls `fetch_enum_values(url, api_key)` — GET request with 5s timeout, Bearer auth if key present
6. Parses response as `{data: [{id: "model-name"}, ...]}` (OpenAI `/v1/models` format)
7. On success: updates the field's `allowed_values` + populates the in-memory cache
8. On failure: logs warning, preserves existing `allowed_values` (no breaking change)
9. Returns `Some(detail)` if any field had a refresh_url, `None` otherwise

**API key resolution logic:**
```rust
let api_key = std::env::var(format!("{}_API_KEY", name.to_uppercase().replace('-', "_")))
    .ok().filter(|k| !k.is_empty())
    .or_else(|| std::env::var("LLM_API_KEY").ok().filter(|k| !k.is_empty()));
```

So for the `deepseek` plugin, it checks `DEEPSEEK_API_KEY` first, then `LLM_API_KEY`.

**Response format:**
```json
// POST /api/plugins/deepseek/refresh-models
{
  "success": true,
  "data": {
    "name": "deepseek",
    "config_schema": [
      { "key": "default_model", "label": "Default Model", "type": "enum",
        "allowed_values": ["deepseek-v4-flash", "deepseek-v3", "deepseek-r1", "deepseek-coder"],
        "refresh_url": "https://api.deepseek.com/v1/models" }
    ]
  }
}
```

**Error cases:**
- Plugin not found → `404 Not Found`
- Plugin has no `refresh_url` fields → `400 Bad Request` with message "Plugin has no refresh_url fields"
- Network/parse failure → `500 Internal Server Error`, but the cache keeps the previous values

**Currently used by:**
- `deepseek` (provider) — `refresh_url: "https://api.deepseek.com/v1/models"` + static fallback
- `opencode-go` (provider) — `refresh_url: "https://opencode.ai/zen/go/v1/models"` (no static fallback)

### Hindsight Populator (`hindsight_populator.rs`)
- Located at `src/hindsight_populator.rs`
- Queries new messages from the DB (id > watermark) and retains them into omniagent-hindsight
- Watermark stored at `{data_dir}/hindsight_watermark.json` (JSON with `last_message_id`, `last_run_at`)
- Processes in batches of 200, sub-batches of 50
- Uses `strategy: "fast"` to skip LLM extraction (works offline)
- Tags messages by role, type, and subtype for semantic filtering at recall time
- **Builtin action**: `builtin_hindsight_populator` registered in both DB `actions` table and scheduler dispatch
- **MCP tool**: `actions_hindsight_populator` for manual agent triggering
- **Cron**: Job `hindsight_populator` (every 15 min, `mode=action`, deactivated by default)
- **Recall integration**: `context_builder.rs` calls `POST /v1/default/banks/{bank}/memories/recall` with the user query, injects results as Low-priority block

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

### Planning Mode Resolution

Planning mode is resolved **at thread creation time** and stamped on `threads.planning_mode`.

**Source locations:**
- **Resolution:** `src/db/types.rs` — `resolve_thread_planning_mode()` (simple), `resolve_thread_planning_mode_with_content()` (complexity-based), `classify_complexity_for_planning()` (threshold logic)
- **Max iterations:** `src/db/types.rs` — `max_iterations_for_planning_mode()` maps mode → iteration cap
- **Prompt injection:** `src/prompt_builder.rs` — planning instructions injected based on `thread.planning_mode`
- **Table columns:** `threads.planning_mode` (runtime truth), `channels.planning_mode` (per-channel override), `cron_jobs.planning_mode` (per-job override)

**Modes:**
| Value | Meaning |
|-------|---------|
| `prompt_only` | No planning — LLM responds immediately |
| `auto_plan` | Single planning step before responding |
| `auto_subtasks` | Full subtask decomposition (default) |
| `always` | Legacy alias for `auto_subtasks` |

**Priority chain** (first non-empty wins):
1. Task `planning_mode` — cron job planning mode (highest — overrides channel)
   - Valid values: empty (→ complexity-based default), `prompt_only`, `auto_plan`, `auto_subtasks`
2. Channel `planning_mode` — override for the entire channel
   - Valid values: empty (→ default), `prompt_only`, `auto_plan`, `auto_subtasks`, `never` (→ `prompt_only`), `always` (→ `auto_subtasks`)
3. Kanban tasks — always `resolve_max_plan(global_mode)` (no complexity classification)
4. User/Cron default — `classify_complexity_for_planning()` via content heuristics

**Complexity classification (`classify_complexity_for_planning`):**
- Simple: `char_len < SIMPLE_MAX (60) || word_count ≤ 3 + greeting` → `prompt_only`
- Complex: `char_len > STANDARD_MAX (200) || action keywords match` → `auto_subtasks`
- Standard: `auto_plan` (via global `PLANNING_MODE` env var)

**Env vars:** `PLANNING_MODE`, `PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS`, `PLANNING_COMPLEXITY_STANDARD_MAX_CHARS`, `PLANNING_COMPLEXITY_KEYWORDS` — all adjustable via `/settings` endpoint.

**Iteration caps** per mode (configured in `AgentConfig`):
- `prompt_only` → `max_iterations_no_plan` (default 5)
- `auto_plan` → `max_iterations_simple_plan` (default 10)
- `auto_subtasks`/`always` → `max_iterations_complex_plan` (default 25)

The per-`process_message` cap was previously hardcoded to 12 (`remaining.clamp(0, 12)`). It now uses the full remaining budget from the MAX_ITERATIONS_* settings directly (`remaining.max(0)`), so a single user message can consume all remaining iterations for the thread.

**When the iteration limit is reached**, the thread is marked `interrupted` (not `failed`). Instead of a hardcoded message, the executor calls the LLM to generate a summary that includes:
- The iteration count (`{current_iter}/{iter_limit}`)
- What was accomplished
- What remains to be done

The LLM summary is saved as the only post-loop message (type `summary`, subtype `interrupted`, `is_summary=true`).

### Cron Schedule Format

Cron expressions use **5-field Linux format** (`min hour day month weekday`). The scheduler prepends `"0 "` (second=0) for the `cron` crate (which expects 6-field). Both `create_cron_job` and `update_cron_job` MCP tools validate exactly 5 fields.

Examples:
- `0 * * * *` — every hour
- `*/15 * * * *` — every 15 minutes
- `0 9 * * 1-5` — weekdays at 9am

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
