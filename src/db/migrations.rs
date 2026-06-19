use anyhow::Result;
use sql_forge::sql_forge;
use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> Result<()> {
    // Enable pgvector extension — wrapped in DO block so it doesn't fail
    // if pgvector isn't installed (optional vector support).
    sql_forge!(
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
    sql_forge!(
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
    sql_forge!(
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
    sql_forge!(
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

    // Migration: add msg_type, msg_subtype, iteration_count columns
    // (idempotent — skips if columns already exist)
    sql_forge!(
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
    sql_forge!(
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

    sql_forge!(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_thread_seq
            ON messages(thread_id, thread_sequence);
        "#,
    )
    .execute(pool)
    .await?;

    // Profile and model columns for channels
    sql_forge!(
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
    sql_forge!(
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

    // Drop base_path from profiles (path is derived as <data_dir>/profiles/<name>/)
    sql_forge!(
        r#"
        ALTER TABLE profiles DROP COLUMN IF EXISTS base_path;
        "#,
    )
    .execute(pool)
    .await?;

    // Create profiles table
    sql_forge!(
        r#"
        CREATE TABLE IF NOT EXISTS profiles (
            id              BIGSERIAL PRIMARY KEY,
            name            TEXT NOT NULL UNIQUE,
            model           TEXT,
            provider        TEXT,
            base_url        TEXT,
            api_key         TEXT,
            max_tokens      INT,
            temperature     DOUBLE PRECISION,
            allowed_tools   JSONB DEFAULT '[]',
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Migration: make thread_id nullable so seq-0 messages can be inserted
    // without a pre-determined thread_id, then set thread_id = id after insert.
    sql_forge!(
        r#"
        ALTER TABLE messages
        ALTER COLUMN thread_id DROP NOT NULL;
        "#,
    )
    .execute(pool)
    .await?;

    // ── Kanban tasks table ──
    sql_forge!(
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
    sql_forge!(
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
    sql_forge!(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS display_name TEXT NOT NULL DEFAULT ''
        "#,
    )
    .execute(pool)
    .await?;

    // Add running flag for atomic concurrency guard
    sql_forge!(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS running BOOLEAN NOT NULL DEFAULT false
        "#,
    )
    .execute(pool)
    .await?;

    // Migration: add iterations column (per-LLM-call counter)
    sql_forge!(
        r#"
        ALTER TABLE messages
        ADD COLUMN IF NOT EXISTS iterations INT NOT NULL DEFAULT 0
        "#,
    )
    .execute(pool)
    .await?;

    // ── Read-only user for query_database tool ──
    sql_forge!(
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

    sql_forge!("GRANT CONNECT ON DATABASE omniagent TO omniagent_readonly")
        .execute(pool)
        .await?;

    sql_forge!("GRANT USAGE ON SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await?;

    sql_forge!("GRANT SELECT ON ALL TABLES IN SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await?;

    sql_forge!("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO omniagent_readonly")
        .execute(pool)
        .await?;

    // ── Add channel_id and profile to kanban_tasks ──
    sql_forge!(
        r#"
        ALTER TABLE kanban_tasks
        ADD COLUMN IF NOT EXISTS channel_id BIGINT REFERENCES channels(id),
        ADD COLUMN IF NOT EXISTS profile TEXT
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add channel_id and profile to cron_jobs ──
    sql_forge!(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS channel_id BIGINT REFERENCES channels(id),
        ADD COLUMN IF NOT EXISTS profile TEXT
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add readonly column to channels ──
    sql_forge!(
        r#"
        ALTER TABLE channels
        ADD COLUMN IF NOT EXISTS readonly BOOLEAN NOT NULL DEFAULT false
        "#,
    )
    .execute(pool)
    .await?;

    // ── Add updated_at to cron_jobs for stale-lock detection ──
    sql_forge!(
        r#"
        ALTER TABLE cron_jobs
        ADD COLUMN IF NOT EXISTS updated_at TIMESTAMP WITH TIME ZONE DEFAULT NOW()
        "#,
    )
    .execute(pool)
    .await?;

    // ── Summaries table for cross-thread thread summaries ──
    sql_forge!(
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
    sql_forge!(
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

    sql_forge!(
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
    sql_forge!(
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
    sql_forge!(
        r#"
        DO $$ BEGIN
            ALTER TABLE messages ALTER COLUMN thread_id SET NOT NULL;
        EXCEPTION
            WHEN others THEN NULL;
        END $$;
        "#
    )
    .execute(pool)
    .await?;

    // Add closed column to channels (default false — channels start opened)
    sql_forge!(
        r#"
        ALTER TABLE channels
        ADD COLUMN IF NOT EXISTS closed BOOLEAN NOT NULL DEFAULT false
        "#
    )
    .execute(pool)
    .await?;

    // Index on threads for channel_id + status queries
    sql_forge!(
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
    sql_forge!(
        r#"
        ALTER TABLE threads
        ADD COLUMN IF NOT EXISTS terminal BOOLEAN NOT NULL DEFAULT false
        "#
    )
    .execute(pool)
    .await?;

    Ok(())
}
