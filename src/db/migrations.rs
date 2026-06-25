use anyhow::Result;
use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> Result<()> {
    phase_1_core_tables(pool).await?;
    phase_2_message_channel_migrations(pool).await?;
    phase_3_feature_tables(pool).await?;
    phase_4_indexes_and_columns(pool).await?;
    phase_5_planning_and_search(pool).await?;
    phase_6_vector_and_secrets(pool).await?;
    phase_7_seed_actions(pool).await?;
    phase_8_iteration_number(pool).await?;
    phase_9_cron_schedule_5_field(pool).await?;
    phase_10_fix_thread_causes(pool).await?;
    phase_11_rename_tool_result_msg_type(pool).await?;
    phase_12_migrate_user_role(pool).await?;
    phase_13_migrate_actions_to_yaml(pool).await?;
    Ok(())
}

/// Phase 1: Core tables — extensions, channels, messages, channel_stops.
async fn phase_1_core_tables(pool: &PgPool) -> Result<()> {
    // Enable pgvector extension — wrapped in DO block so it doesn't fail
    // if pgvector isn't installed (optional vector support).
    sqlx::query(
            r#"
            DO $$ BEGIN
                CREATE EXTENSION IF NOT EXISTS vector;
            EXCEPTION
                WHEN OTHERS THEN
                    -- vector extension not available, continue without it
            END $$;
            "#
        )
        .execute(pool)
        .await?;

    // Create channels table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channels (
            id          BIGSERIAL PRIMARY KEY,
            name        TEXT NOT NULL,
            platform    TEXT NOT NULL,
            external_id TEXT NOT NULL,
            cause       TEXT NOT NULL,
            metadata    JSONB DEFAULT '{}',
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(platform, external_id)
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Create messages table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id              BIGSERIAL PRIMARY KEY,
            channel_id      BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            role            TEXT NOT NULL,
            content         TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'pending',
            thread_id       BIGINT NOT NULL,
            thread_sequence INT NOT NULL,
            external_id     TEXT,
            metadata        JSONB DEFAULT '{}',
            embedding       TEXT,
            summary_text    TEXT,
            is_summary      BOOL NOT NULL DEFAULT false,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(channel_id, external_id),
            UNIQUE(thread_id, thread_sequence)
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Create channel_stops table for tracking stopped/paused channels
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channel_stops (
            id          BIGSERIAL PRIMARY KEY,
            channel_id  BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            stopped_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(channel_id)
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 2: Column migrations on messages and channels tables.
async fn phase_2_message_channel_migrations(pool: &PgPool) -> Result<()> {
    // Migration: add msg_type, msg_subtype, iteration_count columns
    // (idempotent — skips if columns already exist)
    sqlx::query(
        r#"
        ALTER TABLE messages
            ADD COLUMN IF NOT EXISTS msg_type TEXT NOT NULL DEFAULT 'message',
            ADD COLUMN IF NOT EXISTS msg_subtype TEXT,
            ADD COLUMN IF NOT EXISTS iteration_count INT NOT NULL DEFAULT 0;
        "#,
    )
    .execute(pool)
    .await?;

    // Drop the unique constraint on (thread_id, thread_sequence) since
    // a single LLM turn can produce multiple records (reasoning, message).
    sqlx::query(
        r#"
        DO $$ BEGIN
            ALTER TABLE messages DROP CONSTRAINT messages_thread_id_thread_sequence_key;
        EXCEPTION
            WHEN undefined_object THEN NULL;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_thread_seq
            ON messages(thread_id, thread_sequence);
        "#,
    )
    .execute(pool)
    .await?;

    // Profile and model columns for channels
    sqlx::query(
        r#"
        ALTER TABLE channels
            ADD COLUMN IF NOT EXISTS current_profile TEXT NOT NULL DEFAULT 'default',
            ADD COLUMN IF NOT EXISTS current_model TEXT,
            ADD COLUMN IF NOT EXISTS current_provider TEXT;
        "#,
    )
    .execute(pool)
    .await?;

    // Profile, model, provider, processing_time for messages
    sqlx::query(
        r#"
        ALTER TABLE messages
            ADD COLUMN IF NOT EXISTS profile TEXT NOT NULL DEFAULT 'default',
            ADD COLUMN IF NOT EXISTS provider TEXT,
            ADD COLUMN IF NOT EXISTS model TEXT,
            ADD COLUMN IF NOT EXISTS processing_time_ms INT,
            ADD COLUMN IF NOT EXISTS token_usage JSONB;
        "#,
    )
    .execute(pool)
    .await?;

    // Migration: make thread_id nullable so seq-0 messages can be inserted
    // without a pre-determined thread_id, then set thread_id = id after insert.
    sqlx::query(
        r#"
        ALTER TABLE messages
        ALTER COLUMN thread_id DROP NOT NULL;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Drop profiles table — data now lives in profiles/<name>/config.json ──
    sqlx::query(
        r#"
        DROP TABLE IF EXISTS profiles CASCADE;
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 3: Feature tables — kanban, cron, summaries, threads, subscriptions,
/// read-only user, and data migration for threads.
async fn phase_3_feature_tables(pool: &PgPool) -> Result<()> {
    // ── Kanban tasks table ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_tasks (
            id         TEXT PRIMARY KEY,
            title      TEXT NOT NULL,
            body       TEXT DEFAULT '',
            status     TEXT NOT NULL DEFAULT 'backlog',
            priority   INTEGER DEFAULT 0,
            assignee   TEXT DEFAULT '',
            created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
            updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Cron jobs table ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cron_jobs (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            display_name TEXT NOT NULL DEFAULT '',
            schedule    TEXT NOT NULL,
            prompt      TEXT NOT NULL DEFAULT '',
            skills      TEXT DEFAULT '[]',
            enabled     BOOLEAN DEFAULT true,
            last_run_at TIMESTAMP WITH TIME ZONE,
            next_run_at TIMESTAMP WITH TIME ZONE,
            created_at  TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Add display_name if it doesn't exist (for existing tables)
    sqlx::query(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT ''
        "#,
    )
    .execute(pool)
    .await?;

    // Add running flag for atomic concurrency guard
    sqlx::query(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS running BOOLEAN NOT NULL DEFAULT false
        "#,
    )
    .execute(pool)
    .await?;

    // Migration: add iterations column (per-LLM-call counter)
    sqlx::query(
        r#"
        ALTER TABLE messages
        ADD COLUMN IF NOT EXISTS iterations INT NOT NULL DEFAULT 0
        "#,
    )
    .execute(pool)
    .await?;

    // ── Read-only user for query_database tool ──
    sqlx::query(
        r#"
        DO $$
        BEGIN
            CREATE USER omniagent_readonly WITH PASSWORD 'omniagent_readonly';
        EXCEPTION
            WHEN duplicate_object THEN NULL;
        END $$;
        "#
    )
    .execute(pool)
    .await?;

    sqlx::query("GRANT CONNECT ON DATABASE omniagent TO omniagent_readonly")
        .execute(pool)
        .await?;

    sqlx::query("GRANT USAGE ON SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await?;

    sqlx::query("GRANT SELECT ON ALL TABLES IN SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await?;

    sqlx::query("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO omniagent_readonly")
        .execute(pool)
        .await?;

    // ── Add channel_id and profile to kanban_tasks ──
    sqlx::query(
        r#"
        ALTER TABLE kanban_tasks
        ADD COLUMN IF NOT EXISTS channel_id BIGINT REFERENCES channels(id),
        ADD COLUMN IF NOT EXISTS profile TEXT
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add channel_id and profile to cron_jobs ──
    sqlx::query(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS channel_id BIGINT REFERENCES channels(id),
        ADD COLUMN IF NOT EXISTS profile TEXT
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add readonly column to channels ──
    sqlx::query(
        r#"
        ALTER TABLE channels
        ADD COLUMN IF NOT EXISTS readonly BOOLEAN NOT NULL DEFAULT false
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add updated_at to cron_jobs for stale-lock detection ──
    sqlx::query(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        "#,
    )
    .execute(pool)
    .await?;

    // ── Summaries table for cross-thread thread summaries ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS summaries (
            id              BIGSERIAL PRIMARY KEY,
            channel_id      BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            next_thread_id  BIGINT NOT NULL,
            content         TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Threads table migration ──
    // Creates the threads table and migrates data from the old flat messages table.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS threads (
            id              BIGSERIAL PRIMARY KEY,
            status          TEXT NOT NULL DEFAULT 'created',
            cause           TEXT NOT NULL,
            channel_id      BIGINT NOT NULL REFERENCES channels(id),
            profile         TEXT NOT NULL DEFAULT 'default',
            provider        TEXT,
            model           TEXT,
            input_tokens    INT DEFAULT 0,
            cached_tokens   INT DEFAULT 0,
            output_tokens   INT DEFAULT 0,
            duration_ms     INT DEFAULT 0,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            started_at      TIMESTAMPTZ,
            ended_at        TIMESTAMPTZ
        );
        "#
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_threads_channel_status ON threads(channel_id, status);
        "#
    )
    .execute(pool)
    .await?;

    // Data migration: create threads for every distinct thread_id in messages
    // Uses runtime sqlx::query to avoid compile-time validation errors when
    // old columns have already been dropped by a previous migration run.
    // This is safe because the migration is idempotent via ON CONFLICT DO NOTHING.
    {
        let has_old_columns: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name='messages' AND column_name='channel_id')"
        )
        .fetch_one(pool)
        .await
        .unwrap_or(false);

        if has_old_columns {
            sqlx::query(
                r#"
                INSERT INTO threads (id, status, cause, channel_id, profile, provider, model, created_at)
                SELECT DISTINCT 
                    COALESCE(m.thread_id, m.id) as id,
                    CASE 
                        WHEN m.status = 'completed' THEN 'completed'
                        WHEN m.status = 'failed' THEN 'failed'
                        WHEN m.status = 'skipped' THEN 'skipped'
                        WHEN m.status = 'processing' THEN 'interrupted'
                        ELSE 'completed'
                    END as status,
                    CASE 
                        WHEN m.role = 'user' THEN 'user'
                        WHEN m.msg_type = 'cron' THEN 'cron'
                        WHEN m.msg_type = 'kanban' THEN 'kanban'
                        ELSE 'user'
                    END as cause,
                    m.channel_id,
                    COALESCE(m.profile, 'default') as profile,
                    m.provider,
                    m.model,
                    m.created_at
                FROM messages m
                WHERE (m.thread_sequence = 0 OR m.thread_id IS NULL)
                  AND NOT EXISTS (SELECT 1 FROM threads t WHERE t.id = COALESCE(m.thread_id, m.id))
                ON CONFLICT (id) DO NOTHING
                "#
            )
            .execute(pool)
            .await?;

            // Update messages where thread_id was NULL
            sqlx::query("UPDATE messages SET thread_id = id WHERE thread_id IS NULL")
                .execute(pool)
                .await?;

            // Make thread_id NOT NULL
            sqlx::query("ALTER TABLE messages ALTER COLUMN thread_id SET NOT NULL")
                .execute(pool)
                .await?;

            // Drop columns that moved to threads table
            sqlx::query(
                r#"
                ALTER TABLE messages 
                    DROP COLUMN IF EXISTS status,
                    DROP COLUMN IF EXISTS channel_id,
                    DROP COLUMN IF EXISTS profile,
                    DROP COLUMN IF EXISTS provider,
                    DROP COLUMN IF EXISTS model,
                    DROP COLUMN IF EXISTS processing_time_ms,
                    DROP COLUMN IF EXISTS token_usage,
                    DROP COLUMN IF EXISTS iterations,
                    DROP COLUMN IF EXISTS iteration_count
                "#
            )
            .execute(pool)
            .await?;

            // Drop old indexes
            sqlx::query("DROP INDEX IF EXISTS idx_messages_channel_status").execute(pool).await?;
            sqlx::query("DROP INDEX IF EXISTS idx_messages_profile").execute(pool).await?;
        }
    }

    // Add foreign key (safe: won't fail if constraint already exists)
    sqlx::query(
        r#"
        DO $$ BEGIN
            ALTER TABLE messages ADD CONSTRAINT fk_messages_thread FOREIGN KEY (thread_id) REFERENCES threads(id);
        EXCEPTION
            WHEN duplicate_object THEN NULL;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // Ensure messages.thread_id is NOT NULL (in case the migration block above was skipped)
    sqlx::query(
        r#"
        DO $$ BEGIN
            ALTER TABLE messages ALTER COLUMN thread_id SET NOT NULL;
        EXCEPTION
            WHEN others THEN NULL;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Channel subscriptions table for summary delivery across channels ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channel_subscriptions (
            id                      BIGSERIAL PRIMARY KEY,
            channel_id              BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            subscriber_platform     TEXT NOT NULL,
            subscriber_resource     TEXT NOT NULL,
            created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(channel_id, subscriber_platform, subscriber_resource)
        );
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 4: Indexes and additional columns — closed, resource_identifier,
/// terminal, kanban dependencies, plugin registry, actions.
async fn phase_4_indexes_and_columns(pool: &PgPool) -> Result<()> {
    // Add closed column to channels (default false — channels start opened)
    sqlx::query(
        r#"
        ALTER TABLE channels
        ADD COLUMN IF NOT EXISTS closed BOOLEAN NOT NULL DEFAULT false
        "#
    )
    .execute(pool)
    .await?;

    // Index on threads for channel_id + status queries
    sqlx::query(
        r#"
        DO $$ BEGIN
            CREATE INDEX IF NOT EXISTS idx_threads_channel_status
            ON threads(channel_id, status);
        EXCEPTION
            WHEN duplicate_table THEN NULL;
        END $$;
        "#
    )
    .execute(pool)
    .await?;

    // Add terminal flag to threads (prevents further state transitions)
    sqlx::query(
        r#"
        ALTER TABLE threads
        ADD COLUMN IF NOT EXISTS terminal BOOLEAN NOT NULL DEFAULT false
        "#
    )
    .execute(pool)
    .await?;

    // ── Make platform nullable in channels, add resource_identifier ──
    sqlx::query(
        r#"
        ALTER TABLE channels ALTER COLUMN platform DROP NOT NULL;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        ALTER TABLE channels ADD COLUMN IF NOT EXISTS resource_identifier TEXT;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        ALTER TABLE channels ADD COLUMN IF NOT EXISTS external_id TEXT;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add UNIQUE(platform, resource_identifier) if it doesn't exist ──
    sqlx::query(
        r#"
        DO $$ BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conname = 'channels_platform_resource_identifier_key'
            ) THEN
                ALTER TABLE channels
                ADD CONSTRAINT channels_platform_resource_identifier_key
                UNIQUE (platform, resource_identifier);
            END IF;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add task_id to threads for kanban task association ──
    sqlx::query(
        r#"
        ALTER TABLE threads
        ADD COLUMN IF NOT EXISTS task_id TEXT REFERENCES kanban_tasks(id)
        "#,
    )
    .execute(pool)
    .await?;

    // ── Kanban task dependencies table ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS kanban_task_dependencies (
            task_id TEXT NOT NULL REFERENCES kanban_tasks(id) ON DELETE CASCADE,
            depends_on_id TEXT NOT NULL REFERENCES kanban_tasks(id) ON DELETE CASCADE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (task_id, depends_on_id)
        )
        "#,
    )
    .execute(pool)
    .await?;

    // ── Cron job mode, direct_task_type, active columns ──
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS mode TEXT NOT NULL DEFAULT 'agentic'"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS direct_task_type TEXT DEFAULT NULL"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT true"#,
    )
    .execute(pool)
    .await?;

    // ── Add schedule_task_id to threads for cron job / schedule association ──
    sqlx::query(
        r#"
        ALTER TABLE threads
        ADD COLUMN IF NOT EXISTS schedule_task_id TEXT
        "#,
    )
    .execute(pool)
    .await?;

    // Index for fast lookups by schedule_task_id
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_threads_schedule_task_id
        ON threads(schedule_task_id)
        "#,
    )
    .execute(pool)
    .await?;

    // ── Plugin registry table ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS plugin_registry (
            id          SERIAL PRIMARY KEY,
            name        VARCHAR(255) NOT NULL UNIQUE,
            plugin_type VARCHAR(50)  NOT NULL,
            version     VARCHAR(50)  NOT NULL DEFAULT '0.1.0',
            source      TEXT,
            status      VARCHAR(20)  NOT NULL DEFAULT 'enabled',
            manifest    JSONB        NOT NULL DEFAULT '{}',
            config      JSONB        NOT NULL DEFAULT '{}',
            created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
            updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Unique index on (name) for upsert operations
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_registry_name ON plugin_registry(name);
        "#,
    )
    .execute(pool)
    .await?;

    // Migrate id column from SERIAL (INT4) to BIGSERIAL (INT8) to match Rust i64
    sqlx::query(
        r#"
        DO $$ BEGIN
            ALTER TABLE plugin_registry ALTER COLUMN id TYPE BIGINT;
        EXCEPTION
            WHEN OTHERS THEN
                -- Column already BIGINT or migration not needed
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Thread subtasks table ──
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

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_thread_subtasks_thread_id
        ON thread_subtasks(thread_id);
        "#,
    )
    .execute(pool)
    .await?;

    // ── Actions table ──
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS actions (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            tool_name   TEXT NOT NULL,
            params      JSONB NOT NULL DEFAULT '{}',
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Actions ID sequence ──
    sqlx::query(
        r#"CREATE SEQUENCE IF NOT EXISTS actions_id_seq START 1;"#,
    )
    .execute(pool)
    .await?;

    // ── Add is_builtin column to actions table ──
    sqlx::query(
        r#"ALTER TABLE actions ADD COLUMN IF NOT EXISTS is_builtin BOOLEAN NOT NULL DEFAULT false;"#,
    )
    .execute(pool)
    .await?;

    // ── Add action_id column to cron_jobs table ──
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS action_id TEXT REFERENCES actions(id);"#,
    )
    .execute(pool)
    .await?;

    // ── Seed built-in actions ──
    sqlx::query(
        r#"
        INSERT INTO actions (id, name, tool_name, params, is_builtin)
        VALUES ('builtin_kanban_dispatcher', 'Kanban Dispatcher', 'actions_kanban_dispatcher', '{}', true)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO actions (id, name, tool_name, params, is_builtin)
        VALUES ('builtin_relevance_indexer', 'Relevance Indexer', 'actions_relevance_indexer', '{}', true)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO actions (id, name, tool_name, params, is_builtin)
        VALUES ('builtin_hindsight_populator', 'Hindsight Populator', 'actions_hindsight_populator', '{}', true)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO actions (id, name, tool_name, params, is_builtin)
        VALUES ('builtin_setup_knowledge_pipeline', 'Setup Knowledge Pipeline', 'actions_setup_knowledge_pipeline', '{}', true)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add silent column to cron_jobs table ──
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS silent BOOLEAN NOT NULL DEFAULT false;"#,
    )
    .execute(pool)
    .await?;

    // ── Add archived column to kanban_tasks ──
    sqlx::query(
        r#"ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS archived BOOLEAN NOT NULL DEFAULT false;"#,
    )
    .execute(pool)
    .await?;

    // ── Add position column to kanban_tasks ──
    sqlx::query(
        r#"ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS position INTEGER;"#,
    )
    .execute(pool)
    .await?;

    // ── Add template column to kanban_tasks ──
    sqlx::query(
        r#"ALTER TABLE kanban_tasks ADD COLUMN IF NOT EXISTS template TEXT DEFAULT '';"#,
    )
    .execute(pool)
    .await?;

    // ── Add template column to cron_jobs ──
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS template TEXT DEFAULT '';"#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 5: Planning mode columns and GIN trigram search index.
async fn phase_5_planning_and_search(pool: &PgPool) -> Result<()> {
    // ── Add planning_mode columns ──
    // Threads: single source of truth for planning mode at runtime
    sqlx::query(
        r#"ALTER TABLE threads ADD COLUMN IF NOT EXISTS planning_mode TEXT NOT NULL DEFAULT '';"#,
    )
    .execute(pool)
    .await?;

    // Channels: override for all threads in the channel
    sqlx::query(
        r#"ALTER TABLE channels ADD COLUMN IF NOT EXISTS planning_mode TEXT NOT NULL DEFAULT '';"#,
    )
    .execute(pool)
    .await?;

    // Cron jobs: task-level override for cron threads
    sqlx::query(
        r#"ALTER TABLE cron_jobs ADD COLUMN IF NOT EXISTS planning_mode TEXT NOT NULL DEFAULT '';"#,
    )
    .execute(pool)
    .await?;

    // ── GIN trigram index for ILIKE search performance ──
    // Enables pg_trgm extension (idempotent) and creates a GIN index on
    // messages.content for fast ILIKE / LIKE / similarity queries.
    sqlx::query(
        r#"CREATE EXTENSION IF NOT EXISTS pg_trgm;"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_content_trgm
        ON messages USING gin (content gin_trgm_ops)
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 6: Native vector column with HNSW index and secrets tables.
async fn phase_6_vector_and_secrets(pool: &PgPool) -> Result<()> {
    // ── Phase 1: native vector column + HNSW index + two-stage decay ──
    // Adds a native vector(1536) column, backfills existing TEXT embeddings,
    // creates an HNSW index for fast ANN search, then enables two-stage
    // recency-weighted retrieval. The old TEXT `embedding` column is kept as
    // a fallback during Phase 1; it will be dropped when Phase 2 completes.
    //
    // All operations are idempotent — safe to run on every startup.
    let vector_available: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')"#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);

    if vector_available {
        // 1. Add native vector(1536) column
        sqlx::query(
            r#"
            ALTER TABLE messages
            ADD COLUMN IF NOT EXISTS embedding_vec vector(1536)
            "#,
        )
        .execute(pool)
        .await?;

        // 2. Backfill existing TEXT embeddings into the vector column
        //    Runs only if there are rows with embedding IS NOT NULL AND embedding_vec IS NULL.
        //    The cast from TEXT to vector(1536) uses the `[0.1,0.2,...]` array literal format.
        let backfill_result = sqlx::query(
            r#"
            UPDATE messages
            SET embedding_vec = embedding::vector(1536)
            WHERE embedding IS NOT NULL
              AND embedding != ''
              AND embedding_vec IS NULL
            "#,
        )
        .execute(pool)
        .await?;
        let backfilled = backfill_result.rows_affected();
        if backfilled > 0 {
            tracing::info!("Backfilled {} embeddings into embedding_vec column", backfilled);
        }

        // 3. Create HNSW index on the vector column for fast ANN search.
        //    IF NOT EXISTS is supported for non-concurrent CREATE INDEX in PG 14+.
        //    The index uses cosine distance (<=>) which matches our query operator.
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_messages_embedding_vec_hnsw
            ON messages USING hnsw (embedding_vec vector_cosine_ops)
            "#,
        )
        .execute(pool)
        .await?;

        // 4. Also create a B-tree index on created_at for the recency re-ranking.
        //    This helps the final ORDER BY in the two-stage approach when the
        //    candidate pool is large (up to 100 rows), though it's mainly useful
        //    for the outer re-ranking step.
        //    For 100 rows the sort is in-memory, so this is mostly for completeness.
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_messages_created_at
            ON messages(created_at DESC)
            "#,
        )
        .execute(pool)
        .await?;

        tracing::info!("pgvector HNSW index and embedding_vec column ready");
    } else {
        tracing::warn!(
            "pgvector extension not available — skipping HNSW index and vector column. \
             Text-cast fallback will be used for semantic search."
        );
    }

    // ── Secrets for user-managed key/value store with versioning ──
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

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_secret_versions_secret_id
        ON secret_versions(secret_id);
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 7: Seed additional built-in actions (idempotent — runs every startup).
async fn phase_7_seed_actions(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO actions (id, name, tool_name, params, is_builtin)
        VALUES ('builtin_setup_knowledge_pipeline', 'Setup Knowledge Pipeline', 'actions_setup_knowledge_pipeline', '{}', true)
        ON CONFLICT (id) DO NOTHING;
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 8: Add iteration_number column to messages table for tracking
/// per-LLM-call iteration within the tool-calling loop.
async fn phase_8_iteration_number(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"ALTER TABLE messages ADD COLUMN IF NOT EXISTS iteration_number INT NOT NULL DEFAULT 0;"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Phase 9: Migrate cron schedules to 5-field Linux format (min hour dom month dow).
/// Deactivates all existing schedules and strips the seconds field from any
/// old 6-field expressions (old format: sec min hour dom month dow).
async fn phase_9_cron_schedule_5_field(pool: &PgPool) -> Result<()> {
    // 1. Deactivate all existing cron jobs — users must explicitly re-enable
    sqlx::query(r#"UPDATE cron_jobs SET active = false, updated_at = NOW() WHERE active = true;"#)
        .execute(pool)
        .await?;

    // 2. Strip the first field (seconds) from any 6-field cron expressions.
    //    Old 6-field format: "sec min hour dom month dow" → 5-field: "min hour dom month dow"
    //    Uses string manipulation: find schedule values with exactly 5 spaces (6 fields)
    //    and remove everything before the first space.
    sqlx::query(
        r#"
        UPDATE cron_jobs
        SET schedule = SUBSTRING(schedule FROM POSITION(' ' IN schedule) + 1),
            updated_at = NOW()
        WHERE schedule ~ '^[^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+$'
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 10: Fix invalid thread causes and add CHECK constraint.
/// Valid causes are 'user' (user-initiated threads) and 'system' (cron/kanban tasks).
async fn phase_10_fix_thread_causes(pool: &PgPool) -> Result<()> {
    // 1. Fix existing invalid causes — 'user-request' -> 'user', 'test' -> 'user', old 'cron'/'kanban' -> 'system'
    sqlx::query(
        r#"
        UPDATE threads SET cause = 'user'
        WHERE cause NOT IN ('user', 'system')
        "#,
    )
    .execute(pool)
    .await?;

    // 2. Add CHECK constraint (idempotent via DO block)
    sqlx::query(
        r#"
        DO $$ BEGIN
            ALTER TABLE threads ADD CONSTRAINT chk_thread_cause
                CHECK (cause IN ('user', 'system'));
        EXCEPTION
            WHEN duplicate_object THEN NULL;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 11: Rename msg_type 'tool_result' to 'tool-result' for consistency.
async fn phase_11_rename_tool_result_msg_type(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE messages SET msg_type = 'tool-result'
        WHERE msg_type = 'tool_result'
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 12: Migrate message role 'user' to 'cause'.
/// The only message roles should be 'cause' (initiating message) and 'agent' (response).
async fn phase_12_migrate_user_role(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE messages SET role = 'cause'
        WHERE role = 'user'
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Phase 13: Migrate actions from DB to YAML file.
/// Drops FK on cron_jobs.action_id and drops the actions table.
async fn phase_13_migrate_actions_to_yaml(pool: &PgPool) -> Result<()> {
    // 1. Check if the actions table still exists
    let table_exists: bool = sqlx::query_scalar(
        r#"SELECT EXISTS (SELECT FROM information_schema.tables WHERE table_name = 'actions')"#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false);

    if !table_exists {
        return Ok(());
    }

    // 2. Drop the FK constraint on cron_jobs.action_id first
    sqlx::query(
        r#"
        DO $$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'cron_jobs_action_id_fkey') THEN
                ALTER TABLE cron_jobs DROP CONSTRAINT cron_jobs_action_id_fkey;
            END IF;
        END $$;
        "#,
    )
    .execute(pool)
    .await?;

    // 3. Drop the actions table
    sqlx::query(r#"DROP TABLE IF EXISTS actions;"#)
        .execute(pool)
        .await?;

    // 4. Drop the actions_id_seq sequence
    sqlx::query(r#"DROP SEQUENCE IF EXISTS actions_id_seq;"#)
        .execute(pool)
        .await?;

    tracing::info!("[migration] Phase 13 complete: actions table dropped, FK removed");
    Ok(())
}
