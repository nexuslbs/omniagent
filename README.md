# OmniAgent

Next-generation agent system built with Rust, PostgreSQL + pgvector, and MCP tool support.

## Features

| **Hindsight Memory** | Persistent cross-session memory via omniagent-hindsight, with automatic population from new messages and semantic recall in context assembly |
| **Hindsight Populator** | Background action (deactivated by default) that retains messages into hindsight every 15 minutes. Activate via `UPDATE cron_jobs SET active = true WHERE id = 'hindsight_populator'`. Cron schedules use standard 5-field Linux format (minute hour day month weekday: the leading seconds field is not used). |
| **Kanban Boards & Workflows** | Role-based kanban workflows (executor → tester → reviewer) from `config/workflows.yml`, board defaults from `config/boards.yml` (feature-gated on file presence), board/dependency/channel-busy gates in the dispatcher, `auto_approve`, `review_on_fail` |
| **Event Hooks** | Event-driven hooks fired fire-and-forget, isolated from the triggering work: `thread_started`, `thread_finished`, `new_message` - delivered to the hooks channel (Dashboard Hooks page) |
| **Token Context Budgets** | Token-only budgets (`prompt_token_budget_soft/hard`, default 100000/500000) owned by the prompt plugin's `compact-messages` tool; `chars/4` fallback when no tokenizer is available |
| **models.yml Overrides** | `config/models.yml` provides plugin-less provider definitions + provider/model/token-budget overrides (`model_config.<model> > providers.<name> > global settings`) |
| **Builtin `omniagent-api`** | Internal self-API fetch MCP tool (no host/scheme/port needed) replacing the old `kanban_*` / `cron_*` plugin tools; the kanban/cron API uses YML property names (`channel`, `workflow`, `cron`, `provider`, `model`) |
| **Plugin Config References** | Config values support `$secret:name` (load from secrets DB) and `$env:VAR_NAME` (load from env var) prefixes: keeps secrets out of YAML, single source of truth for shared config. |

### 🧠 Context Builder & Grounding
- **Priority-ranked prompt assembly** (`ContextBuilder`): NeverTrim (system, MEMORY.md, subtasks) → High (thread messages) → Normal (tool defs) → Low (retrieved content)
- **Token budgeting**: per-block character caps, lowest-priority blocks dropped when over budget
- **Grounding policy**: embedded in every system prompt: prefer evidence, state uncertainty, cite references
- **Evidence metadata**: `messages.metadata` captures context diagnostics (`context.selected_message_ids`, `block_counts`, `dropped_blocks`, `total_chars`) and grounding flags

### 🔍 Hybrid Retrieval
- **4-tier retrieval** controlled by profile `retrieval_aggressiveness` (0-3):
  - Level 1: ILIKE text search in messages + wiki text search (walkdir)
  - Level 2+: pgvector semantic search (`<=>` cosine similarity on message embeddings) + Qdrant vector search on wiki content
- **Query classifier**: heuristic (Greeting/Command/FollowUp/Factual/ExternalQuery) gates whether retrieval runs
- Re-ranking with recency and same-thread boosts

### 💾 Memory Promotion
- **3 MCP tools** (`promote_to_memory`, `list_memories`, `review_memories`)
- YAML frontmatter

### 🔄 Dynamic Enum Refresh (`refresh_url`)
- Providers can declare a `refresh_url` in their plugin manifest or `models.yml` to fetch live enum options (models, etc.)
- Refreshed on demand via the dashboard / providers API; enums cached with metadata (`fetched_at`, `etag`)

### 🪪 Plugin Config References (`$secret:` / `$env:`)

Config values in `plugins.yml` (and other YAML configs) support two reference prefixes:

| Prefix | Source | Example |
|--------|--------|---------|
| `$secret:name` | Secrets DB table (`/secrets` page) | `$secret:my_telegram_token` |
| `$env:VAR_NAME` | Process environment variable | `$env:OPENCODE_GO_API_KEY` |

The YAML file stores the reference string, never the resolved value. The agent resolves references at runtime, so secrets stay out of version control and shared values have a single source of truth.

### 🔌 Plugin System (MCP Tools, Platforms, Providers)

The plugin system has **three sources**:

| Source | Location | Description |
|--------|----------|-------------|
| **Built-in** | `/app/plugins/{type}/{name}/` | Workspace crates inside the omniagent image (cron, kanban, memory, metrics, plugin-manager, query, search, subtasks, hindsight, prompt, wiki, ...) |
| **Bundled** | `plugins/{type}/{name}/` (omni-stack fork) | Standalone crates added by forked repos, same layout as built-in with a `plugin.json` manifest |
| **Remote** | `plugins/{type}/.remote/{name}/` | Git-cloned from external repositories via `install-git` / Download (Update) |

**Plugin identity is the composite key `[type + source + name]`** - never look up by name alone. Action handlers derive type+source from the URL path (HARD RULE, see AGENTS.md).

The dashboard Tools page shows plugins from all sources; YAML presence/`builtin: true` flags determine the primary source. Plugin state (enabled/disabled, config) lives in `plugins.yml`; installs compile + register, uninstall removes binary + disables, update re-clones from git + recompiles.

### 🔌 MCP External Servers

External MCP servers are configured via `MCP_SERVERS_CONFIG` (a JSON file listing server name, command, args, and env). They appear as tool sources in the MCP registry and the dashboard Tools page.

### 📋 Thread Subtasks

- **Subtasks MCP tool**: manage a subtask list per thread (create, update, complete, cancel) with statuses `pending / completed / cancelled / error`
- Subtask state is included in context assembly at the NeverTrim tier
- Failed subtasks can be retried (bounded by `max_unfinished_subtask_retries`)

### Requirements

- Rust (stable) + PostgreSQL 16 with pgvector
- Qdrant (optional, for wiki/message vector search)

### Setup

```bash
cargo build --release
cp .env.example .env
```

### Verify

```bash
# Edit .env with at minimum DATABASE_URL and LLM_PROVIDER
cargo run
# → ok
curl http://localhost:8080/health
```

## Channels

Channels represent communication endpoints (Telegram, Mattermost, API, cron, kanban). Each channel has its own profile, provider, model, and planning-mode configuration. Messages are processed sequentially within a channel, in parallel across channels.

On the Mattermost platform, the `$new` command creates/updates a channel by that name: `$new <name>` - the optional first argument names the channel instead of prompting.

### Channel Fields

| Field | Description |
|-------|-------------|
| `name` | Unique channel name (the stable identifier used everywhere: API, `threads.channel_id`, config files) |
| `platform` | `mattermost`, `telegram`, `api`, `cron`, `cli` (platform-less = `cli`: never delivers externally) |
| `external_id` | Platform-specific resource identifier |
| `cause` | How messages are created: `user`, `cron`, `kanban`, `api`, ... |
| `current_profile` | Default profile for the channel |
| `current_provider` | Overrides the profile's provider |
| `current_model` | Overrides the profile's model |
| `planning_mode` | Default planning mode for threads in this channel |
| `template` | Optional template name injected into every user message's prompt (also the default template for cron/kanban tasks in this channel) |
| `readonly` | Protects the channel from deletion (e.g. the default cron channel) |
| `closed` | Stops processing new messages (they remain pending) while retaining history |

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

## Profiles

Profiles bundle model configuration, provider, and allowed tools. A `default` profile is created on first startup.

Profile fields:
- **provider**: LLM provider (e.g., `opencode-go`, `openai`, `anthropic`, `deepseek`)
- **model**: LLM model name (e.g., `deepseek-v4-flash`, `claude-sonnet-4`)
- **allowed_tools**: which MCP tools the agent can use

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

### LLM Provider and Model Resolution

The effective provider and model for each request are resolved in this order:

**Provider resolution chain:**
1. **Channel** `current_provider`: if set, this provider is used
2. **Profile** `provider`: if the channel has no provider, the profile's provider is used
3. **`LLM_PROVIDER` env var**: if neither channel nor profile defines a provider, the environment variable is used
4. **Error**: if none of the above is set, the agent returns an error

**Model resolution depends on where the provider came from:**

- **Provider from channel** → the model is taken from the channel's `current_model`, **or** the provider's `default_model` if the channel has no model set. The profile's model is **ignored** at this level.
- **Provider from profile** → the model is taken from the profile's `model`, **or** the provider's `default_model` if the profile has no model set. The channel's model is **ignored** at this level.
- **Provider from `LLM_PROVIDER` env var** → the model is always the provider's `default_model`. Both channel and profile models are **ignored**.
- **No model resolved at any level** → the agent returns an error.

**API key resolution:**
The API key is read from the `{PROVIDER}_API_KEY` environment variable matching the resolved provider name (e.g. `DEEPSEEK_API_KEY` for deepseek, `OPENCODE_GO_API_KEY` for opencode-go). `models.yml` provider entries can also declare `api_key: "$env:..."` / `"$secret:..."`. There is no generic fallback: the correct key must be set for the active provider.

**models.yml overrides (`config/models.yml`):** at startup the agent loads provider/model overrides from `models.yml` (absent/empty = zero behavior change):
- `providers.<name>.plugin: false` defines a **plugin-less provider** using builtin chat_completions/anthropic support (no plugin code needed)
- `providers.<name>.models` replaces the plugin's `default_model.allowed_values` in selectors (Channels page, Providers page, `/models` page)
- Provider-level fields (`api_mode`, `supports_reasoning`, `default_base_url`, `refresh_url`, `default_model`, `api_key`, `token_budget_*`, `max_tokens*`) override the plugin config
- `model_config.<model>` per-model overrides take **highest precedence** - for each of soft/hard token budgets: `model_config.<model> > providers.<name> > global settings`

**Summary table:**

| Provider source | Model source | Model fallback |
|----------------|-------------|----------------|
| Channel | Channel model | Provider default_model |
| Profile | Profile model | Provider default_model |
| LLM_PROVIDER env | N/A | Provider default_model |

## Execution Model

### Sequential Per Channel, Parallel Across Channels

Messages in a channel are processed sequentially (one thread at a time), and channels run in parallel. Each channel's processing loop:
1. Picks the next pending message for the channel (FIFO by seq).
2. Creates a thread if one isn't running.
3. Runs the agent loop (prompt assembly → LLM → tool calls) until the thread terminates.
4. Marks the thread terminal (completed/failed) and moves on.

### Message Lifecycle

1. A message is inserted with `status = 'pending'`.
2. The dispatcher (channel loop) picks it up, creates a thread, and marks the message `processing`.
3. The agent runs; each iteration appends messages (`prompt`, `response`, `reasoning`, `tool`, `tool_output`, `iteration`, ...).
4. On terminal transition the thread is marked `completed`/`failed` and event hooks fire (`thread_finished`).
5. Seq-0 message is the user prompt; `read_keep_last`/`read_excerpt_chars` control how much context is retained for long threads.

### Message Types

| Type | Description |
|------|-------------|
| `prompt` | The assembled prompt sent to the LLM |
| `response` | The LLM's text response |
| `reasoning` | LLM reasoning content (when supported) |
| `tool` | A tool invocation request |
| `tool_output` | The tool result |
| `iteration` | Iteration boundary marker |
| `delegate_result` | Delegated subtask output |
| `skill` | Skill attachment/context |

### Error Subtypes

Failures carry a subtype for diagnostics: `provider_error`, `tool_error`, `context_overflow`, `rate_limit`, `auth_error`, `timeout`, `no_channel`, `invalid_request`, etc. The Dashboard Messages page filters on subtype.

### Per-Message Timing and Token Usage

Each message records timing (created/finished) and token usage (prompt_tokens, completion_tokens, total_tokens) - the Dashboard Overview charts aggregate these.

### Startup Cleanup

On startup the agent marks stale `processing` threads as `failed` (`skip_on_startup`) so a crash doesn't leave threads stuck forever. A **Postgres advisory lock** (session-scoped, `db::try_acquire_advisory_lock`) guarantees only ONE instance runs against a database at a time: a second instance refuses to start ("advisory lock key held") instead of racing the cleanup and marking live threads skipped (the fix for the zombie-executor duplicate-thread bug).

### Profile Resolution at Message Time

The channel's `current_profile` (or the message's explicit profile) is resolved at message time; a channel can switch profiles between messages.

### Provider/Model Validation at Execution Time

Provider/model are validated at execution time against the resolution chain above. Unknown provider/model → thread fails with a clear error; the Dashboard surfaces it.

## Cron Jobs

Cron jobs are stored in the `cron_jobs` table and driven by the scheduler. They dispatch messages to a channel on schedule, creating threads with cause `cron`.

### Creating a Cron Job

```sql
INSERT INTO cron_jobs (id, name, schedule, channel_id, prompt, active)
VALUES ('job-1', 'daily-report', '0 9 * * *', 'cron-default', 'Write the daily report', true);
```

### Fields

| Field | Description |
|-------|-------------|
| `name` | Unique job name |
| `schedule` | 5-field cron (minute hour day month weekday - no leading seconds field) |
| `channel` | Destination channel |
| `prompt` | Prompt template for the dispatched message |
| `mode` | Execution mode |
| `active` | Whether the job is currently scheduled |

### Execution Modes

- **message**: dispatch a normal message to the channel (default)
- **run**: run a registered action (e.g. `hindsight_populator`)

### Cron Planning Mode

Cron-dispatched threads can run in planning mode; the channel's `planning_mode` field is the default.

### Knowledge Pipeline

A cron job (`knowledge-pipeline`) performs periodic maintenance: summarize channels, update wiki/skills from threads, run relevance indexing, populate hindsight.

### Scheduler

The scheduler ticks every `kanban_dispatcher_interval` (15s default) and dispatches due cron jobs + kanban tasks.

## Kanban Tasks

Kanban tasks are project-management items dispatched to channels with cause `kanban`. They are stored in the `kanban_tasks` table and driven by **boards** (`config/boards.yml`) and **role-based workflows** (`config/workflows.yml`).

### Creating a Kanban Task

```sql
INSERT INTO kanban_tasks (id, title, body, board, workflow_id, status)
VALUES ('task-1', 'Fix login bug', 'Details...', 'omnidev', 'omniagent-dev', 'todo');
```

Via the API (field names match the YML properties - `channel`, `workflow`, `board`, not `channel_id`/`workflow_id`):

```bash
curl -X POST localhost:8080/kanban/tasks \
  -H 'Content-Type: application/json' \
  -d '{"title":"...","body":"...","board":"omnidev","workflow":"omniagent-dev"}'
```

### Boards (`config/boards.yml`)

Boards are optional (feature-gated on file presence). When `boards.yml` is present:
- Task create/edit **requires** a valid board (API rejects missing/invalid board).
- The board defines defaults resolved AT LOAD TIME: task → board → channel → global settings (channels, workflows, templates, provider/model, planning mode). Loaders return resolved data, never shallow values.
- The board has a workflow (`workflow_id`), plan, and provider/model defaults for tasks on that board.

### Workflows (`config/workflows.yml`)

Workflows define the **role-based lifecycle** of a task - which roles run in which order, with what template/mode/provider/model:

```yaml
omniagent-dev:
  auto_approve: false        # require explicit review approval
  review_on_fail: false      # testing failure routes to review (not directly back to executor)
  clear_executions_on_review: true
  retries: 3                 # global default per role
  roles:
    executor:
      template: dev-executor
      mode: agent            # agent | action
      action_id: null        # used when mode: action
      plan_mode: on
      retries: 9
    tester:
      template: dev-tester
      mode: agent
      plan_mode: on
    reviewer:
      template: dev-reviewer
      mode: agent
      plan_mode: on
```

- **Roles**: each role (executor/tester/reviewer/...) runs in sequence, creating a thread in the role's step. `mode: agent` runs a prompt template; `mode: action` runs a registered action by `action_id`.
- **auto_approve**: when true, the workflow skips review approval (executor-only tasks like this one).
- **review_on_fail**: when true, a failed testing step routes to review instead of straight back to executor.
- Each task execution records its steps in `task_executions`; `clear_executions_on_review` wipes them when a task returns to review so re-reviews start clean.

### Task Lifecycle

```
todo → running (executor) → review → testing → done
        ↑______________________|  (review_on_fail=false: test fail → executor)
        ↑__________________|        (review_on_fail=true:  test fail → review)
```

Review can push a task back to running with a verdict (approve/request changes) via `POST /kanban/tasks/{id}/review`.

### Statuses

| Status | Meaning |
|--------|---------|
| `todo` | Not yet started |
| `running` | A workflow step is executing |
| `review` | Awaiting reviewer verdict |
| `testing` | Awaiting tester verification |
| `done` | Completed and approved |
| `blocked` | Stopped (e.g. repeated workflow failures) |

### Kanban Dispatcher

The dispatcher scans for dispatchable tasks every `kanban_dispatcher_interval` seconds and:
- Starts `todo` tasks on their board's workflow when the board/execution gates allow.
- **Board gate**: boards with in-flight task limits stop further dispatches until a task finishes.
- **Channel-busy gate**: a task whose channel is busy waits rather than queueing behind live work.
- **Archived-task guard**: archived tasks are NEVER dispatched (the dispatch SQL excludes them - an archived task can't come back to life).
- Dispatches create threads with cause `kanban` on the task's channel (resolved via the board chain).

### Channel and Profile Assignment

Tasks without an explicit channel resolve through: task → board → `default_kanban_channel` setting → the kanban channel. Provider/model/template/plan_mode resolve the same way at load time (never at execution).

## Memory Management

### Location

- Agent memories live in the data dir: `memories/MEMORY.md`, `memories/USER.md`, plus per-profile memory directories.
- Wiki content (long-term, human-readable) lives in the profile wiki directory.
- Promoted memories (validated facts) are stored as markdown files under the wiki `Memory/Promoted/` with YAML frontmatter (provenance, confidence, expiry).

### Memory Configuration

`settings.yml` → `memory:` block:

| Setting | Default | Description |
|---------|---------|-------------|
| `memory_max_chars` | 5000 | Cap for MEMORY.md |
| `vectorize_messages` | true | Enable message embedding for semantic recall |
| `messages_vectorization_method` | `local` | `local` (sentence-transformers) or provider-based |
| `messages_vectorization_interval` | 5 | Seconds between vectorization batches |
| `vectorize_wiki` | false | Enable wiki embedding |
| `wiki_vectorization_interval` | 3600 | Seconds between wiki re-vectorization |

## HTTP API

The agent exposes an HTTP API on port 8080 (see `api-reference.md` for the full surface). Field names in the API follow the YML property names (`channel`, `workflow`, `cron`, `provider`, `model` - not `channel_id`/`workflow_id`/`current_*`).

| Area | Endpoints |
|------|-----------|
| Health | `GET /health` |
| Threads | `GET /threads`, `GET /threads/{id}` |
| Messages | `GET /messages`, `GET /messages/{id}` |
| Channels | `GET/POST/PATCH /channels`, `GET /channels/{name}` |
| Profiles | `GET /profiles` |
| Cron | `GET/POST /schedule`, `GET/PATCH/DELETE /schedule/{id}`, `POST /schedule/{id}/run` |
| Kanban | `GET/POST /kanban/tasks`, `GET/PATCH/DELETE /kanban/tasks/{id}`, `POST /kanban/tasks/{id}/review`, `GET/POST /kanban/boards` (when boards.yml present) |
| Plugins | `GET /plugins`, `POST /plugins/{type}/{source}/{name}/install`, `DELETE /plugins/{type}/{source}/{name}`, `POST .../enable` / `disable` / `reinstall` / `download` |
| Models | `GET /models` (models.yml overrides) |
| Secrets | `GET/POST /secrets`, `DELETE /secrets/{name}` |

### Builtin `omniagent-api` Tool

The agent exposes its own API to itself as the MCP tool `omniagent-api` (builtin): it fetches `http://localhost:8080/...` internally - no host/scheme/port configuration needed. It replaced the old `kanban_*` / `cron_*` plugin tools; the plugin's `fetch` tool gates unsafe methods via the `allow_unsafe_methods` config.
