// Database schema definitions.
//
// This module documents the database table schemas for the OmniAgent system.
// Migrations are run via raw SQL in the migrations module.
//
// ── channels ──────────────────────────────────────────────────────────────
//
// Stores communication channels (e.g., Telegram group/channel, cron jobs).
//
//  id          BIGSERIAL PRIMARY KEY        -- auto-incrementing
//  name        TEXT NOT NULL                -- e.g. "user-lucas", "cron-daily-backup"
//  platform    TEXT NOT NULL                -- e.g. "telegram", "cron"
//  external_id TEXT NOT NULL                -- e.g. Telegram chat ID (legacy, same as resource_identifier)
//  resource_identifier TEXT                 -- identifier within the platform (chat_id, session id, etc.)
//  cause       TEXT NOT NULL                -- 'user' or 'cron'
//  metadata    JSONB DEFAULT '{}'           -- arbitrary metadata
//  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
//  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
//
//  UNIQUE(platform, external_id)
//  UNIQUE(platform, resource_identifier)
//
// ── messages ──────────────────────────────────────────────────────────────
//
// Stores messages received across channels, including agent replies and tool
// calls. Messages are grouped into threads for conversation tracking.
//
//  id               BIGSERIAL PRIMARY KEY           -- auto-incrementing
//  channel_id       BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE
//  role             TEXT NOT NULL                   -- 'user', 'agent', 'system', 'tool'
//  content          TEXT NOT NULL                   -- message body
//  status           TEXT NOT NULL DEFAULT 'pending'
//                                                   -- 'pending', 'processing', 'completed',
//                                                   -- 'failed', 'skipped'
//  thread_id        BIGINT                          -- groups related messages (sequential); NULL for seq-0 until normalized to id
//  thread_sequence  INT NOT NULL                    -- order within thread
//  external_id      TEXT                            -- e.g. Telegram message ID
//  metadata         JSONB DEFAULT '{}'              -- arbitrary metadata
//  embedding        TEXT                            -- embedding vector as text; cast to
//                                                   -- vector(1536) at query time if the
//                                                   -- pgvector extension is available
//  summary_text     TEXT                            -- cached summary of the message
//  is_summary       BOOL NOT NULL DEFAULT false
//  msg_type         TEXT NOT NULL DEFAULT 'message' -- 'message', 'reasoning', 'tool_call', 'tool_result'
//  msg_subtype      TEXT                            -- optional subtype (tool name, etc.)
//  iteration_count  INT NOT NULL DEFAULT 0          -- which agent turn in the thread
//  created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
//
//  UNIQUE(channel_id, external_id)
//  INDEX(thread_id, thread_sequence)
//
// ── channel_stops ─────────────────────────────────────────────────────────
//
// Tracks channels that have been stopped (paused). When a channel is stopped,
// new pending messages are not processed until the stop is cleared.
//
//  id          BIGSERIAL PRIMARY KEY
//  channel_id  BIGINT NOT NULL REFERENCES channels(id) ON DELETE CASCADE
//  stopped_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
//
//  UNIQUE(channel_id)
//
// ── Indexes ───────────────────────────────────────────────────────────────
//
//  idx_messages_channel_status  ON messages(channel_id, status, created_at)
//  idx_messages_thread          ON messages(thread_id, thread_sequence)
//
// ── Extension ─────────────────────────────────────────────────────────────
//
//  pgvector (CREATE EXTENSION vector) — provides vector(1536) type for
//  embedding storage and similarity search. Optional; the DO block in
//  migrations gracefully handles absence.
//
// ── messages.metadata Conventions ─────────────────────────────────────────
//
// The `metadata` JSONB column stores structured metadata per message.
// Standard top-level keys:
//
//  error_type       string     Present on error messages ('processing', etc.)
//  original_msg_id  int        Original message ID for error messages
//  context          object     Context assembly diagnostics (agent responses)
//    selected_message_ids  []int    Message IDs selected for the prompt
//    wiki_files            []string Wiki file paths referenced
//    block_counts          {}       Char counts per context block label
//    dropped_blocks        []string Block labels dropped due to budget
//    total_chars           int      Total assembled character count
//  grounding        object     Grounding policy metadata
//    policy_applied  bool     Whether grounding policy was applied
