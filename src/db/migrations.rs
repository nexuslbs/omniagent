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
    // Replace it with a non-unique index covering all three.
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

    Ok(())
}
