use crate::chat_message::ChatMessage;

/// Compact old assistant messages that contain tool_calls JSON.
///
/// Replaces the full function arguments with a condensed reference
/// like `tool_a(), tool_b()` and removes the following tool-role
/// messages entirely. Preserves the `keep_recent` most recent
/// tool-calling assistant messages.
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
                    let tool_names: Vec<&str> = messages[idx..tool_end]
                        .iter()
                        .skip(1)
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

                let condensed = if summary.is_empty() {
                    "[compact]".to_string()
                } else {
                    format!("[compact: {}{}]", summary.join(", "), tool_info)
                };

                messages[idx].content = condensed;
                messages[idx].tool_calls = None;
                messages.drain(idx + 1..tool_end);
            }
        }
    }
}
