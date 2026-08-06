# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Workflow-based role assignment (implementation phases 0a, 0–7 of the
`WorkflowImplementation` plan). Kanban tasks are now routed through a
role-based workflow state machine (executor → tester → reviewer) with retry
limits, manual-only review decisions, step-aware recovery, and a canonical
7-status list (`backlog`, `todo`, `running`, `testing`, `review`, `blocked`,
`done`). The retired `ready` status is rejected everywhere.

### Added — Phase 0a: planning mode

- `planning_mode` column on `kanban_tasks` and `channels` (schema + API).
- Kanban board/detail expose the planning-mode flag with channel fallback.

### Added — Phase 0: workflow schema + config

- Schema v6 DDL: `kanban_tasks.workflow_id`, `thread_status`,
  `workflow_state`, workflow fields on `threads`, `kanban_history` comment
  column (R4 migration: open `ready` tasks → `running`).
- `workflows.yml` parsing and validation (`src/workflows.rs`), including
  role resolution and `clear_executions_on_review`.
- `ready` removed from the valid status list.

### Changed — Phase 1: canonical status list

- Seven-status list enforced across server, DB, and tools;
  `validate_status("ready")` now fails everywhere.

### Added — Phase 2: fail-thread tool + workflow step keys

- Builtin `fail_thread` tool (`src/agent/fail_thread.rs`) with F-matrix
  routing (F0–F4), consumed by `kanban_updater` and task tools.
- `metadata.workflow_step` step keys: `""` (→ executor), `running`,
  `testing`, `blocked`; anything else (incl. `review` and role names) is
  invalid.

### Changed — Phase 3: atomic engine transitions

- Server-loop transitions are transactional: retry counts per role,
  interruption reruns, no thread spawn when a task is already
  `blocked`/`done`, and at retry limit the task moves to `blocked` with an
  auto comment and the step never starts.

### Changed — Phase 3b: role-aware prompt context

- `prompt_generate` emits a workflow-context block; tester/reviewer use
  inverse role prompts (template as user prompt, task description as system
  prompt).

### Added — Phase 4: reviewer/tester decision routing

- Tester `fail`/normal routing; reviewer decisions per R12, with
  manual-only review endpoints (`kanban_review_task` tool +
  `POST /kanban/tasks/:id/review`) and target-status validation (R5).

### Changed — Phase 4b: clear_executions_on_review

- `clear_executions_on_review` retry guard in `fail_thread`: executor/tester
  limit rolls to review instead of `blocked` when set.

### Added — Phase 5: workflow CRUD + reset

- `workflows.yml` CRUD endpoints (atomic save + reload) and
  `POST /workflows/executions/reset` to clear per-task execution counters.

### Changed — Phase 6: step-aware recovery

- Channel closure/deletion recovery is step-aware
  (`skip_recovery` in `src/db/threads.rs`): implicit continuation
  re-schedules a fresh thread without consuming a retry; `blocked`/`done`
  tasks are never rescheduled; explicit stop never auto-retries and never
  moves the task to `todo`.

### Changed — Phase 7: docs, polish, verification

- Added this CHANGELOG documenting phases 0a, 0–7.
- Retired `ready` fully removed from production defaults
  (`db::threads` recovery paths default to `todo`).
- Shared `is_terminal_status` predicate (`blocked`/`done`) used by
  `engine_transition` and `manual_review_decision`, with unit tests for the
  matrix gaps (`ready` never terminal, blocked/done never spawn a thread).
- Full verification: `cargo build --release` (zero warnings),
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`,
  `cargo test` (394 passed), dashboard `npm run build` + unit tests.
