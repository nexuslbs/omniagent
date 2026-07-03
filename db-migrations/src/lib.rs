//! Database migrations for OmniAgent.
//!
//! Single-phase declarative schema — creates the FINAL state of all tables
//! as they exist after all incremental migrations are applied.
//!
//! No legacy data migrations, no ADD COLUMN / DROP COLUMN evolution steps.
//! Safe to run on every startup (all statements use IF NOT EXISTS).
//!
//! Profile columns (channels.current_profile, threads.profile) have NO
//! DEFAULT — the application supplies the profile name from DEFAULT_PROFILE
//! env var at insert time.

use anyhow::Result;
use sqlx::PgPool;

pub async fn run(pool: &PgPool) -> Result<()> {
    create_extensions(pool).await?;
    create_tables(pool).await?;
    create_indexes(pool).await?;
    create_vector_support(pool).await?;
    create_triggers(pool).await?;
    create_readonly_user(pool).await?;
    seed_kanban_channel(pool).await?;
    tracing::info!("[migration] Schema v2 applied successfully");
    Ok(())
}

// ── Extensions ──────────────────────────────────────────────────────────────

async fn create_extensions(pool: &PgPool) -> Result<()> {
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .execute(pool)
        .await?;

    // pgvector is optional — silently skip if not installed
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
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS channels (
            id                  BIGSERIAL PRIMARY KEY,
            name                TEXT NOT NULL,
            platform            TEXT,
            external_id         TEXT,
            resource_identifier TEXT,
            cause               TEXT NOT NULL,
            metadata            JSONB DEFAULT '{}',
            current_profile     TEXT NOT NULL,
            current_model       TEXT,
            current_provider    TEXT,
            readonly            BOOLEAN NOT NULL DEFAULT false,
            closed              BOOLEAN NOT NULL DEFAULT false,
            planning_mode       TEXT NOT NULL DEFAULT '',
            template            TEXT DEFAULT '',
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(platform, external_id),
            UNIQUE(platform, resource_identifier)
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Channel stops ──────────────────────────────────────────────────────
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

    // ── Threads ───────────────────────────────────────────────────────────
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS threads (
            id                BIGSERIAL PRIMARY KEY,
            status            TEXT NOT NULL DEFAULT 'created',
            cause             TEXT NOT NULL,
            channel_id        BIGINT NOT NULL REFERENCES channels(id),
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
            planning_mode     TEXT NOT NULL DEFAULT '',
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
            channel_id      BIGINT REFERENCES channels(id),
            profile         TEXT,
            archived        BOOLEAN NOT NULL DEFAULT false,
            position        INTEGER,
            template        TEXT DEFAULT '',
            planning_mode   TEXT NOT NULL DEFAULT '',
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
            channel_id        BIGINT REFERENCES channels(id),
            profile           TEXT,
            running           BOOLEAN NOT NULL DEFAULT false,
            action_id         TEXT,
            silent            BOOLEAN NOT NULL DEFAULT false,
            template          TEXT DEFAULT '',
            planning_mode     TEXT NOT NULL DEFAULT ''
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
            channel_id      BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            next_thread_id  BIGINT NOT NULL,
            content         TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );
        "#,
    )
    .execute(pool)
    .await?;

    // ── Channel subscriptions ─────────────────────────────────────────────
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

    // Channels: unique name constraint (created separately so IF NOT EXISTS works)
    sqlx::query(
        r#"DO $$ BEGIN
            IF NOT EXISTS (
                SELECT 1 FROM pg_constraint
                WHERE conname = 'channels_name_key'
            ) THEN
                ALTER TABLE channels ADD CONSTRAINT channels_name_key UNIQUE (name);
            END IF;
        END $$;"#,
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
    let vector_available: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')",
    )
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
        tracing::warn!("[migration] pgvector not available — skipping vector column");
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

// ── Read-only user ──────────────────────────────────────────────────────────

async fn create_readonly_user(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"DO $$
        BEGIN
            CREATE USER omniagent_readonly WITH PASSWORD 'omniagent_readonly';
        EXCEPTION
            WHEN duplicate_object THEN NULL;
        END $$;"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("GRANT CONNECT ON DATABASE omniagent TO omniagent_readonly")
        .execute(pool)
        .await
        .ok();

    sqlx::query("GRANT USAGE ON SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await
        .ok();

    sqlx::query("GRANT SELECT ON ALL TABLES IN SCHEMA public TO omniagent_readonly")
        .execute(pool)
        .await
        .ok();

    sqlx::query(
        "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO omniagent_readonly",
    )
    .execute(pool)
    .await
    .ok();

    tracing::info!("[migration] Read-only user omniagent_readonly configured");
    Ok(())
}

// ── Seed data ───────────────────────────────────────────────────────────────

async fn seed_kanban_channel(pool: &PgPool) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO channels (name, platform, external_id, resource_identifier, cause, current_profile)
        SELECT 'kanban', 'kanban', 'kanban', 'kanban', 'system', ''
        WHERE NOT EXISTS (
            SELECT 1 FROM channels WHERE platform = 'kanban' AND name = 'kanban'
        );
        "#,
    )
    .execute(pool)
    .await?;

    tracing::info!("[migration] Kanban channel seeded");
    Ok(())
}
