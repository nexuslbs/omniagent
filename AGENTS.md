# OmniAgent — AGENTS.md

## Guidelines

### Cron / Schedule Mode

Cron jobs have a `mode` field:
- **`agentic`** (default): Creates a thread, and the agent executor processes it with LLM calls.
- **`action`**: Executes a tool directly via the MCP registry. **No thread, no messages, no LLM calls.** The action is resolved from `actions.yml` (or hardcoded built-in).

**NEVER change action-mode schedules to agentic mode.** Action-mode jobs (kanban_dispatcher, hindsight_populator, relevance_indexer) have NO prompt and would create empty/meaningless threads. If you see an action-mode job creating a thread, it's a regression — fix the scheduler to handle `mode='action'` before thread creation.

### Silent Cron Jobs

The `silent` flag (boolean) on cron jobs controls whether the job produces visible output:
- `silent=true`: No thread is created, no messages are saved. Even errors just log — no visible trace in the DB. The job executes and completes silently.
- `silent=false` (default): Normal thread creation, messages, and platform delivery.

**Silent is NOT a substitute for action mode.** Silent agentic jobs would create threads (wasting tokens) — use action mode instead for tool-only jobs.

### Platform Delivery — Unified Logic

All message delivery uses the same code path, regardless of thread origin (user, kanban, cron):

1. **seq-0 (cause message)**: If the thread's channel has `platform` and `resource_identifier` set, the cause message is delivered to that platform as a new message/post. For user threads, this comes FROM the platform (message received). For system threads, this is posted TO the platform (message sent as bot).

2. **seq-1+ (responses)**: All messages use `cause_external_id` for threading. If the cause message has an external_id, subsequent messages reply in the platform thread. If no external_id, they go as new posts.

3. **No platform = no delivery**: If the channel lacks `platform` or `resource_identifier`, no delivery happens. No special-casing for thread type.

**Key principle**: The channel IS the delivery target. If a kanban task or cron job's channel has `platform='mattermost'` and `resource_identifier='abc'`, messages go to Mattermost automatically. No kanban/cron-specific resolution is needed.

### `enqueue_delivery` — DO NOT add non-user special cases
The `enqueue_delivery` function in `src/agent/helpers.rs` was historically full of special cases for non-user threads (skipping delivery for system threads). **These are now removed.** ALL messages follow the same path:
- Channel has platform + resource_identifier → deliver
- Channel has no platform → skip
- Tool results → skip (never delivered to platforms)

If someone needs to change delivery behavior, modify the channel's platform/resource_identifier, not the delivery code.

### Run-Cron Endpoint
`POST /run-cron/{schedule_id}` triggers a cron job immediately. Returns proper HTTP errors:
- **200** with `{ schedule_id, thread_id: null }` for action/silent jobs
- **200** with `{ schedule_id, thread_id: <id> }` for agentic jobs
- **404** when the job doesn't exist
- **409** when the job is inactive (use `?force=true` to override)
- **500** for other errors

The endpoint goes through `fire_cron_job_by_id()` in `scheduler.rs`.

### NEVER modify the database directly — always use the API

**When sending messages or creating threads, NEVER use `psql`, `sqlx`, `sql_forge`, `INSERT INTO` or any other direct database modification.** Always use the HTTP API (port 3001):

- `POST /api/kanban/tasks` — create a kanban task (set `status: "ready"` to trigger agent execution)
- `POST /api/schedule` — create a cron job
- Use `curl` against `http://localhost:12346/api/...` (dashboard proxy → omniagent)

Direct DB writes bypass the agent's state machine, timestamp tracking, and error handling. The API is the single source of truth for state changes.

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
4. Resolves API key: from the provider's resolved plugin config (`detail.config.api_key`), no hardcoded env var names
5. Calls `fetch_enum_values(url, api_key)` — GET request with 5s timeout, Bearer auth if key present
6. Parses response as `{data: [{id: "model-name"}, ...]}` (OpenAI `/v1/models` format)
7. On success: updates the field's `allowed_values` + populates the in-memory cache
8. On failure: logs warning, preserves existing `allowed_values` (no breaking change)
9. Returns `Some(detail)` if any field had a refresh_url, `None` otherwise

**API key resolution logic:**
The API key is read from the provider's resolved plugin config (`detail.config.get("api_key")`), which already resolves `$env:` references defined by the user in `providers.yml`. No hardcoded env var names are used.

```rust
let api_key = detail.config
    .get("api_key")
    .and_then(|v| v.as_str())
    .filter(|s| !s.is_empty())
    .map(|s| s.to_string());
```

So for the `deepseek` plugin, the key comes from `providers.yml`:
```yaml
deepseek:
  enabled: true
  config:
    api_key: "$env:DEEPSEEK_API_KEY"
```

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
- Summary generation uses a separate LLM call with `CHANNEL_SUMMARY_TOKENS` max tokens (default 4096)
- Per-thread end-of-execution summaries use `THREAD_SUMMARY_TOKENS` (default 2048)
- Old summaries are deleted alongside old messages via the daily cleanup task
- Config env vars: `SUMMARY_WINDOW` (default 10), `CHANNEL_SUMMARY_TOKENS` (default 4096), `THREAD_SUMMARY_TOKENS` (default 2048), `DELETE_AFTER_DAYS` (default 30)

### Planning Mode Resolution

Planning mode is resolved **at thread creation time** and stamped on `threads.planning_mode`.

**Source locations:**
- **Resolution:** `src/db/threads.rs` — `resolve_thread_planning_mode_with_content()` (core logic), `classify_complexity_for_planning()` (threshold logic), `resolve_cron_planning_mode()`, `resolve_max_plan()`
- **Max iterations:** `src/db/threads.rs` — `max_iterations_for_planning_mode()` maps mode → iteration cap
- **Prompt injection:** `src/prompt_builder.rs` — planning instructions injected based on `thread.planning_mode`
- **Table columns:** `threads.planning_mode` (runtime truth), `channels.planning_mode` (per-channel override), `cron_jobs.planning_mode` (per-job override)

**Modes:**

| Value | Meaning |
|-------|---------|
| `prompt_only` | No planning — LLM responds immediately |
| `auto_plan` | Single planning step before responding |
| `auto_subtasks` | Full subtask decomposition (only when explicitly configured — see below) |
| `always` | Legacy alias for `auto_subtasks` |

**When is `auto_subtasks` available?**

`auto_subtasks` (full subtask decomposition) is **not** the default. It is only available when explicitly configured in one of these ways:

- **Global `PLANNING_MODE` env var** set to `auto_subtasks` or `plan with subtasks`
- **Channel** `planning_mode` set to `auto_subtasks` or `always`
- **Cron job** `planning_mode` set to `plan_with_subtasks` or `auto_subtasks`
- **Kanban tasks** — always use the max plan mode derived from the global `PLANNING_MODE` (so if global is `auto_subtasks`, kanban gets `auto_subtasks`)
- **Task-level explicit override** (for cron jobs and kanban tasks)

If none of these explicitly enables it, the complexity-based classification caps at `auto_plan` — it will never spontaneously promote to `auto_subtasks`.

**Priority chain** (first non-empty wins):

1. **Cron task** `planning_mode` — highest priority, overrides channel and global
   - Valid values: empty (→ complexity-based default), `no_plan` (→ `prompt_only`), `simple_plan` (→ `auto_plan`), `plan_with_subtasks` (→ `auto_subtasks`), `max_plan` (→ `resolve_max_plan(global_mode)`), or direct canonical values
2. **Channel** `planning_mode` — override for the entire channel
   - Valid values: empty (→ default), `prompt_only`, `auto_plan`, `auto_subtasks`, `never` (→ `prompt_only`), `always` (→ `auto_subtasks`)
3. **Kanban tasks** — always `resolve_max_plan(global_mode)` (no complexity classification)
4. **User / Cron default** — `classify_complexity_for_planning()` via content heuristics (see below)

**Complexity classification (`classify_complexity_for_planning`):**

The classifier evaluates prompt content against threshold heuristics and returns a canonical planning mode. The outcome is **capped by the resolved planning mode context** — `auto_subtasks` is only returned when the global `PLANNING_MODE` or an explicit task/channel setting has enabled it.

| Complexity Level | Criteria | Resulting Mode |
|---|---|---|
| **Simple** | `char_len < SIMPLE_MAX (60)` or `word_count ≤ 3 + greeting` | `prompt_only` — no planning needed |
| **Standard** | Everything between Simple and Complex | `auto_plan` — single planning step |
| **Complex** | `char_len > STANDARD_MAX (200)` or action keywords match | `auto_subtasks` **iff** the resolved planning mode context permits it (global `PLANNING_MODE` is `auto_subtasks`, or an explicit task/channel/cron setting enables it); otherwise caps at `auto_plan` |

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

### Actions Feature (Saved Tool Invocations)

The term "actions" is used in two distinct contexts — do not confuse them:

### 1. Saved Actions (Dashboard Pages / HTTP API)

Saved Actions are parameterized tool invocations stored in `{data_dir}/actions.yml`. They let users save a tool name + arguments as a reusable function that can be triggered from the dashboard or associated with a cron job.

**HTTP API** (backed by YAML file, not database):
| Method | Route | Description |
|--------|-------|-------------|
| `GET` | `/actions` | List all saved actions |
| `POST` | `/actions` | Create a new action |
| `PUT` | `/actions/{id}` | Update an action |
| `DELETE` | `/actions/{id}` | Delete an action |
| `POST` | `/actions/{id}/run` | Execute a saved action via the MCP registry |

**Source:** `src/server/actions.rs` — reads/writes YAML file atomically with `.tmp` rename.

**YAML format** (`{data_dir}/actions.yml`):
```yaml
actions:
  a6:
    enabled: true
    tool_name: delete_subtask
    params:
      subtask_id: 1
  builtin_kanban_dispatcher:
    enabled: true
    tool_name: kanban_dispatcher
    params: {}
    description: Pick up pending kanban tasks and create agent threads
    is_builtin: true
```

- Each action has a string ID (the YAML key), a `tool_name` matching a registered MCP tool, and `params` (object).
- Built-in actions (flagged `is_builtin: true`) are protected from deletion in the UI.
- The YAML file is the authoritative source for all actions, replacing the old database-backed `actions` table (Phase 13 migration).

**Cron job integration:** Cron jobs can reference a saved action via `action_id`. When `mode=action`, the scheduler executes the saved action's tool directly instead of creating an agent thread.

### 2. "actions" MCP Toolset (External MCP Server)

The `actions` MCP server (`plugins/mcp/actions/`) is an external stdio MCP server that provides 4 built-in tools often used within saved actions:
- `kanban_dispatcher` — process pending kanban tasks
- `hindsight_populator` — retain messages into hindsight memory
- `relevance_indexer` — update wiki relevance index
- `setup_knowledge_pipeline` — create knowledge pipeline cron job

The name "actions" for this toolset is arbitrary; it simply indicates the tools are designed to be called from saved actions or cron jobs. These 4 tools are implemented as a separate Rust binary (`mcp-server-actions`) launched as a subprocess, not built into the main omniagent binary.

**See also:** `{data_dir}/plugins/mcp/actions/mcp-config.json` for server configuration.

## Cron Schedule Format

Cron expressions use **5-field Linux format** (`min hour day month weekday`). The scheduler prepends `"0 "` (second=0) for the `cron` crate (which expects 6-field). Both `create_cron_job` and `update_cron_job` MCP tools validate exactly 5 fields.

Examples:
- `0 * * * *` — every hour
- `*/15 * * * *` — every 15 minutes
- `0 9 * * 1-5` — weekdays at 9am

### Channel Templates

Channels can have a `template` field (TEXT, stored in `channels.template`). When set:

1. **User messages**: The template name is injected into the seq-0 message metadata under `"template"`. The agent executor loads the template content from `profiles/<name>/templates/<name>.md` and injects it as a `=== Task Template ===` block into the system prompt.

2. **Cron jobs**: If the cron job has no `template` set, the channel's `template` is used as fallback.

3. **Kanban tasks**: If the kanban task has no `template` set, the channel's `template` is used as fallback.

**Priority order**: Task-level template → channel template → no template.

### Seq-0 Message Types

Every thread starts with a seq-0 (cause) message. The `msg_type` and `msg_subtype` fields identify the origin:

| Source | `msg_type` | `msg_subtype` | Thread `cause` |
|--------|-----------|---------------|-----------------|
| User message | `Cause` | Platform name (e.g., `mattermost`) | `user` |
| Cron job (scheduled) | `cron` | Cron job name | `system` |
| Cron job (manual run) | `cron` | Cron job name | `user` |
| Kanban task | `kanban` | Kanban task ID | `system` |

The `msg_type` controls template loading in the executor (templates load for `Cause`, `cron`, and `kanban` types).

### Provider/Model Stamping and Validation

**Stamping at creation time** — When any seq-0 message is created (user message, cron job, kanban ready-task), `provider` and `model` are resolved and stamped on the **thread** using this chain:
1. Channel `current_provider` / `current_model`
2. Profile `provider` / `model`
3. `LLM_PROVIDER` env var (model from provider plugin's `default_model`)
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
-- repeat for 'system' cause (used for cron/kanban tasks)

-- Add cause messages
INSERT INTO messages (thread_id, role, content, thread_sequence, msg_type)
VALUES (<thread_id>, 'cause', 'your prompt', 0, 'message');  -- or if cron/kanban

-- Set pending
UPDATE threads SET status = 'pending' WHERE channel_id = <ch_id> AND status = 'created';
```

**Expected:** Threads processed **sequentially** (one after another) in the same channel. All complete with cause and msg_type preserved.

#### Test 2: Different channels (parallelism)
```sql
-- Insert threads in DIFFERENT channels
INSERT INTO threads (...) VALUES ('created', 'user',  <ch_a>, ...);
INSERT INTO threads (...) VALUES ('created', 'system', <ch_b>, ...);
INSERT INTO threads (...) VALUES ('created', 'system', <ch_c>, ...);
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

### block_in_place Anti-Pattern

`tokio::task::block_in_place()` blocks the calling thread, preventing the tokio worker from making progress on other tasks. This should be avoided in async code.

**Affected location:**

`plugins/mcp/util/src/lib.rs:324` — `handle_tools_call()` in `mcp_server_util`:

```rust
let (text, is_error) = match tokio::task::block_in_place(|| (entry.handler)(args)) {
```

This wraps a synchronous handler call in `block_in_place()` inside an async function. The handler type is `Box<dyn Fn(&Value) -> Result<(String, bool)> + Send + Sync>` — a synchronous closure.

**Why it's problematic:**
- `block_in_place()` tells tokio to hand off the current worker to a replacement thread, but the sync handler still runs on the same OS thread, preventing that worker from polling other tasks.
- If the sync handler blocks on I/O (e.g., a database query or HTTP call), the entire worker thread is stalled.
- The `block_in_place()` primitive exists for bridging sync code into async — but only when the sync code will block for less than a few microseconds. Long-running sync handlers stall the runtime.

**The fix:**

Use **async handlers** (`BoxFuture`) instead of synchronous closures, allowing the MCP server to `await` the handler without blocking:

```rust
// Async handler type:
pub type AsyncToolHandler = Box<dyn Fn(Value) -> BoxFuture<'static, Result<(String, bool)>> + Send + Sync>;

// In handle_tools_call:
let (text, is_error) = match (entry.handler)(args).await {
    Ok(result) => result,
    Err(e) => { ... }
};
```

Alternatively, if the handler must remain synchronous, wrap it with `tokio::task::spawn_blocking()` instead of `block_in_place()`:

```rust
let (text, is_error) = match tokio::task::spawn_blocking(move || (entry.handler)(args)).await {
    Ok(Ok(result)) => result,
    Ok(Err(e)) => { ... }
    Err(e) => { ... }  // panic in handler
};
```

**Note:** The main `McpRegistry::execute()` method in `src/mcp/mod.rs:193` already uses the correct pattern (`spawn_blocking`). Only `mcp_server_util::handle_tools_call()` uses `block_in_place`.

### MCP Server Timeouts

External MCP servers have multiple timeout layers configured through `McpServerConfig` (defined in `src/mcp/external/config.rs`):

| Timeout Layer | Default | Config Field | Description |
|---|---|---|---|
| **Up / Restart** | 300s (5 min) | _(process lifecycle)_ | Time to wait for an MCP server subprocess to start up and respond to the `initialize` handshake. If the server doesn't respond within this window, it's considered dead and gets restarted. |
| **Exec / Run** | 600s (10 min) | `timeout_secs` | Time to wait for a single tool call (`tools/call`) to complete. Long-running tools (e.g., filesystem operations, Docker containers, searches) may need this. |
| **Circuit breaker cooldown** | 30s | _(hardcoded)_ | After N consecutive failures (default 3), the circuit breaker opens. After 30s it transitions to HalfOpen, allowing one probe request. |

**Specifiable by the agent:** The `timeout_secs` field in `McpServerConfig` is configurable per-server via the `mcp-servers.json` config file:

```json
{
  "servers": [
    {
      "name": "my-server",
      "transport": "stdio",
      "command": "python3",
      "args": ["server.py"],
      "timeout_secs": 600,
      "max_retries": 5
    }
  ]
}
```

The config file can be specified via `MCP_SERVERS_CONFIG` env var or placed at `<data_dir>/config/mcp-servers.json`. Environment variable references (`${VAR}` and `${VAR:-default}`) are supported in all string fields.

For **built-in MCP plugins** (those under `plugins/mcp/`), their `mcp-config.json` files are auto-discovered and merged. These plugins use the `mcp_server_util` framework (see block_in_place anti-pattern above) which reads requests via async stdin and dispatches to sync handlers. The timeout for these built-in plugins is inherited from the parent omniagent process's lifecycle — there is no per-call timeout within the plugin itself.

## Docker & Deployment Pitfalls

### ⚠️ Container filesystem path mismatch

The `filesystem` MCP tool and `compose` MCP tool operate in different path namespaces.

The omniagent Docker container mounts volumes that remap paths:
```
Host path                       Container path
/opt/workspace/omniagent        /app
/opt/workspace/omni-workspace   /opt/workspace   ← filesystem writes go here
/opt/workspace/omni-stack       /opt/data
```

**Critical effect:** When `filesystem` writes to `/opt/workspace/playground/...`, the bytes land at `/opt/workspace/omni-workspace/playground/...` on the host. But `compose(project_dir="/opt/workspace/playground/...")` looks at the ACTUAL host path `/opt/workspace/playground/...`, which does NOT contain the files.

**Rule:** Before writing files that will later be deployed via `compose`, verify the container's mount map:
```
docker inspect omni-stack-omniagent-1 --format '{{range .Mounts}}{{.Source}} -> {{.Destination}}{{"\n"}}{{end}}'
```
Write files to a path whose container-side `.Destination` corresponds to a `.Source` that `compose` can reach on the host. Always use verified paths, not assumptions.

### ⚠️ Port-in-use detection is container-scoped

`fetch http://localhost:PORT/` from inside the omniagent container only checks ports INSIDE the omniagent container's network namespace. A container like `repo-web-1` may have `0.0.0.0:12347->5173/tcp` on the HOST but be unreachable from inside the omniagent container's localhost.

**Always use these methods to check host port availability:**
```
docker ps --format "table {{.Names}}\t{{.Ports}}" | grep ":PORT_NUMBER"
```
The Docker socket is available at `/var/run/docker.sock` inside the omniagent container, so `docker ps` shows real host port mappings.

### ⚠️ Compose files don't auto-deploy

Writing a `docker-compose.yml` via `filesystem_write` does NOT deploy it. Always explicitly call:
```
compose(project_dir="<verified-host-path>", command="up", args="-d")
```

### Platform Plugin `message_deleted` Notification

Platform plugins can send a `message_deleted` notification to the omniagent when a user deletes a message:

```json
{"method": "message_deleted", "params": {"resource_identifier": "<channel-id>", "external_id": "<deleted-post-id>"}}
```

The omniagent's client.rs handles this:
1. Looks up the thread whose seq-0 (cause) message has matching `external_id` AND belongs to the channel matching `resource_identifier`
2. If the thread is `pending` or `processing` → marks it `skipped` + `terminal` (the agent stops processing it)
3. If the thread is already terminal → does nothing
4. If the message is NOT the seq-0 (cause) message → does nothing (only thread-root deletion stops the agent)

**Mattermost plugin:** The WebSocket event handler detects `post_deleted` events and sends this notification automatically. Polling mode currently does NOT detect post deletions — use WebSocket mode (`connection_mode: websocket` in platforms.yml) for this feature. Platform-specific settings like connection mode are configured via the platform config (platforms.yml or dashboard UI), not in .env.
Then verify with `docker ps` and `curl http://localhost:PORT/`.
