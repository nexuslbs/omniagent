# OmniAgent

Next-generation agent system built with Rust, PostgreSQL + pgvector, and MCP tool support.

## Features

| **Hindsight Memory** | Persistent cross-session memory via omniagent-hindsight, with automatic population from new messages and semantic recall in context assembly |
| **Hindsight Populator** | Background action (deactivated by default) that retains messages into hindsight every 15 minutes. Activate via `UPDATE cron_jobs SET active = true WHERE id = 'hindsight_populator'` |

### 🧠 Context Builder & Grounding
- **Priority-ranked prompt assembly** (`ContextBuilder`) — NeverTrim (system, MEMORY.md, subtasks) → High (thread messages) → Normal (tool defs) → Low (retrieved content)
- **Token budgeting** — per-block character caps, lowest-priority blocks dropped when over budget
- **Grounding policy** — embedded in every system prompt: prefer evidence, state uncertainty, cite references
- **Evidence metadata** — `messages.metadata` captures context diagnostics (`context.selected_message_ids`, `block_counts`, `dropped_blocks`, `total_chars`) and grounding flags

### 🔍 Hybrid Retrieval
- **4-tier retrieval** controlled by profile `retrieval_aggressiveness` (0-3):
  - Level 1: ILIKE text search in messages + wiki text search (walkdir)
  - Level 2+: pgvector semantic search (`<=>` cosine similarity on message embeddings) + Qdrant vector search on wiki content
- **Query classifier** — heuristic (Greeting/Command/FollowUp/Factual/ExternalQuery) gates whether retrieval runs
- Re-ranking with recency and same-thread boosts

### 💾 Memory Promotion
- **3 MCP tools** (`promote_to_memory`, `list_memories`, `review_memories`)
- YAML frontmatter with `confidence`, `source_message_ids`, `source_tool_outputs`, `created_at`, `expires_at`, `last_verified_at`
- 30-day default expiry with review workflow

### 🔄 Dynamic Enum Refresh (`refresh_url`)

Provider plugins can define a `refresh_url` on `enum` type `config_schema` fields to dynamically fetch model options from an external API at runtime, rather than relying on a static `allowed_values` list.

**How it works:**

1. **Plugin definition** — a `ConfigSchemaField` with `type: "enum"` and a `refresh_url` pointing to an OpenAI-compatible `/v1/models` endpoint:
   ```json
   { "key": "default_model", "label": "Default Model", "type": "enum", "refresh_url": "https://api.deepseek.com/v1/models" }
   ```

2. **On-demand refresh** — `POST /api/plugins/{name}/refresh-models` fetches models from the URL, parses `{data: [{id: "model-name"}, ...]}` responses, and updates an in-memory cache.

3. **In-memory cache** — `DYNAMIC_ENUM_CACHE` (Mutex\<HashMap\<String, DynamicEnumEntry\>\>) with a 5-minute TTL. Cache is checked when enriching plugin data for API responses (`enrich_plugin()`).

4. **API key resolution** — for authenticated endpoints, the key is resolved as `{PLUGIN_NAME}_API_KEY` → `LLM_API_KEY` environment variable, sent as a `Bearer` token.

5. **Graceful fallback** — if the fetch fails, existing `allowed_values` are preserved (either hardcoded fallbacks in `plugin.json` or the previous cache entry).

**Currently used by:**
- **deepseek** — `refresh_url: "https://api.deepseek.com/v1/models"` with static fallback `["deepseek-v4-flash", "deepseek-v3", "deepseek-r1"]`
- **opencode-go** — `refresh_url: "https://opencode.ai/zen/go/v1/models"` (no static fallback)

### 🔌 MCP External Servers
- **stdio transport** — spawn subprocesses, JSON-RPC 2.0 over stdin/stdout
- **HTTP transport** — connect to remote MCP servers via HTTP POST
- **Circuit breaker** — automatic disable after N consecutive failures
- **Dynamic tool registry** — external tools auto-merge with built-in tools at startup
- Configured via `MCP_SERVERS_CONFIG` env var or `<data_dir>/config/mcp-servers.json`

### 📋 Thread Subtasks

Thread subtasks enable the LLM to decompose a complex request into trackable sub-items. Subtasks are stored in the `thread_subtasks` table and managed via the `manage_subtasks` MCP tool.

**Tool: `manage_subtasks`**
- Actions: `add`, `list`, `update`, `delete`, `get_counts`
- Each subtask has: `id`, `thread_id`, `description`, `status` (pending/completed/cancelled), `priority`
- Returns structured JSON with `current_subtask`, counts per status, and full subtask list

**Current Subtask Logic:**
- The first pending subtask (ordered by `priority DESC`, `created_at ASC`) is the "current" subtask
- When all subtasks are completed/cancelled, `current_subtask` is `null`
- This drives the prompt injection — only the current subtask is prominently displayed

**Prompt Injection:**
- When subtasks exist, a `[Thread Subtasks]` section is injected into the system prompt (NeverTrim tier)
- Shows current subtask with status emoji, and remaining subtask count
- Only injected when there are active (non-cancelled) subtasks — empty threads see no section

**Override Pattern:**
- To redefine a thread's subtasks, delete all existing ones (`action: delete` for each) then add new ones
- Bulk updates supported via SQL-level operations (e.g., mark all as completed)

### Requirements

- Docker & Docker Compose
- An LLM API key (OpenCode Go, OpenAI, Anthropic, or DeepSeek)

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
   - `DATABASE_URL` — PostgreSQL connection string (default: `postgres://omniagent:***@postgres:5432/omniagent`)

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

## Channels

Channels represent communication endpoints. Each channel has its own state, profile, and model configuration. The agent processes messages **sequentially within a channel** but **in parallel across channels**.

### Channel Fields

| Field | Description |
|-------|-------------|
| `name` | Human-readable channel name |
| `platform` | Platform identifier (e.g., `telegram`, `api`, `cron`) |
| `external_id` | Platform-specific address (chat ID, channel name, etc.) |
| `resource_identifier` | Canonical resource address — used in (platform, resource_identifier) unique constraint |
| `current_profile` | Profile to use for message processing |
| `current_provider` | Provider override (overrides profile) |
| `current_model` | Model override (overrides profile) |
| `closed` | Boolean (default `false`). A closed channel retains history but **won't process new messages** |
| `readonly` | Boolean (default `false`). Protects the channel from deletion |

### Creating a Channel

```sql
INSERT INTO channels (name, platform, external_id, resource_identifier, cause, current_profile)
VALUES ('my-channel', 'api', 'my-channel-1', 'my-channel-1', 'user', 'default');
```

Each channel can set a custom profile, provider, and model:
```sql
UPDATE channels SET current_profile = 'research', current_provider = 'anthropic', current_model = 'claude-sonnet-4' WHERE id = 1;
```

### Cron Channel

Every OmniAgent instance has a default cron channel (platform='cron', name='cron-default') created automatically. This channel is used as the fallback destination for cron jobs and kanban tasks that don't specify a channel. It is marked as `readonly=true` to prevent accidental deletion.

### Readonly Channels

Channels can be marked as `readonly` (e.g., the default cron channel) to protect them from deletion:
```sql
ALTER TABLE channels ADD COLUMN readonly BOOLEAN NOT NULL DEFAULT false;
```

### Closed Channels

Channels can be marked as `closed` (boolean, default `false`). A closed channel:
- Retains all message history
- Does **not** process new messages (they remain pending)
- Can be reopened by setting `closed = false`

```sql
ALTER TABLE channels ADD COLUMN closed BOOLEAN NOT NULL DEFAULT false;
```

### Channel Subscriptions

The `channel_subscriptions` table enables cross-platform listening:

| Field | Description |
|-------|-------------|
| `channel_id` | The channel that receives updates |
| `subscriber_platform` | Platform of the subscriber |
| `subscriber_resource` | Resource identifier of the subscriber |

A Telegram channel can subscribe to another channel's summaries — when a summary is generated, it's forwarded to the subscriber. The unique constraint is `(channel_id, subscriber_platform, subscriber_resource)`.

```sql
INSERT INTO channel_subscriptions (channel_id, subscriber_platform, subscriber_resource)
VALUES (1, 'telegram', 'my-telegram-chat');
```

## Profiles

Profiles bundle model configuration, provider, and allowed tools. A `default` profile is created on first startup.

Profile fields:
- **provider** — LLM provider (e.g., `opencode-go`, `openai`, `anthropic`, `deepseek`)
- **model** — LLM model name (e.g., `deepseek-v4-flash`, `claude-sonnet-4`)
- **allowed_tools** — which MCP tools the agent can use

### Creating a Profile

```sql
INSERT INTO profiles (name, provider, model, allowed_tools)
VALUES (
  'research',
  'anthropic',
  'claude-sonnet-4',
  '["filesystem_read", "filesystem_write", "fetch", "search_messages", "search_wiki"]'
);
```

### Profile vs Channel Priority

The effective model and provider are resolved as:
1. **Message** `profile` (highest) — set per-message for cron/kanban tasks
2. **Channel** `current_profile` / `current_model` / `current_provider`
3. **Profile** `model` / `provider`
4. Environment defaults
5. Built-in fallbacks

If neither the channel nor the profile specifies a model, the prompt will fail with an error.

## Execution Model

### Sequential Per Channel, Parallel Across Channels

The agent runs a **supervisor loop** that:
1. Lists all channels from the database
2. Spawns a dedicated `channel_handler` task for each channel that isn't already running
3. Each `channel_handler` independently polls its channel for pending messages
4. Within a channel, messages are processed one at a time (FIFO order)
5. Across channels, processing happens in parallel

```
┌─────────────────────────────────────────────────┐
│  Supervisor Loop (every 5 sec)                   │
│                                                   │
│  ├── Channel A ── handler ── msg₁ ── msg₂ ── ... │
│  ├── Channel B ── handler ── msg₁ ── msg₂ ── ... │
│  ├── Channel C ── handler ── msg₁ ── msg₂ ── ... │
│  └── cron/kanban ── handler ── msg₁ ── msg₂ ... │
└─────────────────────────────────────────────────┘
```

### Message Lifecycle

```
User inserts a message (status = pending)
  │
  ▼
Agent picks it up, marks as processing
  │
  ├─ LLM responds with text → saved as msg_type='message'
  ├─ LLM includes reasoning → saved as msg_type='reasoning' (separate row)
  ├─ LLM plans next step → saved as msg_type='plan'
  ├─ LLM calls tools in parallel → saved as msg_type='multi-tool'
  └─ LLM calls tools → tool executed, result fed back, loop continues
  │
  ▼
Prompt marked as completed, processing_time_ms and token_usage set
```

### Message Types

| `msg_type` | Description |
|------------|-------------|
| `message` | Standard user or assistant message |
| `cron` | Cron-triggered message |
| `kanban` | Kanban-triggered message |
| `tool` | Tool invocation |
| `tool_result` | Tool execution result |
| `reasoning` | LLM reasoning/thinking content |
| `summary` | Thread summary |
| `plan` | LLM planning or reasoning step |
| `multi-tool` | Parallel tool calls from the LLM |
| `error` | Processing error (see `msg_subtype` for error codes) |

### Error Subtypes

| `msg_subtype` | Description |
|---------------|-------------|
| `no-profile` | Profile field is empty |
| `no-provider` | Provider field is empty |
| `no-model` | Model field is empty |
| `invalid-profile` | Profile does not exist in the registry |

### Per-Message Timing and Token Usage

Each message stores its own timing and token data:

- **`processing_time_ms`**: Wall-clock time spent processing this message (stored per-message, not thread-level)
- **`token_usage`**: JSONB object with:
  - `prompt_tokens` — tokens in the prompt
  - `completion_tokens` — tokens in the completion
  - `cached_tokens` — tokens served from cache (if supported by provider)
  - `reasoning_tokens` — tokens used for reasoning/thinking (if supported)

```json
{
  "prompt_tokens": 1523,
  "completion_tokens": 412,
  "cached_tokens": 0,
  "reasoning_tokens": 89
}
```

### Startup Cleanup

On startup, the agent runs `skip_on_startup()` which marks all messages with status `pending` or `processing` as `skipped`. This prevents messages from being stuck indefinitely after a container restart.

### Profile Resolution at Message Time

When a message is created (seq-0), the `provider` and `model` fields are **stamped** on the message using this resolution chain:

1. **Message** `profile` field (highest priority) — set per-message for cron/kanban tasks
2. **Channel** `current_provider` / `current_model` / `current_profile`
3. **Profile** `provider` / `model` (if set in the profile)
4. **Environment variable** `LLM_PROVIDER` (model comes from provider plugin's `default_model`)
5. **Built-in defaults** `opencode-go` / `deepseek-v4-flash`

This happens at creation time for:
- **User messages**: provider/model are stamped when the message is inserted
- **Cron jobs**: provider/model are resolved and stamped by the cron scheduler
- **Kanban tasks**: when a task is moved to 'ready' status, provider/model are resolved and stamped

### Provider/Model Validation at Execution Time

When the agent picks up a pending message for processing, it **validates** the stamped fields before calling the LLM:

1. `profile` must be non-empty → fails with `msg_type='error'`, `msg_subtype='no-profile'`
2. Profile must exist in the registry → fails with `msg_subtype='invalid-profile'`
3. `provider` must be set and non-empty → fails with `msg_subtype='no-provider'`
4. `model` must be set and non-empty → fails with `msg_subtype='no-model'`

If validation fails, an error message is inserted into the thread and the original message is marked as `failed`. The agent uses **only** the stamped values — no fallback chain is run during execution.

For **cron jobs**: profile comes from the cron job's `profile` field, or the channel's `current_profile` if NULL
For **kanban tasks**: profile comes from the task's `profile` field, or the channel's `current_profile` if NULL
For **user messages**: profile comes from the channel's `current_profile` at message creation time

## Cron Jobs

Cron jobs are scheduled tasks that execute on a recurring schedule. Each job can target a specific channel and profile.

### Creating a Cron Job

```sql
-- Via MCP tool (recommended)
-- Use the create_cron_job tool with optional channel_id and profile params

-- Or directly in SQL:
INSERT INTO cron_jobs (id, name, display_name, schedule, prompt, channel_id, profile)
VALUES ('cron_abc123', 'hourly-report', 'Hourly Report', '0 * * * *', 'Generate the hourly report', 1, 'research');
```

### Fields

| Field | Description |
|-------|-------------|
| `channel_id` | Channel to fire in (NULL = default cron channel) |
| `profile` | Profile to use (NULL = channel's current_profile) |
| `schedule` | 5-field Linux cron expression (min hour day month weekday) — the scheduler internally prepends `0` (second=0) for the `cron` crate |
| `prompt` | The message content to execute |
| `mode` | Execution mode: `agentic` (default), `direct`, or `action` |
| `direct_task_type` | Task type for `direct` mode (e.g., `kanban_dispatcher`) |
| `action_id` | Action ID for `action` mode — references the `actions` table |
| `enabled` | Whether the job is active |
| `active` | Whether the job is currently claimed by a scheduler |

### Execution Modes

- **`agentic`** (default): Normal cron agent execution — the prompt is sent to the LLM for processing, with full tool access and reasoning. When `instruction_file` is set, the template content is injected as a "Task Template" block before the prompt.
- **`action`**: Executes a registered action from the `actions` table (user-defined or built-in). The action's MCP tool is called with its saved parameters. No LLM call is made — the action runs as a direct Rust function or MCP tool invocation. Optional `silent` mode suppresses thread creation on success (only creates error threads).

### Cron Planning Mode

Cron jobs support the same planning modes as channels, selectable from the dashboard UI:

| Value | Resolution | Use Case |
|-------|-----------|----------|
| Empty (Default) | Complexity-based classification | Simple prompts don't waste tokens on planning |
| `prompt_only` | No planning or subtasks | Scripted prompts that don't need decomposition |
| `auto_plan` | Single planning step | Moderate prompts needing one planning pass |
| `auto_subtasks` | Full subtask decomposition | Complex multi-step pipelines (e.g., Knowledge Pipeline) |

Cron planning mode has **highest priority** in the resolution chain: cron job → channel → kanban → default.

The planning mode is resolved at thread creation time via `resolve_thread_planning_mode_with_content()` and stamped on `threads.planning_mode`. For backward compatibility, the `max_plan` value is still accepted and resolves to the maximum plan mode enabled globally.

### Knowledge Pipeline

The Knowledge Pipeline is a periodic maintenance cron that runs 6 steps:

1. **Per-channel summarization** — cross-thread summaries for channels with enough new completed threads
2. **Wiki/skill update from messages** — groups completed threads by profile, extracts durable knowledge, updates wiki pages and skills
3. **Wiki relevance indexing** — scores wiki files by recency and reference count, updates `relevant-index.md`
4. **Skill relevance indexing** — same scoring for skill files, writes `relevant-skills-index.md`
5. **Hindsight population** — batch-retains new messages into omniagent-hindsight (skipped if disabled)
6. **Hindsight consolidation** — triggers the consolidation pipeline (skipped if disabled)

**Setup:** Run the `Setup Knowledge Pipeline` action (built-in, idempotent). Creates a cron job with:
- Schedule: `0 */6 * * *` (every 6 hours, configurable)
- Mode: `agentic`
- Planning mode: `max_plan` (enables subtask decomposition)
- Instruction file: `knowledge-pipeline.md` (templates in `profiles/<name>/templates/`)

The template is loaded from `<data_dir>/profiles/default/templates/knowledge-pipeline.md` and injected as a task template into the agent's prompt. Sub-task mode (`auto_subtasks`) ensures each step is tracked; errors on individual steps don't abort the entire pipeline (use the `error` subtask status).

### Scheduler

The cron scheduler runs as a background tokio task, polling every 30 seconds. When a job is due:
1. The job is atomically claimed (with stale-lock detection after 10 minutes)
2. The target channel is resolved (job's channel_id or default cron channel)
3. The profile is resolved (job's profile or channel's current_profile)
4. A pending seq-0 system message is inserted with `msg_type='cron'`
5. The message's `profile` field is set to the resolved profile
6. The job's timestamps are updated

Concurrency is enforced at the DB level: `UPDATE ... WHERE NOT running` ensures only one scheduler instance fires each job.

## Kanban Tasks

Kanban tasks provide a structured workflow. Tasks can be assigned to channels and when moved to 'ready' status, they trigger execution.

### Creating a Kanban Task

```sql
-- Via MCP tool (recommended)
-- Use the create_kanban_task tool with optional channel_id and profile params

-- Or directly in SQL:
INSERT INTO kanban_tasks (id, title, body, status, channel_id, profile)
VALUES ('task_abc123', 'Research topic', 'Find latest papers on...', 'todo', 1, 'research');
```

### Task Lifecycle

1. Task is created (typically in `backlog` or `todo` status)
2. Task is updated to `ready` status
3. The system automatically creates a pending seq-0 message in the task's channel
4. The agent picks up the message and processes it
5. After completion, the task can be moved to `review` or `done`

### Statuses

| Status | Description |
|--------|-------------|
| `backlog` | Not yet prioritized |
| `todo` | Ready to be worked on |
| `ready` | Triggers execution (creates a pending message) |
| `running` | Currently being executed |
| `review` | Waiting for review/approval |
| `done` | Completed |
| `blocked` | Blocked by something |

### Kanban Dispatcher

When a cron job is configured with `mode='direct'` and `direct_task_type='kanban_dispatcher'`, it acts as a **kanban dispatcher**. On each tick:

1. Queries all kanban tasks with `status = 'todo'`
2. Orders them by `priority` (ascending, lower = higher priority), then by `position`
3. Moves the first eligible task to `ready` status
4. The task's `body` becomes the prompt for execution
5. The task's `profile` field (or channel's current_profile) is used for resolution

This enables periodic task processing without human intervention — a cron job can drip-feed todo items into the agent's queue.

### Channel and Profile Assignment

Each kanban task can specify:
- `channel_id`: Which channel to execute in (NULL = default cron channel)
- `profile`: Which profile to use (NULL = channel's current_profile at execution time)

When a task is updated to `ready` status, the system:
1. Resolves the target channel (task's channel_id or default cron channel)
2. Resolves the profile (task's profile or channel's current_profile)
3. Creates a pending seq-0 message with `msg_type='kanban'` and `msg_subtype=<task_id>`
4. The agent processes the message like any other pending message

## Memory Management

Memory files are loaded from the profile's memory directory and included in context assembly during the **NeverTrim** priority tier.

### Location

```
$OMNI_DATA_DIR/profiles/<name>/memories/
  MEMORY.md      # Core memory file
  SOUL.md        # Identity/persona file
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MEMORY_MAX_CHARS` | `5000` | Maximum characters in MEMORY.md |
| `USER_MAX_CHARS` | `1000` | Maximum characters for user-specific memory |
| `PLANNING_MODE` | `auto_subtasks` | Global planning mode: `prompt_only`, `auto_plan`, `auto_subtasks`, or `always` |
| `PLANNING_COMPLEXITY_SIMPLE_MAX_CHARS` | `60` | Max chars for "simple" (greeting) classification |
| `PLANNING_COMPLEXITY_STANDARD_MAX_CHARS` | `200` | Max chars for "standard" classification — above this triggers complex planning |
| `PLANNING_COMPLEXITY_KEYWORDS` | (built-in list) | Comma-separated keywords that trigger complex planning |

## Planning Mode

Planning mode controls how the agent approaches a thread — whether it plans ahead, creates subtasks, or responds immediately. The mode is resolved **at thread creation time** and stamped on the `threads.planning_mode` column.

### Mode Values

| Mode | Description |
|------|-------------|
| `prompt_only` | No planning — LLM responds directly. Used for simple/quick interactions. |
| `auto_plan` | The LLM gets a planning step before responding. A single plan is created and executed. |
| `auto_subtasks` | Full subtask-based planning. The LLM decomposes the task into subtasks, then works through them sequentially with tool access. |
| `always` | Legacy alias for `auto_subtasks` (normalized at resolution time). |

### Priority Chain

The planning mode is resolved in this order (first non-empty wins):

1. **Task/Job `planning_mode`** — for cron jobs (`cron_jobs.planning_mode`). When non-empty, it overrides everything below. Can be `prompt_only`, `auto_plan`, `auto_subtasks`, or empty (→ complexity-based default).
2. **Channel `planning_mode`** — set on the `channels` table. Override for an entire channel.
3. **Kanban tasks** — always resolve to the max plan mode currently available (`max_plan` logic based on global `PLANNING_MODE`). Kanban tasks never go through complexity classification.
4. **User / Cron default** — classified by prompt **complexity** (see below). Falls through to the global `PLANNING_MODE` env var.

### Complexity Classification

For user messages and cron jobs without an explicit mode, the system classifies the prompt content:

```
char_len < SIMPLE_MAX (60) OR word_count ≤ 3 + greetings  →  prompt_only
char_len > STANDARD_MAX (200) OR action keywords present   →  auto_subtasks
otherwise                                                  →  auto_plan (via PLANNING_MODE)
```

**Simple messages** (short, greetings, confirmations: "hi", "ok", "thanks", "done", thumbs up) → `prompt_only` — no planning overhead.

**Complex messages** (action keywords: "implement", "refactor", "redesign", "migrate", "multi-step", "fix bug" OR character count > 200) → `auto_subtasks` — full decomposition.

**Standard messages** (everything else) → uses the global `PLANNING_MODE` env var (default `auto_subtasks`), resolved to `auto_plan` for user-facing messages.

### Max Iterations Per Mode

Each planning mode maps to a different iteration limit (how many LLM tool-calling rounds allowed):

| Mode | Config Field | Default |
|------|-------------|---------|
| `prompt_only` / unset | `max_iterations_no_plan` | 5 |
| `auto_plan` | `max_iterations_simple_plan` | 10 |
| `auto_subtasks` / `always` | `max_iterations_complex_plan` | 25 |

These are configured via the profile's `AgentConfig` block, not env vars.

Memory files in the `memories/` directory are loaded and included in every context assembly at the highest priority (NeverTrim tier), ensuring they are always present in the system prompt regardless of token budget constraints.

## Stopping and Resuming

### `POST /stop/{channel_id}`

Stop processing for a specific channel:

```bash
curl -X POST http://localhost:8080/stop/1
```

This will:
1. Mark all **pending** and **processing** messages in the channel as `skipped`
2. Record the stop in the `channel_stops` table
3. Cancel the channel's processing task
4. The supervisor will not respawn a handler for this channel until resumed

### `GET /resume/{channel_id}`

Resume processing for a stopped channel:

```bash
curl http://localhost:8080/resume/1
```

This will:
1. Delete the stop record from `channel_stops`
2. The supervisor will detect the channel is no longer stopped
3. A fresh handler will be spawned in idle state
4. New pending messages will be processed immediately

### Behavior

- Messages created **before** the stop are skipped
- Messages created **after** the stop remain pending and will be processed when resumed
- The channel handler restarts fresh — no state is carried over from before the stop
- If the executor was in the middle of processing a message when `/stop` was called, that message is also marked as skipped

## Configuration Reference

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `OMNI_DATA_DIR` | `/opt/data` | Profile and tools directory |
| `DATABASE_URL` | `postgres://omniagent:***@postgres:5432/omniagent` | PostgreSQL connection string |
| `QDRANT_URL` | `http://localhost:6333` | Qdrant endpoint |
| `LLM_API_KEY` | — | API key for LLM provider |
| `LLM_PROVIDER` | `opencode-go` | Provider: `opencode-go`, `openai`, `anthropic`, `deepseek` |
| `DEEPSEEK_API_KEY` | — | DeepSeek-specific API key |
| `DEEPSEEK_BASE_URL` | *default* | DeepSeek API endpoint base URL |
| `MAX_TOKENS` | `4096` | Max response tokens |
| `TEMPERATURE` | `0.7` | Sampling temperature |
| `MAX_ITERATIONS_NO_PLAN` | `5` | Max agent turns for `prompt_only` mode |
| `MAX_ITERATIONS_SIMPLE_PLAN` | `10` | Max agent turns for `auto_plan` mode |
| `MAX_ITERATIONS_COMPLEX_PLAN` | `25` | Max agent turns for `auto_subtasks` mode |
| `HOST` | `0.0.0.0` | HTTP bind address |
| `PORT` | `8080` | HTTP port |
| `DELETE_AFTER_DAYS` | `30` | Message retention period |
| `SUMMARY_WINDOW` | `10` | Half-window size for channel summarization |
| `CHANNEL_SUMMARY_TOKENS` | `4096` | Max tokens for channel-level summary generation |
| `THREAD_SUMMARY_TOKENS` | `2048` | Max tokens for per-thread end-of-execution summary |
| `MCP_SERVERS_CONFIG` | — | External MCP servers config file path |
| `VECTORIZE_MESSAGES` | `false` | Enable message embedding generation |
| `VECTORIZE_WIKI` | `false` | Enable wiki embedding generation |
| `MEMORY_MAX_CHARS` | `5000` | Max characters in MEMORY.md |
| `USER_MAX_CHARS` | `1000` | Max characters for user memory |

### Settings Organization

Environment variables are organized into categories:

| Category | Variables |
|----------|-----------|
| **General** | `HOST`, `PORT`, `OMNI_DATA_DIR`, `QDRANT_URL`, `DELETE_AFTER_DAYS`, `MAX_ITERATIONS`, `MCP_SERVERS_CONFIG` |
| **LLM** | `LLM_API_KEY`, `LLM_PROVIDER`, `MAX_TOKENS`, `TEMPERATURE`, `DEEPSEEK_API_KEY`, `DEEPSEEK_BASE_URL` |
| **Memory** | `MEMORY_MAX_CHARS`, `USER_MAX_CHARS` |
| **Retrieval** | `VECTORIZE_MESSAGES`, `VECTORIZE_WIKI` |

## API Endpoints

### `GET /health`

Health check. Returns `ok` with status 200.

### `POST /stop/{channel_id}`

Stop processing for a channel. All pending and processing messages are marked as `skipped`.

### `POST /resume/{channel_id}`

Resume processing for a stopped channel.

### `GET /prompt/{channel_name}`

Show the raw system prompt that would be used for a given channel (for debugging).

### `POST /prompt-preview/{channel_name}`

Preview the assembled system prompt with a custom prompt and plan. Useful for testing context assembly without inserting a message.

```bash
curl -X POST http://localhost:8080/prompt-preview/my-channel \
  -H "Content-Type: application/json" \
  -d '{"prompt": "What is the weather?", "plan": false}'
```

Returns:
```json
{
  "system_prompt": "...",
  "messages": [{ "role": "user", "content": "What is the weather?" }],
  "plan": false
}
```

### `GET /settings`

Returns all configuration settings with metadata, organized by category.

```bash
curl http://localhost:8080/settings
```

Response:
```json
{
  "General": [
    {
      "name": "HOST",
      "value": "0.0.0.0",
      "type": "string",
      "description": "HTTP bind address",
      "options": null,
      "readonly": true,
      "default": "0.0.0.0"
    },
    ...
  ],
  "LLM": [ ... ],
  "Memory": [ ... ],
  "Retrieval": [ ... ]
}
```

Read-only settings (`HOST`, `PORT`, `QDRANT_URL`) are marked with `readonly: true` and cannot be modified via the API.

### `PUT /settings`

Update one or more settings. Writes changes back to the `.env` file.

```bash
curl -X PUT http://localhost:8080/settings \
  -H "Content-Type: application/json" \
  -d '{"updates": [{"name": "MAX_TOKENS", "value": "8192"}]}'
```

- Returns `200` with the updated settings list on success
- Returns `403` if attempting to modify a read-only setting
- Returns `404` if a setting name is not recognized

## Sending Messages

Messages are inserted directly into the database. The agent polls for `pending` messages every second.

```sql
INSERT INTO messages (channel_id, thread_id, thread_sequence, role, content, status, msg_type, iteration_count, profile)
VALUES (1, 1, 0, 'user', 'Your prompt here', 'pending', 'message', 0, 'default');
```

### Message Fields

| Field | Description |
|-------|-------------|
| `profile` | Profile to use for processing (overrides channel's current_profile) |
| `msg_type` | Type: `message`, `cron`, `kanban`, `tool`, `tool_result`, `reasoning`, `summary`, `plan`, `multi-tool`, `error` |
| `msg_subtype` | For kanban/cron: stores the task/job ID. For errors: error code (`no-profile`, `no-provider`, `no-model`) |
| `processing_time_ms` | Wall-clock time spent processing the message |
| `token_usage` | JSONB: `{prompt_tokens, completion_tokens, cached_tokens, reasoning_tokens}` |

## CLI Commands

### `/usage`

Reads from the `threads` table to display token usage per channel and totals:

```bash
# Display token usage summary
cargo run -- /usage
```

Output shows:
- Token usage per channel (prompt + completion + cached + reasoning tokens)
- Total token usage across all channels
- Helps with cost tracking and monitoring

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
        Memory/
          Promoted/     # Promoted long-term memories
  config/
    mcp-servers.json    # External MCP server config (optional)
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

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| Messages stay `pending` | Channel stopped or agent not running | Check `GET /health`, resume channel |
| LLM call fails | API key missing or invalid | Check `LLM_API_KEY` in `.env` |
| Processing stuck at `processing` | Container restarted mid-call | On restart, pending/processing messages are marked as skipped |
| No model configured | Profile + channel both lack model | Set `current_model` on channel or `model` on profile |
| Tools returning errors | Path outside data directory | Ensure file paths are under `OMNI_DATA_DIR` |
| Settings write fails with 403 | Attempted to modify read-only setting | `HOST`, `PORT`, `QDRANT_URL` are read-only |

## Internal Docs

For detailed internal architecture, see [AGENTS.md](AGENTS.md).

## Testing

### Test Environment Setup

The system uses PostgreSQL for all state. Test data is injected via direct SQL:

```bash
# Insert a test thread with cause message
docker compose exec postgres psql -U omniagent -d omniagent
```

### Thread Lifecycle Tests

| Test | Setup | Expected |
|------|-------|----------|
| **Single channel, all causes** | 3 threads (user/cron/kanban) → same channel → set pending | Processed **sequentially** (one after another). All complete |
| **Different channels (parallelism)** | 3 threads in 3 different channels → set pending | Processed at the **same second** — each channel handler runs independently |
| **Stop/Resume** | Start a thread → `curl stop/<id>` → verify `skipped` → `resume` → new message | Stopped thread = `skipped`. New thread after resume picks up immediately |
| **Empty provider** | Thread with `provider=''` | **failed** with clear error: "provider is not set" |
| **Empty model** | Thread with `model=''` | **failed** with clear error: "model is not set" |
| **Nonexistent profile** | Thread with `profile='nonexistent'` | Falls back to **default** profile (intentional feature) |

### Verification Commands

```bash
# Watch processing in real-time
docker compose logs -f omniagent | grep -E "Processing|completed|summary|failed"

# Query thread state
docker compose exec postgres psql -U omniagent -d omniagent -c "
SELECT t.id, t.status, t.cause, c.name as ch,
       (SELECT count(*) FROM messages m WHERE m.thread_id = t.id) as msg_count
FROM threads t JOIN channels c ON t.channel_id = c.id
WHERE t.channel_id = <ch_id> ORDER BY t.id;"

# Stop/Resume API
curl http://localhost:8080/stop/<channel_id>
curl http://localhost:8080/resume/<channel_id>"
```
