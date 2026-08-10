use std::path::Path;

use crate::chat_message::ChatMessage;
use crate::dump;

/// Characters of each individual tool result excerpt kept when a tool-call
/// turn is compacted.
pub const TOOL_EXCERPT_CHARS: usize = 800;
/// Overall cap for the concatenated tool-result excerpt (prevents a single
/// compacted message from blowing the context).
pub const TOTAL_EXCERPT_CAP: usize = 4000;

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
                        }
                    }
                    if tool_count > 0 {
                        outcome.dump_file = Some(format!("context-{current_iteration}.json"));
                    }
                }

                let mut excerpt = String::new();
                let mut total_excerpt = 0;
                for m in messages[idx + 1..tool_end].iter() {
                    let content_preview: String =
                        m.content.chars().take(TOOL_EXCERPT_CHARS).collect();
                    let chunk_len = content_preview.len();
                    if total_excerpt + chunk_len > TOTAL_EXCERPT_CAP {
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
