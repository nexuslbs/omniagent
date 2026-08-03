use crate::chat_message::ChatMessage;

/// Max chars of a single tool-result excerpt embedded in the compact marker.
const TOOL_EXCERPT_CHARS: usize = 800;
/// Overall cap for the concatenated tool-result excerpts.
const TOTAL_EXCERPT_CAP: usize = 4000;

/// Compact old assistant messages that contain tool_calls JSON.
///
/// Replaces the full function arguments with a condensed reference
/// like `tool_a(), tool_b()` AND appends a truncated excerpt of each
/// compacted tool-role result (`Result excerpt: <first ~800 chars of
/// each tool message content, joined, capped>`) so the agent keeps the
/// call graph, the tool names AND what it actually learned from the
/// tool results (e.g. file contents) after compaction. Tool-role
/// messages are still drained so the count-reduction budget contract
/// (and the existing `[compact: ...]` marker tests) hold.
pub fn compact_old_assistant_messages(messages: &mut Vec<ChatMessage>, keep_recent: usize) {
    loop {
        let tool_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == "assistant" && m.tool_calls.is_some())
            .map(|(i, _)| i)
            .collect();

        if tool_indices.len() <= keep_recent {
            return;
        }

        let compact_up_to = tool_indices.len() - keep_recent;
        for &idx in tool_indices.iter().take(compact_up_to).rev() {
            if let Some(ref calls) = messages[idx].tool_calls {
                let summary: Vec<String> = calls
                    .iter()
                    .map(|tc| format!("{}()", tc.function.name))
                    .collect();

                let mut tool_end = idx + 1;
                while tool_end < messages.len() && messages[tool_end].role == "tool" {
                    tool_end += 1;
                }

                let tool_count = tool_end - idx - 1;
                let tool_info = if tool_count > 0 {
                    let tool_names: Vec<&str> = messages[idx + 1..tool_end]
                        .iter()
                        .filter_map(|m| m.name.as_deref())
                        .collect();
                    if !tool_names.is_empty() {
                        format!(". Results from: {}", tool_names.join(", "))
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };

                // Condensed but content-bearing digest of the drained tool
                // results: the first ~800 chars of each tool message, joined
                // and capped overall, so the agent retains what it learned
                // (e.g. file contents) even after the tool messages drain.
                let mut excerpt = String::new();
                let mut excerpt_chars = 0usize;
                for m in &messages[idx + 1..tool_end] {
                    if excerpt_chars >= TOTAL_EXCERPT_CAP {
                        break;
                    }
                    if m.content.is_empty() {
                        continue;
                    }
                    let head: String = m.content.chars().take(TOOL_EXCERPT_CHARS).collect();
                    let head_chars = head.chars().count();
                    let more = m.content.chars().count() - head_chars;
                    let mut piece = match m.name.as_deref() {
                        Some(n) if !n.is_empty() => format!("--- {}:\n{}", n, head),
                        _ => head,
                    };
                    if more > 0 {
                        piece.push_str(&format!("[... +{} more chars]", more));
                    }
                    excerpt_chars += piece.chars().count();
                    excerpt.push_str(&piece);
                    excerpt.push('\n');
                }

                let content = if excerpt.is_empty() {
                    if summary.is_empty() {
                        "[compact]".to_string()
                    } else {
                        format!("[compact: {}{}]", summary.join(", "), tool_info)
                    }
                } else if summary.is_empty() {
                    format!("[compact]. Result excerpt: {}", excerpt.trim_end())
                } else {
                    format!(
                        "[compact: {}{}. Result excerpt: {}]",
                        summary.join(", "),
                        tool_info,
                        excerpt.trim_end()
                    )
                };

                messages[idx].content = content;
                messages[idx].tool_calls = None;

                if tool_count > 0 {
                    messages.drain(idx + 1..tool_end);
                }
            }
        }
    }
}
