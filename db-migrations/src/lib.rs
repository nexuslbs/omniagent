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

/// Hosts treated as "known dev targets": the DB-write guard allows schema
/// writes from a DEV-BUILT binary to these hosts. Everything else is treated
/// as a production/live database and REFUSED for dev builds.
///
/// Release-built images (OMNIAGENT_BUILD_MODE=release baked at build time by
/// the release pipeline) are NOT subject to this list: the operator
/// explicitly chose to run that image, and the declarative schema is
/// idempotent (CREATE TABLE IF NOT EXISTS ...), so release images
/// auto-apply migrations on container start (no manual env vars).
///
/// NOTE: the bare compose service name `postgres` is deliberately NOT in this
/// list - the production omni-stack uses exactly that host in its
/// DATABASE_URL, so allowing it would let a dev binary write to prod. The dev
/// overlay (docker-compose.dev.yml) gives the omnidev postgres the dev-only
/// alias `omnidev-postgres` and points the dev DATABASE_URL at it.
const KNOWN_DEV_DB_HOSTS: [&str; 5] = [
    "localhost",
    "127.0.0.1",
    "::1",
    "omnidev-postgres", // omnidev dev overlay network alias (docker-compose.dev.yml)
    "omnidev-postgres-1", // omnidev postgres container name (docker exec workflows)
];

/// True when this binary was built in RELEASE mode: the release image build
/// (publish pipeline) passes `--build-arg OMNIAGENT_BUILD_MODE=release` to the
/// production Dockerfile, which bakes it as ENV. Release-built images
/// auto-apply the idempotent declarative schema on container start against
/// ANY database - the operator explicitly chose to run that image, so the
/// dev-host restriction does not apply. Dev builds (Dockerfile.dev, plain
/// `cargo run`, `docker build` without the release arg) have no marker (or
/// OMNIAGENT_BUILD_MODE=dev) and stay fully guarded (fail closed).
fn is_release_build() -> bool {
    is_release_build_mode(std::env::var("OMNIAGENT_BUILD_MODE").ok().as_deref())
}

/// Pure helper (unit-testable): a build mode is "release" ONLY for the exact
/// string "release". Anything else - absent, "dev", "development", "test",
/// "local" - counts as a dev build (fail closed).
fn is_release_build_mode(value: Option<&str>) -> bool {
    matches!(value, Some("release"))
}

/// Guard: refuse to auto-apply declarative schema to a database that is not a
/// known dev target when the BINARY IS DEV-BUILT. There is no env-var
/// override (the earlier DB-write env escape hatch was removed; none exists now).
///
/// Decision table (checked in order):
///   1. OMNIAGENT_BUILD_MODE=release (baked)  -> allow (release image
///      auto-applies the idempotent schema on start - no manual env vars)
///   2. DB host in KNOWN_DEV_DB_HOSTS         -> allow (dev build, dev DB)
///   3. anything else                         -> REFUSE (dev build pointed at
///      a non-dev / production database)
///
/// WHY: omniagent migrations are declarative and auto-run at every startup
/// (CREATE TABLE IF NOT EXISTS ..., no schema_migrations versioning), so a
/// dev-built binary pointed at the production postgres silently creates
/// tables in the live DB before the feature is even committed. Incident
/// 2026-08-27/28: the kanban-tags dev workflow created kanban_tags/task_tags
/// in the PROD omni-stack DB (172.18.0.4:5432) via `cargo sqlx prepare` +
/// live API verification. This guard makes that fail loudly instead.
/// Release-built images skip the dev-host check: building a release image is
/// an explicit operator action and the schema is idempotent, so running it
/// against the production DB on upgrade is exactly what a version upgrade
/// must do (no manual env vars).
fn guard_db_write_allowed() -> Result<()> {
    // 1. Release-built image: the operator chose to run it; auto-apply the
    //    idempotent declarative schema on container start (version upgrades).
    if is_release_build() {
        return Ok(());
    }
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            // No URL in the environment: the pool was supplied programmatically
            // by the caller, nothing to guard against.
            return Ok(());
        }
    };
    let host = db_host(&database_url)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if KNOWN_DEV_DB_HOSTS.contains(&host.as_str()) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "refusing to auto-apply schema to non-dev database (host '{}'): this \
         binary is DEV-built (no OMNIAGENT_BUILD_MODE=release marker), so \
         db-migrations must run against the omnidev dev postgres only (known \
         dev hosts: {}); NEVER against the omni-stack production DB. Build a \
         RELEASE image (--build-arg OMNIAGENT_BUILD_MODE=release) to \
         auto-apply migrations on upgrade (no env vars, no override).",
        if host.is_empty() { "<unknown>" } else { &host },
        KNOWN_DEV_DB_HOSTS.join(", "),
    ))
}

/// Extract the host portion of a postgres:// DATABASE_URL.
/// Handles: postgres://user:pass@host:5432/db, @host/db, [::1]:5432, and the
/// unix-socket form postgres:///dbname (returns None -> empty host).
fn db_host(database_url: &str) -> Option<&str> {
    let rest = database_url.split_once("://")?.1;
    let rest = match rest.find('@') {
        Some(idx) => &rest[idx + 1..],
        None => rest,
    };
    let host_port = match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    };
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

pub async fn run(pool: &PgPool) -> Result<()> {
    guard_db_write_allowed()?;
    create_extensions(pool).await?;
    create_tables(pool).await?;
    create_indexes(pool).await?;
    create_vector_support(pool).await?;
    create_search_support(pool).await?;
    create_triggers(pool).await?;
    backfill_thread_end_usage_messages(pool).await?;
    backfill_terminal_thread_usage_aggregates(pool).await?;
    migrate_channels_to_yml(pool).await?;
    assert_retention_regression_guards(pool).await?;

    // -- Kanban boards (config/boards.yml) --
    // Nullable `board` column on kanban_tasks: NULL = no board. Boards are
    // ALWAYS enabled (src/boards.rs): a task's board is always validated against
    // the effective board set (config/boards.yml when present, else the built-in
    // default set) and a missing file never disables boards; only pre-existing
    // rows keep NULL until they are edited. Board deletion removes its tasks via
    // the board-delete API handler (per-task cleanup mirrors the existing
    // task-delete behavior).
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS board TEXT")
        .execute(pool)
        .await
        .ok();

    // -- Orchestration-task budget overrides (task tier) --
    // Nullable TASK-tier columns for the two orchestration knobs enforced by
    // `src/agent/orchestration_budget.rs`: NULL = the task declares nothing and
    // the other tiers apply (workflow/role > task > board > global setting).
    // Idempotent ADD COLUMN IF NOT EXISTS, like every other migration here.
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS token_budget BIGINT")
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS iteration_min_interval_secs BIGINT",
    )
    .execute(pool)
    .await
    .ok();

    // ── Kanban tags (kanban task tags) ─────────────────────────────────────
    // kanban_tags: free-form label registry (one row per unique tag name).
    // task_tags: task <-> tag association (FK CASCADE: deleting a task or a
    // tag cleans up its links). Tag add/remove operations write durable
    // kanban_history entries ('tag_added' / 'tag_removed').
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_tags (
            id         BIGSERIAL PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS task_tags (
            task_id    TEXT NOT NULL REFERENCES kanban_tasks(id) ON DELETE CASCADE,
            tag_id     BIGINT NOT NULL REFERENCES kanban_tags(id) ON DELETE CASCADE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (task_id, tag_id)
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_task_tags_tag_id ON task_tags (tag_id)")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tags ADD COLUMN IF NOT EXISTS color TEXT")
        .execute(pool)
        .await
        .ok();
    tracing::info!("[migration] Kanban tags tables (kanban_tags, task_tags) added");

    // -- Event-driven Hooks (thread_started / new_message / thread_completed /
    //    thread_interrupted / thread_failed / thread_skipped / thread_merged /
    //    thread_terminated) --
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
    // The event set is EXTENDED over time (terminal lifecycle events), so the
    // event constraint is DROPPED and RE-ADDED on every start: CREATE ... IF
    // NOT EXISTS alone would leave an old, narrower constraint in place.
    sqlx::query(
        r#"
        ALTER TABLE hooks DROP CONSTRAINT IF EXISTS hooks_event_chk;
        ALTER TABLE hooks ADD CONSTRAINT hooks_event_chk
            CHECK (event IN ('thread_started', 'new_message', 'thread_completed',
                             'thread_interrupted', 'thread_failed', 'thread_skipped',
                             'thread_merged', 'thread_terminated'));
        "#,
    )
    .execute(pool)
    .await
    .ok();

    // Idempotent CHECK constraints (scope/mode/count value validation).
    sqlx::query(
        r#"
        DO $$
        BEGIN
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

    // hook_counters: runtime hook counter state - one JSON counter per hook
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

    // task_runs: scheduler cadence bookkeeping - one last-fired timestamp per
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

    // schedule_runs: one row per schedule FIRE (manual or cron) with its
    // terminal outcome (status/exit_code/output tail). Forced runs stay
    // observable through the API even when the action takes minutes: the
    // HTTP trigger returns immediately and the outcome lands here.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS schedule_runs (
            run_id      TEXT PRIMARY KEY,
            task_key    TEXT NOT NULL,
            trigger     TEXT NOT NULL DEFAULT 'manual',
            status      TEXT NOT NULL DEFAULT 'running',
            started_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            finished_at TIMESTAMPTZ,
            exit_code   INT,
            output      TEXT,
            thread_id   BIGINT,
            error       TEXT
        );
        "#,
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_schedule_runs_task_key ON schedule_runs (task_key, started_at DESC)",
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
    // Note: no backfill UPDATE here - messages is append-only (trigger
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

    // Messages: sub-cause reverse lookup (threads list "merged_into_thread_id":
    // WHERE msg_type='sub_cause' AND original_thread_id = ?). Without it the
    // correlated subquery sequentially scans the whole messages table per list
    // row (3.6 GB / 107k rows in production -> ~1.4 s for a 50-row page).
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_subcause_original_thread
            ON messages(original_thread_id) WHERE msg_type = 'sub_cause';
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!(
        "[migration] Schema v5: messages.channel_id + seq-0 external_id dedup index added"
    );

    // ── Workflow implementation (Phase 0): schema additions ────────────────
    // kanban_tasks: workflow_id = workflow key (NO FK - workflows are
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

    // threads: workflow_id + workflow_step (STEP keys only - running/testing/review,
    // NEVER role names; roles are role/display names only, N5) + task_type
    // ('kanban' | 'cron'). task_id already exists - no task_type backfill (N7).
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
    // seq-0 cause metadata) for ALL agent executions - no task-type-specific
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
    // system) MUST have terminal=true - enforced structurally so a terminal
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
    // ── Data repair: threads.input_tokens stored as the TOTAL prompt ────────
    // `threads.input_tokens` means the CACHE-MISS input only
    // (`SUM(prompt_tokens - cached_tokens)`; operator UPDATE 2026-10-02,
    // telegram thread 3915). Two write paths stored the raw `prompt_tokens`
    // instead - `mark_thread_terminal` (skipped / interrupted / system / merged
    // threads) and `update_thread_progress` (the live per-LLM-call write) - so
    // every surface deriving the cache share as `cached / (cached + input)`
    // (dashboard threads list, overview token trend and its day totals)
    // under-reported those rows: on the dev DB 22 rows carried 101,487,333
    // stored input where the true cache-miss total is ~2.9M - the operator's
    // "the cache tokens is so low" report (telegram thread 4136, 2026-10-05).
    // Both write paths are fixed in code; this repairs the rows they already
    // wrote. GUARD = the exact buggy signature: the stored value EQUALS the
    // message total prompt AND DIFFERS from the message cache-miss total. A
    // correctly written row (prompt - cached) can only match the first
    // condition when cached = 0, where both values are equal and the row is
    // skipped. Idempotent: after the repair the signature no longer matches.
    sqlx::query(
        r#"UPDATE threads t
              SET input_tokens = s.miss_input::int
             FROM (
                 SELECT m.thread_id,
                        COALESCE(SUM(GREATEST(COALESCE((m.token_usage->>'prompt_tokens')::bigint, 0), 0)), 0) AS total_input,
                        COALESCE(SUM(GREATEST(COALESCE((m.token_usage->>'prompt_tokens')::bigint, 0)
                                            - COALESCE((m.token_usage->>'cached_tokens')::bigint, 0), 0)), 0) AS miss_input
                   FROM messages m
                  WHERE m.msg_type <> 'error'
                  GROUP BY m.thread_id
             ) s
            WHERE t.id = s.thread_id
              AND t.input_tokens::bigint = s.total_input
              AND t.input_tokens::bigint <> s.miss_input"#,
    )
    .execute(pool)
    .await
    .ok();
    tracing::info!(
        "[migration] Repaired threads.input_tokens written as the TOTAL prompt (cache-miss semantics)"
    );
    // -- Removed: per-task goal state (goal_phase et al.) --------------------
    // Dispatch is status-gated (only status = 'todo' AND archived = false is
    // ever dispatched) and the status-change -> thread lifecycle never read
    // goal state, so the per-task goal columns were redundant (operator
    // decision, 2026-09-17: status alone drives thread lifecycle and
    // dispatch). Idempotent DROPs clean up databases created while the goal
    // state machine existed (safe to run on every startup).
    sqlx::query("ALTER TABLE kanban_tasks DROP CONSTRAINT IF EXISTS chk_kanban_tasks_goal_phase")
        .execute(pool)
        .await
        .ok();
    for stmt in [
        "ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS goal_phase",
        "ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS goal_blocked_code",
        "ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS goal_blocked_message",
        "ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS goal_max_rounds",
        "ALTER TABLE kanban_tasks DROP COLUMN IF EXISTS goal_revision",
    ] {
        sqlx::query(stmt).execute(pool).await.ok();
    }
    tracing::info!(
        "[migration] Dropped kanban_tasks goal state columns + chk_kanban_tasks_goal_phase CHECK (dispatch is status-gated)"
    );

    // -- Data migration: unify schedule/cron identity on a single name ------
    // The weekly disk-cleanup cron is keyed by its human name
    // 'cron-disk-cleanup'; rows still referencing the old generated keys are
    // re-pointed so the schedule history and cadence follow the rename
    // (idempotent: no-op once renamed, safe on every start).
    sqlx::query(
        "UPDATE threads SET schedule_task_id = 'cron-disk-cleanup' \
         WHERE schedule_task_id IN ('cron_18d0c264ade52f4c', 'cron_18d0c52afa06be59')",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "UPDATE task_runs SET task_key = 'cron-disk-cleanup' \
         WHERE task_key IN ('cron_18d0c264ade52f4c', 'cron_18d0c52afa06be59')",
    )
    .execute(pool)
    .await
    .ok();
    tracing::info!(
        "[migration] Data: disk-cleanup cron re-keyed to 'cron-disk-cleanup' (single-name identity)"
    );

    Ok(())
}

// ── Extensions ──────────────────────────────────────────────────────────────

/// Retention regression guard: `summaries.next_thread_id` is a monotonic
/// next-thread-id counter, NOT a reference that may be lost. It must NEVER get
/// an FK to `threads`: a thread hard-delete would then either fail or cascade
/// the counter away. Fail loudly (refuse to migrate) if one ever appears.
async fn assert_retention_regression_guards(pool: &PgPool) -> Result<()> {
    let fk_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
        FROM information_schema.table_constraints tc
        JOIN information_schema.key_column_usage kcu
          ON kcu.constraint_name = tc.constraint_name
         AND kcu.table_schema = tc.table_schema
        WHERE tc.constraint_type = 'FOREIGN KEY'
          AND tc.table_name = 'summaries'
          AND kcu.column_name = 'next_thread_id'
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if fk_count > 0 {
        anyhow::bail!(
            "retention guard: summaries.next_thread_id must NOT have a foreign key (it is a monotonic counter that must survive thread hard-deletes)"
        );
    }
    Ok(())
}

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
            iterations        INT NOT NULL DEFAULT 0,
            toolset           TEXT
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
            iteration_number  INT NOT NULL DEFAULT 0,
            original_thread_id BIGINT
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
            toolset         TEXT,
            created_at      TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            updated_at      TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Toolset column (additive, v0.2.3) ─────────────────────────────────
    // NULL = the level defines no toolset (previous behavior: all tools
    // allowed); a value is a toolset id from config/toolsets.yml restricting
    // the thread / task to that toolset's tools. Nullable + defaulted, so no
    // data migration is needed.
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS toolset TEXT;")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS toolset TEXT;")
        .execute(pool)
        .await
        .ok();

    // ── Thread usage aggregate columns (v0.4.2) ──────────────────────────
    // full_* and cost: sums over the thread's usage array items (tool-call
    // `_meta.usage` + the omniagent's own LLM-call entries), min-clamped
    // against the omniagent bare totals; populated at thread end exactly like
    // input_tokens / cached_tokens / output_tokens are today.
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS full_input_tokens INT DEFAULT 0;")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS full_cached_tokens INT DEFAULT 0;")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS full_output_tokens INT DEFAULT 0;")
        .execute(pool)
        .await
        .ok();
    sqlx::query(
        "ALTER TABLE threads ADD COLUMN IF NOT EXISTS full_reasoning_tokens INT DEFAULT 0;",
    )
    .execute(pool)
    .await
    .ok();
    sqlx::query("ALTER TABLE threads ADD COLUMN IF NOT EXISTS cost DOUBLE PRECISION DEFAULT 0;")
        .execute(pool)
        .await
        .ok();
    // `full_cost` (operator UPDATE 2026-10-02, telegram threads 3916/3917):
    // `threads.cost` holds the OMNIAGENT-only cost and `full_cost` the FULL
    // cost (omniagent + external agents/sub-agents). Additive + defaulted, so no
    // data migration is needed; new threads carry the split.
    sqlx::query(
        "ALTER TABLE threads ADD COLUMN IF NOT EXISTS full_cost DOUBLE PRECISION DEFAULT 0;",
    )
    .execute(pool)
    .await
    .ok();

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

    // Messages: newest message per thread (threads list "last_message") and
    // per-thread message counts. Without it both run a bitmap/index scan plus
    // a sort over EVERY message of the thread on every list row.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_thread_id_desc
            ON messages(thread_id, id DESC);
        "#,
    )
    .execute(pool)
    .await?;

    // Messages filter dropdowns (GET /messages/filters): the handler runs a
    // SELECT DISTINCT per column over the WHOLE messages table. Without an
    // index each one is a full heap scan, which dominates that endpoint on a
    // large table (3.6 GB in production).
    for stmt in [
        r#"CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role);"#,
        r#"CREATE INDEX IF NOT EXISTS idx_messages_msg_type ON messages(msg_type);"#,
        r#"CREATE INDEX IF NOT EXISTS idx_messages_msg_subtype ON messages(msg_subtype);"#,
    ] {
        sqlx::query(stmt).execute(pool).await?;
    }

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

// -- Message keyword search: tsvector FTS column + GIN index ---------------
// search_messages is served from a dedicated inverted index (tsvector GIN on
// messages.search_tsv) so keyword lookups are index-driven regardless of
// planner statistics (no ANALYZE dependency) and never scan or detoast the
// TOAST-heavy content column. messages_searchable_content() keeps the
// searchable text lean: messages longer than the cap contribute only their
// head and tail (both ends stay findable), so giant tool-output blobs cannot
// bloat the index or the per-row tsvector.
//
// MAINTENANCE MODEL (changed by the v0.1.9 startup-hang fix, incident
// 2026-09-05): v0.1.9 shipped search_tsv as `GENERATED ALWAYS AS (...) STORED`.
// Creating a STORED generated column (or converting an existing column to
// one) makes PostgreSQL compute the value for EVERY existing row at DDL time,
// i.e. a full-table rewrite under an AccessExclusive lock. On a large
// messages table (73k rows, multi-GB with TOAST) that single ALTER ran for
// 8+ minutes at container start, BEFORE the HTTP API bound, so clients saw
// 500/502 and the operator had to roll back. This version converges every
// schema state onto ONE representation that never needs a full-table rewrite:
//
//   - search_tsv is a PLAIN nullable tsvector column. DROP COLUMN only marks
//     the attribute dropped and a plain nullable ADD COLUMN is metadata-only
//     in PG11+; neither rewrites the table.
//   - a BEFORE INSERT OR UPDATE OF content trigger (trg_messages_search_tsv)
//     keeps the column populated with the SAME expression the v0.1.9
//     generated column used (capped content + identifier words, english
//     parse): ranking, identifier-token matching and the content cap are
//     unchanged.
//   - existing rows are backfilled in bounded chunks (one statement per
//     ~1000 ids, row-level locks only), so reads such as count(*) are never
//     blocked behind the migration and no single statement stalls for
//     minutes.
//   - the GIN index is (re)built after the backfill (SHARE lock only).
// The append-only trigger (trg_messages_append_only) is dropped for the
// backfill window and recreated by create_triggers() later in the same
// migration run; the HTTP API is not bound yet at that point, so there are no
// concurrent writers in the window.
//
// Fresh installs and databases already migrated by this version are no-ops
// once the sync trigger exists (checked below), so routine restarts are not
// slowed down.
async fn create_search_support(pool: &PgPool) -> Result<()> {
    // Helper functions (immutable; behavior identical to v0.1.9).
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION messages_searchable_content(content text)
        RETURNS text
        LANGUAGE sql IMMUTABLE PARALLEL SAFE
        AS $$
            SELECT CASE
                WHEN content IS NULL THEN ''
                WHEN char_length(content) <= 20000 THEN content
                ELSE left(content, 10000) || E'\n[...]\n' || right(content, 10000)
            END
        $$;
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION messages_identifier_words(content text)
        RETURNS text
        LANGUAGE sql IMMUTABLE PARALLEL SAFE
        AS $$
            SELECT string_agg(replace(m[1], '_', ''), ' ')
            FROM regexp_matches(content, '([A-Za-z0-9]+(?:_[A-Za-z0-9]+)+)', 'g') AS m
        $$;
        "#,
    )
    .execute(pool)
    .await?;

    // Column state: does search_tsv exist, and is it a STORED generated column?
    let col_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM pg_attribute
            WHERE attrelid = 'messages'::regclass
              AND attname = 'search_tsv'
              AND NOT attisdropped
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    let mut is_generated = false;
    if col_exists {
        is_generated = sqlx::query_scalar(
            r#"
            SELECT attgenerated <> ''
            FROM pg_attribute
            WHERE attrelid = 'messages'::regclass
              AND attname = 'search_tsv'
              AND NOT attisdropped
            "#,
        )
        .fetch_one(pool)
        .await?;
    }

    // The sync trigger only exists on databases this version already migrated:
    // its presence means the plain column is trigger-maintained and the
    // backfill already ran (so routine restarts skip it).
    let sync_trigger_exists: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM pg_trigger
            WHERE tgrelid = 'messages'::regclass
              AND tgname = 'trg_messages_search_tsv'
        )
        "#,
    )
    .fetch_one(pool)
    .await?;

    let needs_backfill = if !col_exists {
        // Plain nullable ADD COLUMN: metadata only, no table rewrite. Covers
        // fresh installs and pre-search_tsv databases (existing rows read
        // NULL until the backfill below fills them).
        sqlx::query("ALTER TABLE messages ADD COLUMN search_tsv tsvector")
            .execute(pool)
            .await?;
        tracing::info!("[migration] messages.search_tsv: added plain tsvector column");
        true
    } else if is_generated {
        // A STORED generated column cannot be altered in place; DROP COLUMN
        // is metadata-only (no rewrite) and so is the plain ADD COLUMN that
        // follows. Existing rows are recomputed by the chunked backfill with
        // the same expression, so the indexed content is preserved.
        sqlx::query("ALTER TABLE messages DROP COLUMN search_tsv")
            .execute(pool)
            .await?;
        sqlx::query("ALTER TABLE messages ADD COLUMN search_tsv tsvector")
            .execute(pool)
            .await?;
        tracing::info!(
            "[migration] messages.search_tsv: converted generated column to plain (no table rewrite)"
        );
        true
    } else if !sync_trigger_exists {
        // Legacy plain column (pre-v0.1.9 install): values are stale (plain
        // english parse) or NULL. First migration onto the trigger-maintained
        // schema: recompute every row.
        tracing::info!(
            "[migration] messages.search_tsv: legacy plain column found, full backfill required"
        );
        true
    } else {
        tracing::info!(
            "[migration] messages.search_tsv: already plain + trigger-maintained (no backfill needed)"
        );
        false
    };

    if needs_backfill {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM messages")
            .fetch_one(pool)
            .await?;
        if total > 0 {
            // The append-only trigger rejects UPDATEs that touch search_tsv,
            // so drop it for the backfill window; create_triggers() recreates
            // it later in this same run (no writers yet: API not bound).
            sqlx::query("DROP TRIGGER IF EXISTS trg_messages_append_only ON messages")
                .execute(pool)
                .await?;
            let min_id: i64 = sqlx::query_scalar("SELECT MIN(id) FROM messages")
                .fetch_one(pool)
                .await?;
            let max_id: i64 = sqlx::query_scalar("SELECT MAX(id) FROM messages")
                .fetch_one(pool)
                .await?;
            let started = std::time::Instant::now();
            let mut done: i64 = 0;
            let mut lo = min_id;
            let step = 1000_i64;
            // The indexed expression (MUST stay identical to the trigger
            // function below and to the v0.1.9 generated-column expression):
            // english parse of the capped content plus underscore identifiers
            // appended as single joined tokens.
            while lo <= max_id {
                let hi = lo.saturating_add(step - 1).min(max_id);
                let res = sqlx::query(
                    r#"
                    UPDATE messages
                    SET search_tsv = to_tsvector('english'::regconfig,
                        messages_searchable_content(content) || ' ' ||
                        COALESCE(messages_identifier_words(messages_searchable_content(content)), ''))
                    WHERE id >= $1 AND id <= $2
                    "#,
                )
                .bind(lo)
                .bind(hi)
                .execute(pool)
                .await?;
                let n = res.rows_affected() as i64;
                done += n;
                tracing::info!(
                    "[migration] search_tsv backfill rows {}-{}: {} rows ({} / {})",
                    lo,
                    hi,
                    n,
                    done,
                    total
                );
                if hi == max_id {
                    break;
                }
                lo = hi.saturating_add(1);
            }
            tracing::info!(
                "[migration] search_tsv backfill complete: {} rows in {:?}",
                done,
                started.elapsed()
            );
        } else {
            tracing::info!("[migration] search_tsv backfill skipped: messages is empty");
        }
    }

    // Sync trigger: keeps search_tsv populated for new and edited rows. The
    // column-list trigger (UPDATE OF content) means the backfill UPDATEs above
    // (which set only search_tsv) never fire it. Its expression is the same as
    // the backfill expression above (content is referenced as NEW.content).
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION messages_search_tsv_sync()
        RETURNS trigger
        LANGUAGE plpgsql
        AS $$
        BEGIN
            NEW.search_tsv := to_tsvector('english'::regconfig,
                messages_searchable_content(NEW.content) || ' ' ||
                COALESCE(messages_identifier_words(messages_searchable_content(NEW.content)), ''));
            RETURN NEW;
        END;
        $$;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TRIGGER IF EXISTS trg_messages_search_tsv ON messages")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER trg_messages_search_tsv \
         BEFORE INSERT OR UPDATE OF content ON messages \
         FOR EACH ROW EXECUTE FUNCTION messages_search_tsv_sync()",
    )
    .execute(pool)
    .await?;

    // GIN index over the final column contents: built once after the backfill
    // (SHARE lock only; reads such as count(*) stay unblocked).
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_search_tsv
        ON messages USING gin (search_tsv);
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!(
        "[migration] messages.search_tsv (plain, trigger-maintained) + GIN index ready for FTS keyword search"
    );
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

            -- Scoped exception (thread-end usage message, 2026-10-01): the
            -- "Usage"-type message inserted at thread end must land as the
            -- thread's 2nd-last message, so the current last message's
            -- thread_sequence is shifted by one and the Usage message takes
            -- the freed slot. The shift is allowed ONLY while the session
            -- variable omniagent.allow_message_seq_shift is 'on' (set inside
            -- the usage-message transaction; see insert_thread_usage_message
            -- in response_handler.rs) AND the ONLY change is
            -- thread_sequence = OLD.thread_sequence + 1. Everything else
            -- keeps the append-only guard intact.
            IF current_setting('omniagent.allow_message_seq_shift', true) = 'on' THEN
                IF NEW.id = OLD.id
                   AND NEW.thread_sequence = OLD.thread_sequence + 1
                   AND NEW.role IS NOT DISTINCT FROM OLD.role
                   AND NEW.content IS NOT DISTINCT FROM OLD.content
                   AND NEW.thread_id IS NOT DISTINCT FROM OLD.thread_id
                   AND NEW.external_id IS NOT DISTINCT FROM OLD.external_id
                   AND NEW.metadata IS NOT DISTINCT FROM OLD.metadata
                   AND NEW.embedding_vec IS NOT DISTINCT FROM OLD.embedding_vec
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

/// Backfill (2026-10-01): insert the thread-end "Usage"-type message for
/// every thread that terminated Completed/Interrupted/Failed before the
/// runtime insertion existed (or while it silently failed), so the invariant
/// "every terminated non-skipped thread has a msg_type='usage' message as its
/// 2nd-last message" holds across the whole DB.
///
/// The per-call usage array was never persisted for those threads, so the
/// content is synthesized from the threads-table bare totals (the omniagent's
/// own LLM usage, the only data recoverable) as a single entry; threads with
/// zero recorded usage get an empty array. Idempotent: threads that already
/// carry a msg_type='usage' message, and Skipped/Merged threads, are
/// untouched. Uses the same scoped trigger exception as the runtime insertion
/// (`omniagent.allow_message_seq_shift`, see `prevent_message_mutation`).
async fn backfill_thread_end_usage_messages(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        DO $$
        DECLARE
            t RECORD;
            max_seq INT;
            content TEXT;
        BEGIN
            PERFORM set_config('omniagent.allow_message_seq_shift', 'on', true);
            FOR t IN
                SELECT th.id, th.profile, th.input_tokens, th.cached_tokens, th.output_tokens
                FROM threads th
                WHERE th.status IN ('completed', 'interrupted', 'failed')
                  AND NOT EXISTS (
                      SELECT 1 FROM messages m
                      WHERE m.thread_id = th.id AND m.msg_type = 'usage'
                  )
            LOOP
                SELECT COALESCE(MAX(thread_sequence), 0) INTO max_seq
                FROM messages WHERE thread_id = t.id;
                IF max_seq = 0 THEN
                    CONTINUE;
                END IF;
                UPDATE messages SET thread_sequence = thread_sequence + 1
                WHERE thread_id = t.id AND thread_sequence = max_seq;
                IF COALESCE(t.input_tokens, 0) > 0 OR COALESCE(t.output_tokens, 0) > 0
                   OR COALESCE(t.cached_tokens, 0) > 0 THEN
                    content := jsonb_build_array(jsonb_build_object(
                        'omniagent', true,
                        'agent', COALESCE(t.profile, 'omni'),
                        'input_tokens', COALESCE(t.input_tokens, 0),
                        'output_tokens', COALESCE(t.output_tokens, 0),
                        'total_tokens', COALESCE(t.input_tokens, 0) + COALESCE(t.output_tokens, 0),
                        'cached_input_tokens', COALESCE(t.cached_tokens, 0),
                        'cache_write_tokens', NULL,
                        'reasoning_tokens', 0,
                        'cost', NULL,
                        'provider', NULL,
                        'model', NULL
                    ))::text;
                ELSE
                    content := '[]';
                END IF;
                INSERT INTO messages (
                    thread_id, role, content, thread_sequence, external_id,
                    metadata, summary_text, is_summary,
                    msg_type, msg_subtype, iteration_number, duration_ms,
                    token_usage, channel_id
                )
                VALUES (
                    t.id, 'agent', content, max_seq, NULL,
                    '{"is_usage": true}'::jsonb, NULL, false,
                    'usage', NULL, 0, 0, '{}'::jsonb,
                    (SELECT channel_id FROM threads WHERE id = t.id)
                );
            END LOOP;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;
    tracing::info!(
        "[migration] Backfilled thread-end Usage messages for terminated non-skipped threads"
    );
    Ok(())
}

/// Backfill (2026-10-01, rework prompted by the tester verdict on thread 3861):
/// populate the threads-table `full_*` / `cost` aggregate columns for terminal
/// threads that still carry the pre-aggregate fallback (0).
///
/// Two classes of rows are covered: legacy threads whose Usage message was
/// inserted by the backfill above, and threads finalized BEFORE the thread-end
/// usage entries existed - the fail-thread tool finalized the row mid-loop, so
/// the loop-exit aggregate write became a no-op against `complete_thread`'s
/// `AND NOT t.terminal` guard (runtime fix: `update_thread_usage_aggregates`).
///
/// The sums come from the usage-message content (the usage ARRAY itself), since
/// the backfilled messages carry no `token_usage` aggregates, and are
/// min-clamped against the thread's bare totals exactly like the runtime
/// `usage_entries::aggregate_fields` clamp.
///
/// SEMANTIC REWRITE (2026-10-02, operator threads 3915/3916/3917): the same
/// backfill rewrites pre-change terminal rows onto the NEW column semantics -
/// `input_tokens` = cache-MISS input only (was cache hit + miss), `cost` = the
/// omniagent-only share (was the combined cost) and the new `full_cost` =
/// omniagent + external sub-agents. A row counts as pre-change while `full_cost = 0` (the
/// column did not exist before and the write path always fills it). Migrations
/// are declarative and run at every startup, so this is idempotent: once
/// `full_cost > 0` the row is left untouched, and Skipped/Merged threads are
/// never touched.
async fn backfill_terminal_thread_usage_aggregates(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE threads t
        SET input_tokens = CASE WHEN COALESCE(t.full_cost, 0) = 0
                                THEN LEAST(COALESCE(t.input_tokens, 0)::bigint, s.sum_omni_input)::int
                                ELSE t.input_tokens END,
            full_input_tokens = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.full_input_tokens
                                     ELSE (s.sum_omni_input + s.sum_external_input)::int END,
            full_cached_tokens = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.full_cached_tokens
                                      ELSE s.sum_cached::int END,
            full_output_tokens = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.full_output_tokens
                                      ELSE s.sum_output::int END,
            full_reasoning_tokens = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.full_reasoning_tokens
                                         ELSE s.sum_reasoning::int END,
            cost = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.cost
                        WHEN s.sum_omni_cost > 0 THEN s.sum_omni_cost
                        ELSE t.cost END,
            full_cost = CASE WHEN COALESCE(t.full_cost, 0) > 0 THEN t.full_cost
                             WHEN s.sum_cost > 0 THEN s.sum_cost
                             ELSE t.cost END
        FROM (
            SELECT m.thread_id AS thread_id,
                   COALESCE(SUM(CASE WHEN COALESCE(e.item ->> 'omniagent', 'false') = 'true'
                                     THEN GREATEST(
                                         COALESCE(CASE WHEN jsonb_typeof(e.item -> 'input_tokens') = 'number'
                                                       THEN (e.item ->> 'input_tokens')::bigint ELSE 0 END, 0)
                                         - COALESCE(CASE WHEN jsonb_typeof(e.item -> 'cached_input_tokens') = 'number'
                                                         THEN (e.item ->> 'cached_input_tokens')::bigint ELSE 0 END, 0), 0)
                                     ELSE 0 END), 0) AS sum_omni_input,
                   COALESCE(SUM(CASE WHEN COALESCE(e.item ->> 'omniagent', 'false') <> 'true'
                                     THEN COALESCE(CASE WHEN jsonb_typeof(e.item -> 'input_tokens') = 'number'
                                                       THEN (e.item ->> 'input_tokens')::bigint ELSE 0 END, 0)
                                     ELSE 0 END), 0) AS sum_external_input,
                   COALESCE(SUM(CASE WHEN jsonb_typeof(e.item -> 'cached_input_tokens') = 'number'
                                     THEN (e.item ->> 'cached_input_tokens')::bigint ELSE 0 END), 0) AS sum_cached,
                   COALESCE(SUM(CASE WHEN jsonb_typeof(e.item -> 'output_tokens') = 'number'
                                     THEN (e.item ->> 'output_tokens')::bigint ELSE 0 END), 0) AS sum_output,
                   COALESCE(SUM(CASE WHEN jsonb_typeof(e.item -> 'reasoning_tokens') = 'number'
                                     THEN (e.item ->> 'reasoning_tokens')::bigint ELSE 0 END), 0) AS sum_reasoning,
                   COALESCE(SUM(CASE WHEN COALESCE(e.item ->> 'omniagent', 'false') = 'true'
                                      AND jsonb_typeof(e.item -> 'cost') = 'object'
                                      AND jsonb_typeof((e.item -> 'cost') -> 'amount_usd') = 'number'
                                     THEN ((e.item -> 'cost') ->> 'amount_usd')::double precision
                                     ELSE 0 END), 0) AS sum_omni_cost,
                   COALESCE(SUM(CASE WHEN jsonb_typeof(e.item -> 'cost') = 'object'
                                      AND jsonb_typeof((e.item -> 'cost') -> 'amount_usd') = 'number'
                                     THEN ((e.item -> 'cost') ->> 'amount_usd')::double precision
                                     ELSE 0 END), 0) AS sum_cost
            FROM messages m
            CROSS JOIN LATERAL jsonb_array_elements(
                CASE WHEN m.content LIKE '[%' THEN m.content::jsonb ELSE '[]'::jsonb END
            ) AS e(item)
            WHERE m.msg_type = 'usage'
              AND COALESCE(e.item -> 'details' ->> 'kind', 'llm-call') = 'llm-call'
            GROUP BY m.thread_id
        ) s
        WHERE t.id = s.thread_id
          AND t.status IN ('completed', 'interrupted', 'failed')
          AND (COALESCE(t.full_cost, 0) = 0
               OR COALESCE(t.full_input_tokens, 0) = 0
               OR COALESCE(t.full_cached_tokens, 0) = 0
               OR COALESCE(t.full_output_tokens, 0) = 0)
        "#,
    )
    .execute(pool)
    .await?;
    tracing::info!("[migration] Backfilled threads-table usage aggregates for terminal threads");
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
    // schema-v5 step later in run()) - ensure it exists first, otherwise
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

#[cfg(test)]
mod tests {
    use super::{db_host, is_release_build_mode, KNOWN_DEV_DB_HOSTS};

    #[test]
    fn db_host_parses_common_urls() {
        assert_eq!(
            db_host("postgres://omniagent:pass@postgres:5432/omniagent"),
            Some("postgres")
        );
        assert_eq!(
            db_host("postgres://omniagent:pass@172.18.0.4:5432/omniagent"),
            Some("172.18.0.4")
        );
        assert_eq!(db_host("postgres://u@localhost/db"), Some("localhost"));
        assert_eq!(db_host("postgres://u:p@[::1]:5432/db"), Some("::1"));
        assert_eq!(db_host("postgres:///dbname"), None);
        assert_eq!(
            db_host("postgres://u@omnidev-postgres:5432/omniagent"),
            Some("omnidev-postgres")
        );
    }

    #[test]
    fn dev_host_allowlist_is_exact() {
        for h in [
            "localhost",
            "127.0.0.1",
            "::1",
            "omnidev-postgres",
            "omnidev-postgres-1",
        ] {
            assert!(
                KNOWN_DEV_DB_HOSTS.contains(&h),
                "{h} should be a known dev host"
            );
        }
        // The bare compose service name and prod-like hosts must NOT be dev:
        // the production omni-stack DATABASE_URL uses host `postgres`.
        for h in ["postgres", "172.18.0.4", "prod-db", "db.example.com", ""] {
            assert!(
                !KNOWN_DEV_DB_HOSTS.contains(&h),
                "{h} must not be a known dev host"
            );
        }
    }

    #[test]
    fn release_build_mode_is_exact_match() {
        assert!(is_release_build_mode(Some("release")));
        for v in [
            None,
            Some("dev"),
            Some("development"),
            Some("test"),
            Some("local"),
            Some("RELEASE"),
            Some("Release"),
            Some(""),
        ] {
            assert!(
                !is_release_build_mode(v),
                "{v:?} must NOT count as a release build (fail closed)"
            );
        }
    }
}
