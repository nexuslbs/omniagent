// Database schema definitions.
//
// This module documents the database table schemas for the OmniAgent system.
// Migrations are run via raw SQL in the migrations module.
//
// ── channels ──────────────────────────────────────────────────────────────
//
// Channel definitions AND runtime state live in {data_dir}/config/channels.yml
// (git-tracked; see src/channels_yaml.rs). There is NO `channels` database
// table and NO foreign keys referencing it — the channels TABLE AND ALL FKs
// WERE DROPPED. The YAML map key is the channel NAME: the stable identifier
// used everywhere — the API channel id, the threads/messages/kanban_tasks/
// summaries channel_id column (TEXT holding the name), and tasks.yml
// `channel:` references. This mirrors the established pattern of referencing
// yml keys by key string: threads.schedule_task_id holds the tasks.yml key,
// threads.workflow_id holds the workflows.yml key, threads.task_id holds the
// kanban task id.
//
// Each channels.yml entry uses BARE field names (no metadata, no external_id,
// no planning_mode, no timestamps):
//
//  <channel name>:
//    resource_identifier: <identifier within the platform>  -- e.g. chat_id, session id
//    platform:            <e.g. "telegram", "cron", "cli">  -- platform-less channel = cli
//    cause:               <'user' | 'system' | 'cron'>
//    profile:             <profile name>
//    model:               <model name>
//    provider:            <provider name>
//    plan:                <true | false>                    -- single plan bool
//
// ── messages ──────────────────────────────────────────────────────────────
//
// Stores messages received across channels, including agent replies and tool
// calls. Messages are grouped into threads for conversation tracking.
//
//  id               BIGSERIAL PRIMARY KEY           -- auto-incrementing
//  channel_id       TEXT NOT NULL                   -- channel NAME (key into channels.yml)
//  role             TEXT NOT NULL                   -- 'cause', 'agent', 'system', 'tool'
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
//  msg_type         TEXT NOT NULL DEFAULT 'message' -- 'message', 'reasoning', 'tool_call', 'tool-result'
//  msg_subtype      TEXT                            -- optional subtype (tool name, etc.)
//  iteration_count  INT NOT NULL DEFAULT 0          -- which agent turn in the thread
//  created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
//
//  UNIQUE(channel_id, external_id)
//  INDEX(thread_id, thread_sequence)
//
// ── Dependent tables ──────────────────────────────────────────────────────
//
// threads.channel_id, messages.channel_id, kanban_tasks.channel_id and
// summaries.channel_id are TEXT columns holding the channel NAME (key into
// channels.yml). There are NO FK constraints referencing channels — the name
// IS the reference, exactly like threads.schedule_task_id / workflow_id /
// task_id reference their respective yml keys.
//
// ── Indexes ───────────────────────────────────────────────────────────────
//
//  idx_messages_channel_status  ON messages(channel_id, status, created_at)
//  idx_messages_thread          ON messages(thread_id, thread_sequence)
//
// ── Extension ─────────────────────────────────────────────────────────────
//
//  pgvector (CREATE EXTENSION vector): provides vector(1536) type for
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
