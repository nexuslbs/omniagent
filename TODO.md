# OmniAgent Memory & Performance Improvements

## Tracking what needs to be done

### ✅ Implemented

#### Leaner System Prompt (prompt_builder.rs)
- [x] Remove empty constants: `RESEARCH_WORKFLOW`, `SKILLS_GUIDANCE`, `WIKI_GUIDANCE`, `DOCKER_EXECUTION_GUIDANCE` - all `""`, removed
- [x] Shorten `DB_SCHEMA` - from raw DDL (~500 tokens) to compact summary format (~150 chars)
- [x] Bench: saves ~600 chars/tokens per turn - immediate token reduction

#### Templates for Kanban & Cron Tasks
- [x] **Migration:** Add `template TEXT` to `kanban_tasks`, `template TEXT` to `cron_jobs`
- [x] **Template loader:** `load_template(data_dir, profile, template_name)` in `prompt_builder.rs`
- [x] **Kanban dispatcher (scheduler.rs):** Fetch `template` from task, store in cause message metadata
- [x] **Cron scheduler (scheduler.rs):** Fetch `template` from job, store in cause message metadata
- [x] **process_thread (agent/mod.rs):** If cause is kanban/cron with template metadata, load template and inject as system message
- [x] **Template example:** Created `code-improvement.md` sample at `/opt/data/profiles/default/templates/`
- [x] **MCP tool (kanban.rs):** Added `template` parameter to `create_kanban_task` tool

#### Adaptive Planning (Task Classification)
- [x] **context_builder.rs:** Replaced `classify_query` with `classify_complexity` returning `Complexity::Simple | Standard | Complex`
  - [x] Simple: < 60 chars, greeting, acknowledgment → skip plan entirely
  - [x] Standard: > 100 chars, clear request → plan as before
  - [x] Complex: contains keywords (implement, refactor, design) OR kanban/cron task → auto-create subtasks
- [x] **agent/mod.rs:** Uses complexity classification to gate:
  - [x] Planning activation (skip for Simple)
  - [x] Subtask creation (auto-create for Complex)

#### Subtask Automation
- [x] **agent/mod.rs:** After plan generation for complex tasks, parse plan lines and create subtasks via `subtask::add_subtask`
  - [x] Parses numbered or bulleted plan lines (max 6)
  - [x] Priority order preserved from plan
  - [x] Subtasks appear in the system prompt as "Current Task Progress"

### ✅ Hindsight Memory Integration
- [x] `HINDSIGHT_URL` config option (env var, set in docker-compose.yml)
- [x] `context_builder.rs`: `hindsight_recall` call for semantic memory via recall API
- [x] Hindsight results injected as Low-priority context block
- [x] `hindsight_populator.rs`: background message retain into hindsight (batch 200, watermarked)
- [x] Builtin action `builtin_hindsight_populator` with cron scheduling (deactivated by default)
- [x] Impact: Richer cross-session memory retrieval

### 🔲 Future Ideas
- [ ] Cron editing tool (mcp) - add `template`/`template` field to cron edit UI
- [ ] Kanban task editing tool (mcp) - add `template` field update
- [ ] Template listing/management MCP tool
- [ ] Dashboard UI for template management
