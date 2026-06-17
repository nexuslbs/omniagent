use anyhow::Result;
use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> Result<()> {
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
        "#,
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

    // Create indexes
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_channel_status
            ON messages(channel_id, status, created_at);
        "#,
    )
    .execute(pool)
    .await?;

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

    // Drop base_path from profiles (path is derived as <data_dir>/profiles/<name>/)
    sqlx::query(
        r#"
        ALTER TABLE profiles DROP COLUMN IF EXISTS base_path;
        "#,
    )
    .execute(pool)
    .await?;

    // Create profiles table
    sqlx::query(
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

    // Index on messages for profile/model queries
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_messages_profile
            ON messages(profile, model);
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

    Ok(())
}
