use std::path::Path;

use crate::chat_message::ChatMessage;
use crate::dump;

/// Excerpt/size limits for compaction, sourced from plugin config
/// (plugin.json config_schema + settings.yml) — no hardcoded limits in code.
#[derive(Debug, Clone, Copy)]
pub struct CompactSettings {
    /// Characters of each individual tool result excerpt kept when a
    /// tool-call turn is compacted.
    pub tool_excerpt_chars: usize,
    /// Overall cap for the concatenated tool-result excerpt (prevents a
    /// single compacted message from blowing the context).
    pub total_excerpt_cap: usize,
    /// Per-result excerpt for read-type tools: generous head+tail so the
    /// agent still sees what it learned.
    pub read_excerpt_chars: usize,
}
/// Tools whose results ARE the agent's working memory (file contents,
/// listings, search hits). When compaction must drain them, keep a much
/// larger excerpt than the generic cap — zeroing them forces the agent to
/// re-read the same files (thread 700 death spiral: 117 sed windows).
fn is_read_type_tool(name: &str) -> bool {
    name.starts_with("filesystem_read")
        || name.starts_with("filesystem_list")
        || name.starts_with("filesystem_search")
        || name.starts_with("filesystem_info")
        || name.starts_with("query_database")
        || name.starts_with("search_messages")
        || name.starts_with("search_wiki")
        || name.starts_with("skills_view")
        || name.starts_with("git_status")
        || name.starts_with("git_run-command")
}

/// Outcome of a compaction pass (WS-2/WS-3): how many tool messages were
/// drained, and whether a durable `context-<iter>.json` digest was written.
#[derive(Debug, Default, Clone)]
pub struct CompactOutcome {
    pub removed: usize,
    pub dump_file: Option<String>,
    pub dump_entries: usize,
}

/// Compact old assistant messages that contain tool_calls JSON.
///
/// Removes tool-call turns (assistant message + following tool messages)
/// from the oldest side of the history and replaces the assistant message
/// with a compact summary of the tool calls.
///
/// WS-2: before tool-role messages are drained, a JSON-lines digest of each
/// destroyed tool result is appended to `context-<current_iteration>.json`
/// in the thread dir (deduped, 200KB cap, keep last 3 dump files) so the
/// agent can recover what it learned after compaction.
pub fn compact_old_assistant_messages(
    messages: &mut Vec<ChatMessage>,
    keep_recent: usize,
    thread_dir: Option<&Path>,
    current_iteration: u32,
    settings: &CompactSettings,
) -> CompactOutcome {
    let mut outcome = CompactOutcome::default();
    loop {
        let tool_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "assistant" && m.tool_calls.is_some())
            .map(|(i, _)| i)
            .collect();

        if tool_indices.len() <= keep_recent {
            return outcome;
        }

        let compact_up_to = tool_indices.len() - keep_recent;

        for &idx in tool_indices.iter().take(compact_up_to).rev() {
            if let Some(ref calls) = messages[idx].tool_calls {
                let summary: Vec<String> =
                    calls.iter().map(|tc| tc.function.name.clone()).collect();

                let mut tool_end = idx + 1;
                while tool_end < messages.len() && messages[tool_end].role == "tool" {
                    tool_end += 1;
                }

                let tool_count = tool_end - idx - 1;

                // WS-2: durable dump of the tool results about to be drained.
                if let Some(dir) = thread_dir {
                    for m in &messages[idx + 1..tool_end] {
                        if m.role == "tool" && !m.content.is_empty() {
                            let tool_name = m.name.as_deref().unwrap_or("");
                            let args = calls
                                .iter()
                                .find(|tc| tc.function.name == tool_name)
                                .map(|tc| tc.function.arguments.to_string())
                                .unwrap_or_default();
                            if dump::append_dump(
                                dir,
                                current_iteration,
                                tool_name,
                                &args,
                                &m.content,
                            ) {
                                outcome.dump_entries += 1;
                            }
                            // Auto-note read-type results into the durable
                            // auto-notes.md (re-injected every iteration).
                            // Dumps are forbidden to re-read (rule 12), so
                            // this is the ONLY recovery channel for drained
                            // read content — otherwise the agent forgets
                            // what it read and re-reads the same files
                            // (thread 700: 117 sed windows of the same
                            // ranges, zero commits).
                            if is_read_type_tool(tool_name) {
                                crate::notes::note_append(
                                    dir,
                                    "auto-notes.md",
                                    &format!(
                                        "## [engine:auto-note {tool_name}]\n{}\n",
                                        m.content
                                            .chars()
                                            .take(settings.read_excerpt_chars)
                                            .collect::<String>()
                                    ),
                                );
                            }
                        }
                    }
                    if tool_count > 0 {
                        outcome.dump_file = Some(format!("context-{current_iteration}.json"));
                    }
                }

                let mut excerpt = String::new();
                let mut total_excerpt = 0;
                for m in messages[idx + 1..tool_end].iter() {
                    let tool_name = m.name.as_deref().unwrap_or("");
                    let is_read = is_read_type_tool(tool_name);
                    let excerpt_chars = if is_read {
                        settings.read_excerpt_chars
                    } else {
                        settings.tool_excerpt_chars
                    };
                    let content_preview: String = m.content.chars().take(excerpt_chars).collect();
                    let chunk_len = content_preview.len();
                    if total_excerpt + chunk_len > settings.total_excerpt_cap {
                        break;
                    }
                    total_excerpt += chunk_len;
                    excerpt.push_str(&content_preview);
                    excerpt.push('\n');
                }

                let content = if summary.is_empty() {
                    "[context compacted: tool calls were removed]".to_string()
                } else if excerpt.is_empty() {
                    format!("[context compacted: {}]", summary.join(", "))
                } else {
                    format!(
                        "[context compacted: {} — results excerpt:]\n{}",
                        summary.join(", "),
                        excerpt
                    )
                };

                messages[idx].content = content;
                messages[idx].tool_calls = None;

                if tool_count > 0 {
                    messages.drain(idx + 1..tool_end);
                    outcome.removed += tool_count;
                }
            }
        }
    }
}
