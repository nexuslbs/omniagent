# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added - Tool-name canonicalization (d2648f1, 1afabef)

- Tools expose a single canonical full name; the `name`/`full_name` duality is
  dropped and the model always sees full tool names (Lucas rule).
- `prompt_generate` / `prompt_compact-messages` remain the default tool names
  (config-overridable settings `prompt_generate_tool` /
  `compact_messages_tool_name`).

### Changed - Token cost accounting + bounded budgets (28549fd)

- Cache-hit usage accounting fixed (prompt cache hit/miss tokens parsed for
  DeepSeek-style usage payloads); token budgets bounded (200K/120K defaults)
  with rustfmt-clean fallback parsing.

### Added - Truncation recovery (6504cf2, 59a682b, 5eb98dc)

- Retry shorter instead of failing after 3x truncation; truncation-aware step
  routing; project-name self-restart guard; no hardcoded provider/plugin
  defaults (all config-driven); `test-truncate` noop-full model added for the
  Part 2 truncation regression test.

### Added - Dispatcher channel-claim guard (ada41e9)

- The dispatcher does not dispatch a `todo` task onto a channel already
  claimed by an active task.

### Changed - Profiles source of truth (c133e25, 8f4a64e, 7db63bc)

- `config/profiles.yml` is the source of truth for profiles; a default profile
  `config.json` is auto-created at startup (profiles/ dir dropped from repo);
  SOUL.md/USER.md support removed in favor of a single root MEMORY.md.

### Added - Scoped/ordered prompt sections (f6157a3)

- Prompt assembly supports scoped, ordered prompt sections (system prompt
  builder).

### Added - code_exec MCP tool (cb72db3, 462914f)

- Run model-written programs in the toolbox container with typed JSON returns;
  hard container-side timeout via busybox `timeout` wrapper.

### Added - Durable goal state machine (5e2e9e8)

- Goal state machine with phase + typed blocked reason + round cap (CAS),
  backed by goal-state queries (offline cache refreshed).

### Changed - Context overflow handling (88af0b1, 43d7f0b, 3eb2f2e)

- Forced compaction + retry on context overflow (kill the death spiral);
  deterministic tool-result pruning (head/middle/tail) before summarization;
  oversized tool results spill to disk with preview + locator.

### Changed - Resolution at load time (100c9d2)

- Channel identity + task defaults resolve AT LOAD TIME (loaders return
  resolved data, never shallow values).

### Chore - Repo hygiene (a923360, 0cd275f)

- Removed committed MCP-server build artifacts from tracking (gitignored, built
  by Dockerfile); removed unused `default_cli_channel` setting.

### Changed - Planning normalized to a single `plan` bool

- The legacy `planning_mode` string duplicate is gone everywhere: DB columns
  (`threads`, `kanban_tasks`, `cron_jobs`, `hooks`, `channels`) are dropped after
  backfilling `plan = true` for `auto_plan`/`auto_subtasks`/`always`; the
  `tasks.yml` `PlanningMode` enum is removed (hooks/schedules now carry a plain
  `plan` boolean); the kanban/cron/hook APIs and the dashboard expose only `plan`.
- The retired `planning_mode`/`plan_with_subtasks` values from the earlier
  "Phase 0a: planning mode" work are replaced by the boolean field.

### Added - Default channel settings

- Three writable select settings - `default_schedule_channel`,
  `default_hook_channel`, `default_kanban_channel` - each a select over the channels
  defined in channels.yml (any platform; a platform-less channel is type `cli`).
- The `kanban`/`cron`/`hook` channel platforms are gone; a channel with no `platform`
  field is a `cli` channel.
- Threads resolve their channel via: explicit channel -> default-*_channel setting -> ''.
  A thread with no channel is still inserted (record kept for audit) and then marked
  failed with "no channel defined".

### Removed - `default_cli_channel` setting

- The `default_cli_channel` setting was removed: the binary has no CLI session mode
  (only `--version`/`--help` args then server boot), and the only `/mcp/execute`
  caller (dashboard read-only `search_database`) never creates sessions, so the
  setting had no live consumer. CLI-platform tool calls with no explicit channel
  keep the previous behavior: no channel (empty `channel_id`).

Workflow-based role assignment (implementation phases 0a, 0–7 of the
`WorkflowImplementation` plan). Kanban tasks are now routed through a
role-based workflow state machine (executor → tester → reviewer) with retry
limits, manual-only review decisions, step-aware recovery, and a canonical
7-status list (`backlog`, `todo`, `running`, `testing`, `review`, `blocked`,
`done`). The retired `ready` status is rejected everywhere.

### Added - Phase 0a: planning mode

- `planning_mode` column on `kanban_tasks` and `channels` (schema + API).
- Kanban board/detail expose the planning-mode flag with channel fallback.

### Added - Phase 0: workflow schema + config

- Schema v6 DDL: `kanban_tasks.workflow_id`, `thread_status`,
  `workflow_state`, workflow fields on `threads`, `kanban_history` comment
  column (R4 migration: open `ready` tasks → `running`).
- `workflows.yml` parsing and validation (`src/workflows.rs`), including
  role resolution and `clear_executions_on_review`.
- `ready` removed from the valid status list.

### Changed - Phase 1: canonical status list

- Seven-status list enforced across server, DB, and tools;
  `validate_status("ready")` now fails everywhere.

### Added - Phase 2: fail-thread tool + workflow step keys

- Builtin `fail_thread` tool (`src/agent/fail_thread.rs`) with F-matrix
  routing (F0–F4), consumed by `kanban_updater` and task tools.
- `metadata.workflow_step` step keys: `""` (→ executor), `running`,
  `testing`, `blocked`; anything else (incl. `review` and role names) is
  invalid.

### Changed - Phase 3: atomic engine transitions

- Server-loop transitions are transactional: retry counts per role,
  interruption reruns, no thread spawn when a task is already
  `blocked`/`done`, and at retry limit the task moves to `blocked` with an
  auto comment and the step never starts.

### Changed - Phase 3b: role-aware prompt context

- `prompt_generate` emits a workflow-context block; tester/reviewer use
  inverse role prompts (template as user prompt, task description as system
  prompt).

### Added - Phase 4: reviewer/tester decision routing

- Tester `fail`/normal routing; reviewer decisions per R12, with
  manual-only review endpoints (`kanban_review_task` tool +
  `POST /kanban/tasks/:id/review`) and target-status validation (R5).

### Changed - Phase 4b: clear_executions_on_review

- `clear_executions_on_review` retry guard in `fail_thread`: executor/tester
  limit rolls to review instead of `blocked` when set.

### Added - Phase 5: workflow CRUD + reset

- `workflows.yml` CRUD endpoints (atomic save + reload) and
  `POST /workflows/executions/reset` to clear per-task execution counters.

### Changed - Phase 6: step-aware recovery

- Channel closure/deletion recovery is step-aware
  (`skip_recovery` in `src/db/threads.rs`): implicit continuation
  re-schedules a fresh thread without consuming a retry; `blocked`/`done`
  tasks are never rescheduled; explicit stop never auto-retries and never
  moves the task to `todo`.

### Changed - Phase 7: docs, polish, verification

- Added this CHANGELOG documenting phases 0a, 0–7.
- Retired `ready` fully removed from production defaults
  (`db::threads` recovery paths default to `todo`).
- Shared `is_terminal_status` predicate (`blocked`/`done`) used by
  `engine_transition` and `manual_review_decision`, with unit tests for the
  matrix gaps (`ready` never terminal, blocked/done never spawn a thread).
- Full verification: `cargo build --release` (zero warnings),
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`,
  `cargo test` (394 passed), dashboard `npm run build` + unit tests.
