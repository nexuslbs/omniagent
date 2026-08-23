//! Database migrations for OmniAgent.
//!
//! Single-phase declarative schema: creates the FINAL state of all tables
//! as they exist after all incremental migrations are applied.
//!
//! No legacy data migrations, no ADD COLUMN / DROP COLUMN evolution steps.
//! Safe to run on every startup (all statements use IF NOT EXISTS).
//!
//! Profile columns (threads.profile) have NO
//! DEFAULT: the application supplies the profile name (default: "omni")
//! at insert time.

use anyhow::Result;
use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> Result<()> {
    create_extensions(pool).await?;
    create_tables(pool).await?;
    create_indexes(pool).await?;
    create_vector_support(pool).await?;
    create_triggers(pool).await?;
    migrate_channels_to_yml(pool).await?;

    // -- Kanban boards (config/boards.yml) --
    // Nullable `board` column on kanban_tasks: NULL = no board. Board gating is
    // feature-flagged on the presence of config/boards.yml (src/boards.rs); the
    // column is inert (and stays NULL for existing tasks) when the file is
    // absent. Board deletion removes its tasks via the board-delete API handler
    // (per-task cleanup mirrors the existing task-delete behavior).
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS board TEXT")
        .execute(pool)
        .await
        .ok();

    // -- Event-driven Hooks (thread_started / thread_finished / new_message) --
    // threads.hook_caused marks hook-caused threads so the hooks engine can
    // skip them (infinite-loop protection: hook threads never re-trigger).
    sqlx::query(
        "ALTER TABLE threads ADD COLUMN IF NOT EXISTS hook_caused BOOLEAN NOT NULL DEFAULT false",
    )
    .execute(pool)
    .await
    .ok();

    // hooks table: mirrors cron_jobs but keyed by event instead of schedule.
    // NOTE (tasks.yml): definitions now live in {data_dir}/config/tasks.yml
    // (`hooks:` key); this table is kept dormant for back-compat and is no
    // longer read for definitions. Hook counters moved to `hook_counters`.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS hooks (
            id            TEXT PRIMARY KEY,
            name          TEXT NOT NULL,
            display_name  TEXT NOT NULL DEFAULT '',
            event         TEXT NOT NULL,
            scope         TEXT NOT NULL DEFAULT 'global',
            target        TEXT,
            counter       JSONB NOT NULL DEFAULT '{"global": 0}'::jsonb,
            count         INT  NOT NULL DEFAULT 1,
            mode          TEXT NOT NULL DEFAULT 'agentic',
            prompt        TEXT,
            action_id     TEXT,
            profile       TEXT,
            channel_id    TEXT,
            plan          BOOLEAN NOT NULL DEFAULT false,
            template      TEXT,
            enabled       BOOLEAN NOT NULL DEFAULT true,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();

    // Idempotent CHECK constraints (event/scope/mode/count value validation).
    sqlx::query(
        r#"
        DO $$
        BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'hooks_event_chk') THEN
                ALTER TABLE hooks ADD CONSTRAINT hooks_event_chk
                    CHECK (event IN ('thread_started', 'thread_finished', 'new_message'));
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'hooks_scope_chk') THEN
                ALTER TABLE hooks ADD CONSTRAINT hooks_scope_chk
                    CHECK (scope IN ('global', 'channel', 'profile'));
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'hooks_mode_chk') THEN
                ALTER TABLE hooks ADD CONSTRAINT hooks_mode_chk
                    CHECK (mode IN ('agentic', 'action'));
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'hooks_count_chk') THEN
                ALTER TABLE hooks ADD CONSTRAINT hooks_count_chk CHECK (count >= 1);
            END IF;
        END $$;
        "#,
    )
    .execute(pool)
    .await
    .ok();

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_hooks_enabled_event ON hooks (enabled, event)")
        .execute(pool)
        .await
        .ok();

    // hook_counters: runtime hook counter state — one JSON counter per hook
    // key (definitions live in {data_dir}/config/tasks.yml). The counter shape
    // matches the legacy hooks.counter JSONB column.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS hook_counters (
            hook_key TEXT PRIMARY KEY,
            counter  JSONB NOT NULL DEFAULT '{"global": 0}'::jsonb
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();

    // task_runs: scheduler cadence bookkeeping — one last-fired timestamp per
    // schedule key (definitions live in {data_dir}/config/tasks.yml). This is
    // the ONLY runtime state the scheduler keeps; runs themselves are
    // observable via the threads each schedule creates.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS task_runs (
            task_key      TEXT PRIMARY KEY,
            last_fired_at TIMESTAMPTZ NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();

    // All messages store the time it took to produce (LLM call time for
    // assistant messages, tool execution time for tool results) and the
    // token usage from the LLM response that produced it.
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS duration_ms INT NOT NULL DEFAULT 0")
        .execute(pool)
        .await
        .ok();
    // Ensure NOT NULL even if column already existed (idempotent)
    sqlx::query("ALTER TABLE messages ALTER COLUMN duration_ms SET NOT NULL")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS token_usage JSONB DEFAULT '{}'")
        .execute(pool)
        .await
        .ok();

    // ── Migrate planning_mode string to plan boolean ─────────────────────
    // Add plan column to threads, backfill from planning_mode
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS plan BOOLEAN NOT NULL DEFAULT false")
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "UPDATE threads SET plan = true WHERE planning_mode IN ('auto_plan', 'auto_subtasks', 'always')"
    )
    .execute(pool)
    .await
    .ok();

    // Add plan column to kanban_tasks
    sqlx::query(
        "ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS plan BOOLEAN NOT NULL DEFAULT false",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "UPDATE kanban_tasks SET plan = true WHERE planning_mode IN ('auto_plan', 'auto_subtasks', 'always')"
    )
    .execute(pool)
    .await
    .ok();

    // Add plan column to cron_jobs (dormant table, back-compat only)
    sqlx::query(
        "ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS plan BOOLEAN NOT NULL DEFAULT false",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "UPDATE cron_jobs SET plan = true WHERE planning_mode IN ('auto_plan', 'auto_subtasks', 'always')"
    )
    .execute(pool)
    .await
    .ok();

    // ── Drop legacy planning_mode columns ────────────────────────────────
    // Normalized to the single `plan` bool: the TEXT duplicate is gone from
    // the schema. Order-independent vs the dormant cron_jobs/hooks tables.
    sqlx::query("ALTER TABLE threads DROP COLUMN IF EXISTS planning_mode")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS planning_mode")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE cron_jobs DROP COLUMN IF EXISTS planning_mode")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE hooks DROP COLUMN IF EXISTS planning_mode")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE channels DROP COLUMN IF EXISTS planning_mode")
        .execute(pool)
        .await
        .ok();

    // ── Add per-message duration and token tracking ─────────────────────
    // Each message stores the time it took to produce (LLM call time for
    // assistant messages, tool execution time for tool results) and the
    // token usage from the LLM response that produced it.
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS duration_ms INT NOT NULL DEFAULT 0")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS token_usage JSONB DEFAULT '{}'")
        .execute(pool)
        .await
        .ok();

    // ── Inbound dedup: prevent duplicate threads for the same platform post ──
    // messages.channel_id denormalizes threads.channel_id so we can enforce
    // per-channel uniqueness of seq-0 external_ids. New inserts populate it
    // via subquery (see db/threads.rs + db/messages.rs); the partial unique
    // index makes double-thread creation impossible even under concurrent
    // delivery (websocket + polling overlap, restart catch-up re-scan).
    // Note: no backfill UPDATE here — messages is append-only (trigger
    // trg_messages_append_only blocks UPDATE); existing rows keep NULL
    // channel_id and the index simply doesn't cover them (NULLs are distinct
    // in btree unique indexes), so enforcement applies to new inserts only.
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS channel_id TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_seq0_external_id \
         ON messages (channel_id, external_id) \
         WHERE thread_sequence = 0 AND external_id IS NOT NULL",
    )
    .execute(pool)
    .await
    .ok();

    // -- Sub-prompts: append pending user prompts to a running thread ------
    // messages.original_thread_id: the pending thread id whose prompt was
    // appended into this (running) thread as a sub-prompt and which was then
    // marked skipped. NULL for ordinary messages. msg_subtype carries the
    // same id as a human-readable reference (per feature spec).
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS original_thread_id BIGINT")
        .execute(pool)
        .await
        .ok();

    tracing::info!(
        "[migration] Schema v5: messages.channel_id + seq-0 external_id dedup index added"
    );

    // ── Workflow implementation (Phase 0): schema additions ────────────────
    // kanban_tasks: workflow_id = workflow key (NO FK — workflows are
    // file-defined, decision N4), thread_status = lifecycle state of the
    // workflow-managed thread (NULL | scheduled | running), workflow_state =
    // execution JSONB, e.g. {"executions": {"running": N, "testing": M, "review": K}}.
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS workflow_id TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS thread_status TEXT")
        .execute(pool)
        .await
        .ok();
    // thread_status CHECK (idempotent DO block, matching the chk_thread_cause pattern).
    sqlx::query(
        "DO $$ BEGIN \
         IF NOT EXISTS (SELECT 1 FROM pg_constraint \
                        WHERE conname = 'chk_kanban_tasks_thread_status') \
         THEN ALTER TABLE kanban_tasks ADD CONSTRAINT chk_kanban_tasks_thread_status \
              CHECK (thread_status IS NULL OR thread_status IN ('scheduled', 'running')); \
         END IF; END $$;",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS workflow_state JSONB")
        .execute(pool)
        .await
        .ok();

    // threads: workflow_id + workflow_step (STEP keys only — running/testing/review,
    // NEVER role names; roles are role/display names only, N5) + task_type
    // ('kanban' | 'cron'). task_id already exists — no task_type backfill (N7).
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS workflow_id TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS workflow_step TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS task_type TEXT")
        .execute(pool)
        .await
        .ok();

    // kanban_history: free-text comment on history entries.
    sqlx::query("ALTER TABLE kanban_history ADD COLUMN IF NOT EXISTS comment TEXT")
        .execute(pool)
        .await
        .ok();

    // ── R4: retire the legacy 'ready' status ───────────────────────────────
    // Pre-existing 'ready' tasks become 'running' (workflow semantics):
    // - with a pending thread -> thread_status = 'scheduled' (the thread is a
    //   scheduled workflow execution)
    // - without one -> thread_status stays NULL
    // Future 'ready' writes are rejected at validation (src/server/kanban.rs).
    sqlx::query(
        "UPDATE kanban_tasks SET status = 'running', thread_status = 'scheduled' \
         WHERE status = 'ready' \
           AND EXISTS (SELECT 1 FROM threads t WHERE t.task_id = kanban_tasks.id \
                       AND t.status = 'pending')",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "UPDATE kanban_tasks SET status = 'running', thread_status = NULL \
         WHERE status = 'ready' \
           AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.task_id = kanban_tasks.id \
                           AND t.status = 'pending')",
    )
    .execute(pool)
    .await
    .ok();

    tracing::info!(
        "[migration] Schema v6: workflow columns (kanban_tasks.workflow_id/thread_status/workflow_state, threads.workflow_id/workflow_step/task_type, kanban_history.comment) + R4 'ready' retirement"
    );

    // ── R7: task template as a first-class thread field ────────────────────
    // threads.template: the task template resolved at thread-creation time by
    // the creator (kanban dispatcher / scheduler / platform user-message path).
    // The execution loop reads it uniformly from the threads table (or the
    // seq-0 cause metadata) for ALL agent executions — no task-type-specific
    // template lookups (owner architecture rule).
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS template TEXT DEFAULT ''")
        .execute(pool)
        .await
        .ok();

    tracing::info!(
        "[migration] Schema v7: threads.template (task template as a first-class thread field)"
    );

    // ── Removed tables: channel_subscriptions + channel_stops ──────────────
    // The cross-channel summary-forwarding feature is removed: messages
    // (including summaries) are delivered ONLY to their own channel. Both
    // tables are gone from the declarative schema above; these idempotent
    // DROPs clean up databases created before the removal (safe to run on
    // every startup).
    sqlx::query("DROP TABLE IF EXISTS channel_subscriptions")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DROP TABLE IF EXISTS channel_stops")
        .execute(pool)
        .await
        .ok();
    tracing::info!(
        "[migration] Dropped channel_subscriptions + channel_stops (cross-channel summary forwarding removed)"
    );
    // ── Removed tables: cron_jobs + hooks (tasks.yml is now the source) ────
    // Definitions moved to {data_dir}/config/tasks.yml (`schedules:` and
    // `hooks:` keys); these idempotent DROPs clean up databases created
    // before the move (safe to run on every startup). Runtime state tables
    // hook_counters + task_runs are kept.
    sqlx::query("DROP TABLE IF EXISTS cron_jobs")
        .execute(pool)
        .await
        .ok();
    sqlx::query("DROP TABLE IF EXISTS hooks")
        .execute(pool)
        .await
        .ok();
    tracing::info!("[migration] Dropped cron_jobs + hooks (definitions now in config/tasks.yml)");

    // ── Terminal status invariant ──────────────────────────────────────────
    // Every thread in a terminal status (skipped/completed/failed/interrupted/
    // system) MUST have terminal=true — enforced structurally so a terminal
    // row can never look like active work to code checking `terminal` (e.g. a
    // dispatch gate `WHERE terminal = false` would block a channel forever).
    // Backfill FIRST: pre-existing bad rows (e.g. operator-stop skips written
    // before the invariant) would make ADD CONSTRAINT fail.
    sqlx::query(
        "UPDATE threads SET terminal = true \
         WHERE status IN ('skipped', 'completed', 'failed', 'interrupted', 'system') \
           AND NOT terminal",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        r#"DO $$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'chk_thread_terminal_status') THEN
                ALTER TABLE threads ADD CONSTRAINT chk_thread_terminal_status
                    CHECK (status NOT IN ('skipped', 'completed', 'failed', 'interrupted', 'system') OR terminal = true);
            END IF;
        END $$;"#,
    )
    .execute(pool)
    .await
    .ok();
    tracing::info!(
        "[migration] Terminal status invariant: threads backfilled + CHECK constraint chk_thread_terminal_status added"
    );

    // ── Goal state machine (durable phase + typed blocked reason + round cap) ──
    // Per-task goal state (omnidev task 4): goal_phase
    // (active/paused/blocked/complete), a stable machine-routable
    // goal_blocked_code (kebab-case) + human goal_blocked_message, an optional
    // goal_max_rounds cap, and a CAS revision counter (goal_revision). All
    // columns are NULL except goal_revision (NOT NULL DEFAULT 0) — a task with
    // NULL goal_phase has no goal state (zero behavior change for tasks that
    // never use goals). Goals are strictly per-task state: the sequential
    // per-channel dispatch model (threads.status gate in kanban_dispatch.rs)
    // is untouched.
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS goal_phase TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS goal_blocked_code TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS goal_blocked_message TEXT")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS goal_max_rounds INT")
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS goal_revision INT NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await
    .ok();
    // goal_phase CHECK (idempotent DO block, matching the
    // chk_kanban_tasks_thread_status pattern).
    sqlx::query(
        "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'chk_kanban_tasks_goal_phase') THEN ALTER TABLE kanban_tasks ADD CONSTRAINT chk_kanban_tasks_goal_phase CHECK (goal_phase IS NULL OR goal_phase IN ('active', 'paused', 'blocked', 'complete')); END IF; END $$;",
    )
    .execute(pool)
    .await
    .ok();
    tracing::info!(
        "[migration] Schema v8: goal state columns (kanban_tasks.goal_phase/goal_blocked_code/goal_blocked_message/goal_max_rounds/goal_revision) + chk_kanban_tasks_goal_phase CHECK"
    );

    Ok(())
}

// ── Extensions ──────────────────────────────────────────────────────────────

async fn create_extensions(pool: &PgPool) -> Result<()> {
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .execute(pool)
        .await?;

    // pgvector is optional: silently skip if not installed
    sqlx::query(
        r#"DO $$ BEGIN
            CREATE EXTENSION IF NOT EXISTS vector;
        EXCEPTION WHEN OTHERS THEN
            -- vector extension not available, continue without it
        END $$;"#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

// ── Tables ──────────────────────────────────────────────────────────────────

async fn create_tables(pool: &PgPool) -> Result<()> {
    // ── Channels ──────────────────────────────────────────────────────────
    // Channels: moved to {data_dir}/config/channels.yml (no DB table).
    // Channel definitions AND runtime state live in channels.yml; dependent
    // tables keep a `channel_id` TEXT column holding the channel NAME
    // (the yml key) -- same pattern as threads.schedule_task_id /
    // threads.workflow_id / threads.task_id referencing yml keys.

    // ── Threads ───────────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS threads (
            id                BIGSERIAL PRIMARY KEY,
            status            TEXT NOT NULL DEFAULT 'created',
            cause             TEXT NOT NULL,
            channel_id        TEXT NOT NULL,
            profile           TEXT NOT NULL,
            provider          TEXT,
            model             TEXT,
            input_tokens      INT DEFAULT 0,
            cached_tokens     INT DEFAULT 0,
            output_tokens     INT DEFAULT 0,
            duration_ms       INT DEFAULT 0,
            created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            started_at        TIMESTAMPTZ,
            ended_at          TIMESTAMPTZ,
            terminal          BOOLEAN NOT NULL DEFAULT false,
            task_id           TEXT,
            schedule_task_id  TEXT,
            parent_id         BIGINT REFERENCES threads(id),
            iterations        INT NOT NULL DEFAULT 0
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Messages ──────────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id                BIGSERIAL PRIMARY KEY,
            role              TEXT NOT NULL,
            content           TEXT NOT NULL,
            thread_id         BIGINT NOT NULL REFERENCES threads(id),
            thread_sequence   INT NOT NULL,
            external_id       TEXT,
            metadata          JSONB DEFAULT '{}',
            embedding         TEXT,
            summary_text      TEXT,
            is_summary        BOOL NOT NULL DEFAULT false,
            created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            msg_type          TEXT NOT NULL DEFAULT 'message',
            msg_subtype       TEXT,
            iteration_number  INT NOT NULL DEFAULT 0
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Kanban tasks ──────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_tasks (
            id              TEXT PRIMARY KEY,
            title           TEXT NOT NULL,
            body            TEXT DEFAULT '',
            status          TEXT NOT NULL DEFAULT 'backlog',
            priority        INTEGER DEFAULT 0,
            assignee        TEXT DEFAULT '',
            channel_id      TEXT,
            profile         TEXT,
            archived        BOOLEAN NOT NULL DEFAULT false,
            position        INTEGER,
            template        TEXT DEFAULT '',
            created_at      TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            updated_at      TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Kanban dependencies ───────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_task_dependencies (
            task_id       TEXT NOT NULL REFERENCES kanban_tasks(id) ON DELETE CASCADE,
            depends_on_id TEXT NOT NULL REFERENCES kanban_tasks(id) ON DELETE CASCADE,
            created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (task_id, depends_on_id)
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Kanban history ────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_history (
            id              BIGSERIAL PRIMARY KEY,
            kanban_task_id  TEXT NOT NULL,
            action          TEXT NOT NULL,
            initial_board   TEXT,
            final_board     TEXT,
            previous_values JSONB,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Cron jobs ─────────────────────────────────────────────────────────
    // NOTE (tasks.yml): definitions now live in {data_dir}/config/tasks.yml
    // (`schedules:` key); this table is kept dormant for back-compat and is
    // no longer read for definitions. Cadence bookkeeping moved to `task_runs`.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cron_jobs (
            id                TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            display_name      TEXT NOT NULL DEFAULT '',
            schedule          TEXT NOT NULL,
            prompt            TEXT NOT NULL DEFAULT '',
            skills            TEXT DEFAULT '[]',
            enabled           BOOLEAN DEFAULT true,
            last_run_at       TIMESTAMP WITH TIME ZONE,
            next_run_at       TIMESTAMP WITH TIME ZONE,
            created_at        TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            updated_at        TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            mode              TEXT NOT NULL DEFAULT 'agentic',
            direct_task_type  TEXT DEFAULT NULL,
            active            BOOLEAN NOT NULL DEFAULT true,
            channel_id        TEXT,
            profile           TEXT,
            running           BOOLEAN NOT NULL DEFAULT false,
            action_id         TEXT,
            silent            BOOLEAN NOT NULL DEFAULT false,
            template          TEXT DEFAULT '',
            plan              BOOLEAN NOT NULL DEFAULT false
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Summaries ─────────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS summaries (
            id              BIGSERIAL PRIMARY KEY,
            channel_id      TEXT NOT NULL,
            next_thread_id  BIGINT NOT NULL,
            content         TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Thread subtasks ───────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS thread_subtasks (
            id          BIGSERIAL PRIMARY KEY,
            thread_id   BIGINT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            description TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'pending',
            priority    INTEGER DEFAULT 0,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Secrets ───────────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS secrets (
            id              BIGSERIAL PRIMARY KEY,
            name            VARCHAR(255) NOT NULL UNIQUE,
            field_type      VARCHAR(20) NOT NULL DEFAULT 'password',
            current_value   TEXT NOT NULL DEFAULT '',
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Secret versions ───────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS secret_versions (
            id              BIGSERIAL PRIMARY KEY,
            secret_id       BIGINT NOT NULL REFERENCES secrets(id) ON DELETE CASCADE,
            version_number  INT NOT NULL,
            value           TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(secret_id, version_number)
        );
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!("[migration] All tables created");
    Ok(())
}

// ── Indexes ─────────────────────────────────────────────────────────────────

async fn create_indexes(pool: &PgPool) -> Result<()> {
    // Messages: thread ordering (replaces dropped UNIQUE constraint)
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_thread_seq
            ON messages(thread_id, thread_sequence);
        "#,
    )
    .execute(pool)
    .await?;

    // Messages: trigram full-text search
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_content_trgm
            ON messages USING gin (content gin_trgm_ops);
        "#,
    )
    .execute(pool)
    .await?;

    // Messages: recency sort for vector search fallback
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_created_at
            ON messages(created_at DESC);
        "#,
    )
    .execute(pool)
    .await?;

    // Threads: channel + status queries
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_threads_channel_status
            ON threads(channel_id, status);
        "#,
    )
    .execute(pool)
    .await?;

    // Threads: schedule task lookup
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_threads_schedule_task_id
            ON threads(schedule_task_id);
        "#,
    )
    .execute(pool)
    .await?;

    // Threads: parent-child tree queries
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_threads_parent_id
            ON threads(parent_id);
        "#,
    )
    .execute(pool)
    .await?;

    // Subtasks: per-thread lookup
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_thread_subtasks_thread_id
            ON thread_subtasks(thread_id);
        "#,
    )
    .execute(pool)
    .await?;

    // Secret versions: per-secret lookup
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_secret_versions_secret_id
            ON secret_versions(secret_id);
        "#,
    )
    .execute(pool)
    .await?;

    // Threads: cause CHECK constraint
    sqlx::query(
        r#"DO $$ BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conname = 'chk_thread_cause'
            ) THEN
                ALTER TABLE threads ADD CONSTRAINT chk_thread_cause
                    CHECK (cause IN ('user', 'system'));
            END IF;
        END $$;"#,
    )
    .execute(pool)
    .await?;

    tracing::info!("[migration] All indexes created");
    Ok(())
}

// ── Vector support (conditional on pgvector) ────────────────────────────────

async fn create_vector_support(pool: &PgPool) -> Result<()> {
    let vector_available: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(pool)
            .await
            .unwrap_or(false);

    if vector_available {
        sqlx::query(
            r#"
            ALTER TABLE messages
            ADD COLUMN IF NOT EXISTS embedding_vec vector(1536);
            "#,
        )
        .execute(pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_messages_embedding_vec_hnsw
            ON messages USING hnsw (embedding_vec vector_cosine_ops);
            "#,
        )
        .execute(pool)
        .await?;

        tracing::info!("[migration] pgvector HNSW index and embedding_vec column ready");
    } else {
        tracing::warn!("[migration] pgvector not available: skipping vector column");
    }

    Ok(())
}

// ── Triggers ────────────────────────────────────────────────────────────────

async fn create_triggers(pool: &PgPool) -> Result<()> {
    // Append-only guard on messages:
    //   - DELETE is always blocked
    //   - UPDATE allowed only if only embedding_vec or external_id changed
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION prevent_message_mutation()
        RETURNS TRIGGER AS $$
        BEGIN
            IF TG_OP = 'DELETE' THEN
                RAISE EXCEPTION 'messages is append-only. Deletion of messages is not permitted.';
            END IF;

            -- Allow UPDATE if only embedding_vec changed (vectorizer)
            IF NEW.embedding_vec IS DISTINCT FROM OLD.embedding_vec THEN
                IF NEW.id = OLD.id
                   AND NEW.role IS NOT DISTINCT FROM OLD.role
                   AND NEW.content IS NOT DISTINCT FROM OLD.content
                   AND NEW.thread_id IS NOT DISTINCT FROM OLD.thread_id
                   AND NEW.thread_sequence IS NOT DISTINCT FROM OLD.thread_sequence
                   AND NEW.external_id IS NOT DISTINCT FROM OLD.external_id
                   AND NEW.metadata IS NOT DISTINCT FROM OLD.metadata
                   AND NEW.embedding IS NOT DISTINCT FROM OLD.embedding
                   AND NEW.summary_text IS NOT DISTINCT FROM OLD.summary_text
                   AND NEW.is_summary IS NOT DISTINCT FROM OLD.is_summary
                   AND NEW.msg_type IS NOT DISTINCT FROM OLD.msg_type
                   AND NEW.msg_subtype IS NOT DISTINCT FROM OLD.msg_subtype
                   AND NEW.iteration_number IS NOT DISTINCT FROM OLD.iteration_number
                THEN
                    RETURN NEW;
                END IF;
            END IF;

            -- Allow UPDATE if only external_id changed (platform post-back)
            IF NEW.external_id IS DISTINCT FROM OLD.external_id THEN
                IF NEW.id = OLD.id
                   AND NEW.role IS NOT DISTINCT FROM OLD.role
                   AND NEW.content IS NOT DISTINCT FROM OLD.content
                   AND NEW.thread_id IS NOT DISTINCT FROM OLD.thread_id
                   AND NEW.thread_sequence IS NOT DISTINCT FROM OLD.thread_sequence
                   AND NEW.embedding_vec IS NOT DISTINCT FROM OLD.embedding_vec
                   AND NEW.metadata IS NOT DISTINCT FROM OLD.metadata
                   AND NEW.embedding IS NOT DISTINCT FROM OLD.embedding
                   AND NEW.summary_text IS NOT DISTINCT FROM OLD.summary_text
                   AND NEW.is_summary IS NOT DISTINCT FROM OLD.is_summary
                   AND NEW.msg_type IS NOT DISTINCT FROM OLD.msg_type
                   AND NEW.msg_subtype IS NOT DISTINCT FROM OLD.msg_subtype
                   AND NEW.iteration_number IS NOT DISTINCT FROM OLD.iteration_number
                THEN
                    RETURN NEW;
                END IF;
            END IF;

            -- Allow content UPDATE for pending threads (message editing on platform)
            IF NEW.content IS DISTINCT FROM OLD.content THEN
                IF NEW.id = OLD.id
                   AND NEW.role IS NOT DISTINCT FROM OLD.role
                   AND NEW.thread_id IS NOT DISTINCT FROM OLD.thread_id
                   AND NEW.thread_sequence IS NOT DISTINCT FROM OLD.thread_sequence
                   AND NEW.external_id IS NOT DISTINCT FROM OLD.external_id
                   AND NEW.metadata IS NOT DISTINCT FROM OLD.metadata
                   AND NEW.embedding_vec IS NOT DISTINCT FROM OLD.embedding_vec
                   AND NEW.embedding IS NOT DISTINCT FROM OLD.embedding
                   AND NEW.summary_text IS NOT DISTINCT FROM OLD.summary_text
                   AND NEW.is_summary IS NOT DISTINCT FROM OLD.is_summary
                   AND NEW.msg_type IS NOT DISTINCT FROM OLD.msg_type
                   AND NEW.msg_subtype IS NOT DISTINCT FROM OLD.msg_subtype
                   AND NEW.iteration_number IS NOT DISTINCT FROM OLD.iteration_number
                   AND EXISTS (SELECT 1 FROM threads t WHERE t.id = NEW.thread_id AND t.status = 'pending')
                THEN
                    RETURN NEW;
                END IF;
            END IF;

            RAISE EXCEPTION 'messages is immutable after insert. Only embedding_vec (vectorizer), external_id (platform post-back), and content (pending thread edits) may be updated. Other columns cannot change.';
        END;
        $$ LANGUAGE plpgsql;
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        DROP TRIGGER IF EXISTS trg_messages_append_only ON messages;
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER trg_messages_append_only
            BEFORE UPDATE OR DELETE ON messages
            FOR EACH ROW EXECUTE FUNCTION prevent_message_mutation();
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!("[migration] Append-only trigger created on messages");
    Ok(())
}

// ── Channels moved to {data_dir}/config/channels.yml ────────────────────────
// The `channels` table AND ALL FOREIGN KEYS REFERENCING IT are dropped.
// Dependent tables keep their `channel_id` column -- RETYPED from BIGINT to
// TEXT, now holding the channel NAME (the channels.yml key) instead of a
// DB-generated id. Order-independent vs the (already removed)
// cron_jobs/hooks/channel_stops/channel_subscriptions tables: the FK drop
// iterates pg_constraint dynamically, so it works whether or not those
// tables still exist.

async fn migrate_channels_to_yml(pool: &PgPool) -> Result<()> {
    // 1. Drop every FK referencing the channels table (dynamic: works no
    //    matter which dependent tables still exist).
    sqlx::query(
        r#"
        DO $$
        DECLARE
            r record;
        BEGIN
            IF to_regclass('public.channels') IS NOT NULL THEN
                FOR r IN
                    SELECT conname, conrelid::regclass AS tbl
                    FROM pg_constraint
                    WHERE contype = 'f' AND confrelid = 'channels'::regclass
                LOOP
                    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', r.tbl, r.conname);
                END LOOP;
            END IF;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // 2. Retype channel_id BIGINT -> TEXT, backfilling with the channel NAME.
    //    Conditional on data_type='bigint' (fresh installs already have TEXT).
    //    Nullability is preserved: threads/summaries stay NOT NULL (backfill
    //    must succeed), messages/kanban_tasks stay nullable.
    for (tbl, not_null) in [
        ("threads", true),
        ("messages", false),
        ("kanban_tasks", false),
        ("summaries", true),
    ] {
        let not_null_sql = if not_null {
            format!("\n                    ALTER TABLE {tbl} ALTER COLUMN channel_id SET NOT NULL;")
        } else {
            String::new()
        };
        let swap = format!(
            r#"
            DO $$
            BEGIN
                IF EXISTS (
                    SELECT 1 FROM information_schema.columns
                    WHERE table_name = '{tbl}' AND column_name = 'channel_id'
                      AND data_type = 'bigint'
                ) AND to_regclass('public.channels') IS NOT NULL THEN
                    ALTER TABLE {tbl} ADD COLUMN IF NOT EXISTS channel_name TEXT;
                    ALTER TABLE {tbl} DISABLE TRIGGER USER;
                    UPDATE {tbl} SET channel_name = c.name
                    FROM channels c
                    WHERE c.id = {tbl}.channel_id;
                    ALTER TABLE {tbl} ENABLE TRIGGER USER;
                    ALTER TABLE {tbl} DROP COLUMN channel_id;
                    ALTER TABLE {tbl} RENAME COLUMN channel_name TO channel_id;{not_null_sql}
                END IF;
            END $$;
            "#
        );
        sqlx::query(sqlx::AssertSqlSafe(swap.as_str()))
            .execute(pool)
            .await?;
    }

    // 3. The channels table itself is gone; channels.yml is the single source.
    sqlx::query("DROP TABLE IF EXISTS channels")
        .execute(pool)
        .await?;

    // 4. Recreate the messages seq-0 dedup index for the TEXT column (the
    //    old BIGINT index was dropped together with the column).
    sqlx::query("DROP INDEX IF EXISTS uq_messages_seq0_external_id")
        .execute(pool)
        .await?;
    // Fresh installs have no messages.channel_id yet (it is added by the
    // schema-v5 step later in run()) — ensure it exists first, otherwise
    // the CREATE INDEX below fails with "column channel_id does not exist".
    sqlx::query("ALTER TABLE messages ADD COLUMN IF NOT EXISTS channel_id TEXT")
        .execute(pool)
        .await?;
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_seq0_external_id
        ON messages (channel_id, external_id)
        WHERE thread_sequence = 0 AND external_id IS NOT NULL
        "#,
    )
    .execute(pool)
    .await?;

    // 5. Recreate the threads channel-status index for the TEXT column.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_threads_channel_status ON threads (channel_id, status)",
    )
    .execute(pool)
    .await?;

    tracing::info!(
        "[migration] Channels moved to config/channels.yml; channels table + FKs dropped"
    );
    Ok(())
}
