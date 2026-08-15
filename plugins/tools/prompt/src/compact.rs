use std::path::Path;

use crate::chat_message::ChatMessage;
use crate::dump;

/// Header marker of the frozen compaction-summary block.
///
/// The block is a SINGLE system message living at a FIXED index right after
/// the never-touched preamble (main system prompt + cause message). Its
/// content is reused VERBATIM on every call between compactions, so the
/// prefix before the growing conversation tail stays byte-identical — this
/// is what preserves DeepSeek prefix caching across iterations.
pub const COMPACTION_SUMMARY_MARKER: &str = "=== Compaction Summary ===";

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

/// Compact old assistant tool-call turns into a single FROZEN summary block.
///
/// Cache-friendly compaction: the OLD approach replaced each drained
/// assistant message IN PLACE with an inline `[context compacted: …]` marker
/// at its original position and deleted the tool messages around it. Every
/// byte after the first drained message shifted, so the common prefix ended
/// at the first drained turn and the whole tail was a cache miss on every
/// call (live 2026-08-14: `cached_tokens` frozen at 12,032 while the prompt
/// grew 72K→86K tokens).
///
/// The NEW approach:
/// - The drained region (the oldest `len - keep_recent` assistant tool-call
///   turns plus their tool results) is REPLACED by ONE system message
///   (`=== Compaction Summary ===`) instead of scattered inline markers.
/// - The block sits at a fixed index = head of the drained region, i.e.
///   right after the never-touched preamble (system prompt + cause + any
///   early non-drained messages). Everything before it is byte-identical
///   across calls.
/// - Between compactions the block content is reused VERBATIM; only at the
///   NEXT compaction does newly drained content APPEND to it (strict
///   superset — the old text is preserved byte-for-byte). The array shape is
///   always `[preamble][frozen summary][growing tail]`.
/// - No drain (tool-call turns <= keep_recent) -> nothing removed/inserted,
///   the caller returns `messages: null` and the core leaves the array alone.
///
/// WS-2/WS-3: before tool-role messages are drained, a JSON-lines digest of
/// each destroyed tool result is appended to `context-<current_iteration>.json`
/// in the thread dir (deduped, 200KB cap, keep last 3 dump files) and
/// read-type results are excerpted into `auto-notes.md` — the agent's only
/// recovery channel for drained read content. Both behaviors are preserved
/// exactly.
pub fn compact_old_assistant_messages(
    messages: &mut Vec<ChatMessage>,
    keep_recent: usize,
    thread_dir: Option<&Path>,
    current_iteration: u32,
    settings: &CompactSettings,
) -> CompactOutcome {
    let mut outcome = CompactOutcome::default();

    // The frozen summary block, once created, is the FIRST system message
    // whose content starts with the marker. It is never a drain candidate
    // (system role) and always sits before the first tool-call turn, so its
    // index is stable across calls and compactions.
    let summary_idx = messages
        .iter()
        .position(|m| m.role == "system" && m.content.starts_with(COMPACTION_SUMMARY_MARKER));

    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "assistant" && m.tool_calls.is_some())
        .map(|(i, _)| i)
        .collect();

    // Null-contract: nothing to drain -> caller returns `messages: null` and
    // the core leaves the array untouched (byte-identical prefix preserved).
    if tool_indices.len() <= keep_recent {
        return outcome;
    }

    let n_drain = tool_indices.len() - keep_recent;
    let drain_start = tool_indices[0]; // head of the drained region
                                       // End of the drained span: just past the LAST drained turn's tool
                                       // results. When keep_recent == 0 every turn is drained, so
                                       // tool_indices[n_drain] would be out of bounds — walk forward from the
                                       // last drained assistant instead. When keep_recent > 0 this lands on
                                       // the first KEPT turn (tool results immediately precede it).
    let mut drain_end = tool_indices[n_drain - 1] + 1;
    while drain_end < messages.len() && messages[drain_end].role == "tool" {
        drain_end += 1;
    }

    // Build the summary entries for the drained span [drain_start, drain_end).
    // The span contains the drained assistant tool-call turns, their tool
    // results, and any interleaved non-tool messages (rare system nudges).
    let mut entries: Vec<String> = Vec::new();
    let mut i = drain_start;
    while i < drain_end {
        let m = &messages[i];
        if m.role == "assistant" && m.tool_calls.is_some() {
            if let Some(ref calls) = m.tool_calls {
                let names: Vec<String> = calls.iter().map(|tc| tc.function.name.clone()).collect();

                let mut tool_end = i + 1;
                while tool_end < drain_end && messages[tool_end].role == "tool" {
                    tool_end += 1;
                }
                let tool_count = tool_end - i - 1;

                // WS-2: durable dump of the tool results about to be drained.
                if let Some(dir) = thread_dir {
                    for tm in &messages[i + 1..tool_end] {
                        if tm.role == "tool" && !tm.content.is_empty() {
                            let tool_name = tm.name.as_deref().unwrap_or("");
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
                                &tm.content,
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
                                        tm.content
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

                // Excerpt for the summary entry (same caps as before).
                let mut excerpt = String::new();
                let mut total_excerpt = 0;
                for tm in &messages[i + 1..tool_end] {
                    let tool_name = tm.name.as_deref().unwrap_or("");
                    let is_read = is_read_type_tool(tool_name);
                    let excerpt_chars = if is_read {
                        settings.read_excerpt_chars
                    } else {
                        settings.tool_excerpt_chars
                    };
                    let content_preview: String = tm.content.chars().take(excerpt_chars).collect();
                    let chunk_len = content_preview.len();
                    if total_excerpt + chunk_len > settings.total_excerpt_cap {
                        break;
                    }
                    total_excerpt += chunk_len;
                    excerpt.push_str(&content_preview);
                    excerpt.push('\n');
                }

                if excerpt.trim().is_empty() {
                    entries.push(format!(
                        "- [iter {}] {} → (results drained)",
                        current_iteration,
                        names.join(", ")
                    ));
                } else {
                    entries.push(format!(
                        "- [iter {}] {} →\n{}",
                        current_iteration,
                        names.join(", "),
                        excerpt.trim_end()
                    ));
                }
                outcome.removed += tool_count;
                i = tool_end;
            } else {
                i += 1;
            }
        } else if m.role == "system" && m.content.starts_with(COMPACTION_SUMMARY_MARKER) {
            // Defensive: never fold the summary block into itself. (The
            // block always sits before the drained span in practice.)
            i += 1;
        } else {
            // Non-tool-call message inside the drained span (system nudges,
            // plain assistant text, etc.): preserve a preview so the
            // information is not silently lost.
            let total = m.content.chars().count();
            let preview: String = m.content.chars().take(800).collect();
            let suffix = if total > 800 { "…" } else { "" };
            entries.push(format!("- [{}] {}{}", m.role, preview, suffix));
            i += 1;
        }
    }

    let joined_entries = entries.join("\n");
    match summary_idx {
        Some(idx) => {
            // Frozen block exists: append VERBATIM (strict superset). The
            // already-frozen text keeps its exact bytes; only new entries
            // are added at the end.
            messages[idx].content = format!("{}\n{}", messages[idx].content, joined_entries);
            messages.drain(drain_start..drain_end);
        }
        None => {
            // First compaction: create the block at the head of the drained
            // region — immediately after the never-touched preamble — and
            // remove the drained span (shifted by the insertion).
            let block = ChatMessage {
                role: "system".to_string(),
                content: format!(
                    "{}\nFrozen prefix block: older conversation turns were compacted into this summary, oldest first. Everything before this block is the fixed preamble; everything after is the live conversation. Recover destroyed read results from auto-notes.md / context-*.json dumps.\n{}",
                    COMPACTION_SUMMARY_MARKER, joined_entries
                ),
                tool_call_id: None,
                tool_calls: None,
                name: None,
            };
            messages.insert(drain_start, block);
            messages.drain(drain_start + 1..drain_end + 1);
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_message::{ToolCallData, ToolCallFunction};

    fn settings() -> CompactSettings {
        CompactSettings {
            tool_excerpt_chars: 800,
            total_excerpt_cap: 4000,
            read_excerpt_chars: 2000,
        }
    }

    fn user_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    fn assistant_msg(content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        }
    }

    fn tool_call_msg(name: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: Some(vec![ToolCallData {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: name.to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            name: None,
        }
    }

    fn tool_result(name: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
            name: Some(name.to_string()),
        }
    }

    fn json(messages: &[ChatMessage]) -> String {
        serde_json::to_string(messages).unwrap()
    }

    fn summary_pos(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .position(|m| m.content.starts_with(COMPACTION_SUMMARY_MARKER))
            .expect("summary block must be present")
    }

    fn six_turn_conversation() -> Vec<ChatMessage> {
        let mut msgs = vec![user_msg("start")];
        for i in 0..6 {
            msgs.push(tool_call_msg("filesystem_read", &format!("reading {i}")));
            msgs.push(tool_result("filesystem_read", &format!("result {i}")));
        }
        msgs.push(assistant_msg("final answer"));
        msgs
    }

    // (1) Null-contract: no drain -> nothing removed/inserted, the array is
    // byte-identical afterwards (the caller returns `messages: null` and the
    // core leaves the prefix untouched).
    #[test]
    fn no_drain_leaves_messages_byte_identical() {
        let mut msgs = vec![user_msg("start")];
        for i in 0..2 {
            msgs.push(tool_call_msg("filesystem_read", &format!("reading {i}")));
            msgs.push(tool_result("filesystem_read", &format!("result {i}")));
        }
        msgs.push(assistant_msg("final"));
        let before = json(&msgs);
        let outcome = compact_old_assistant_messages(&mut msgs, 3, None, 7, &settings());
        assert_eq!(outcome.removed, 0, "no drain -> nothing removed");
        assert_eq!(
            json(&msgs),
            before,
            "no-drain call must not rewrite the array (byte-identical)"
        );
    }

    // (2) Frozen-block property: after one compaction the array is
    // [preamble][frozen summary][kept tail]; the summary is a SINGLE system
    // message at a fixed index right after the preamble, the tail survives
    // verbatim, and the drained turns are folded into the block.
    #[test]
    fn first_compaction_creates_frozen_summary_at_fixed_position() {
        let mut msgs = six_turn_conversation();
        let outcome = compact_old_assistant_messages(&mut msgs, 2, None, 7, &settings());
        assert!(outcome.removed >= 4, "4 of 6 tool-result messages drained");

        // Shape: [preamble (user cause)][summary][tail].
        assert_eq!(msgs[0].role, "user", "preamble must never be touched");
        assert_eq!(msgs[1].role, "system", "summary must be a system message");
        assert!(msgs[1].content.starts_with(COMPACTION_SUMMARY_MARKER));
        assert_eq!(
            msgs.len(),
            7,
            "6 turns + final -> user, summary, 4 tail, final"
        );

        // Kept tail survives verbatim (2 most recent turns + final answer).
        assert_eq!(msgs[2].content, "reading 4");
        assert_eq!(msgs[3].content, "result 4");
        assert_eq!(msgs[4].content, "reading 5");
        assert_eq!(msgs[5].content, "result 5");
        assert_eq!(msgs[6].content, "final answer");

        // Drained turns are folded into the block (names + excerpts).
        assert!(msgs[1].content.contains("filesystem_read"));
        assert!(msgs[1].content.contains("result 0"));
        assert!(msgs[1].content.contains("result 3"));
    }

    // (3) Byte-identical prefix across no-drain calls, including a growing
    // tail (the core appends messages between compactions).
    #[test]
    fn frozen_summary_prefix_is_byte_identical_across_calls() {
        let mut msgs = six_turn_conversation();
        compact_old_assistant_messages(&mut msgs, 2, None, 7, &settings());
        let pos = summary_pos(&msgs);
        assert_eq!(pos, 1);
        let prefix_before = json(&msgs[..=pos]);
        let full_before = json(&msgs);

        // Two no-drain calls (keep_recent == current tool-call count): the
        // array must be byte-identical after each.
        let tool_turns = msgs
            .iter()
            .filter(|m| m.role == "assistant" && m.tool_calls.is_some())
            .count();
        for iter in 8..10 {
            let before = json(&msgs);
            let outcome =
                compact_old_assistant_messages(&mut msgs, tool_turns, None, iter, &settings());
            assert_eq!(outcome.removed, 0, "no drain expected on call {iter}");
            assert_eq!(json(&msgs), before, "array must stay byte-identical");
        }
        assert_eq!(json(&msgs), full_before);

        // Growing tail: append fresh turns; a no-drain call must leave the
        // [preamble][summary] prefix byte-identical.
        msgs.push(tool_call_msg("filesystem_read", "reading 6"));
        msgs.push(tool_result("filesystem_read", "result 6"));
        let outcome =
            compact_old_assistant_messages(&mut msgs, tool_turns + 1, None, 10, &settings());
        assert_eq!(outcome.removed, 0);
        assert_eq!(
            json(&msgs[..=pos]),
            prefix_before,
            "[preamble][frozen summary] must be byte-identical as the tail grows"
        );
    }

    // (4) Strict-superset property: a second compaction appends new entries
    // to the existing block and keeps its position — the first summary's
    // text is preserved verbatim (byte-for-byte), never re-rendered.
    #[test]
    fn second_compaction_summary_is_strict_superset() {
        let mut msgs = six_turn_conversation();
        compact_old_assistant_messages(&mut msgs, 2, None, 7, &settings());
        let pos = summary_pos(&msgs);
        let summary1 = msgs[pos].content.clone();

        // Grow the tail by 2 turns and compact again (keep_recent=2 drains
        // the 2 oldest turns of the current tail).
        msgs.push(tool_call_msg("filesystem_read", "reading 6"));
        msgs.push(tool_result("filesystem_read", "result 6"));
        msgs.push(tool_call_msg("filesystem_read", "reading 7"));
        msgs.push(tool_result("filesystem_read", "result 7"));
        msgs.push(assistant_msg("done"));
        let outcome = compact_old_assistant_messages(&mut msgs, 2, None, 8, &settings());
        assert_eq!(
            outcome.removed, 2,
            "2 tool messages of the old tail drained"
        );

        // Position unchanged; content is a strict superset of summary1.
        assert_eq!(
            summary_pos(&msgs),
            pos,
            "summary block position must not move"
        );
        let summary2 = &msgs[pos].content;
        assert!(
            summary2.starts_with(&summary1),
            "second compaction must preserve the first summary verbatim"
        );
        assert!(summary2.len() > summary1.len(), "new entries must append");
        assert!(
            summary2.contains("result 5"),
            "newly drained content must be appended to the frozen block"
        );
        assert!(
            !summary1.contains("result 5"),
            "fixture sanity: turn 5 was kept after compaction 1"
        );

        // The newest turns survive verbatim as the tail (the intermediate
        // "final answer" text sits between the summary and the new turns).
        assert!(msgs.iter().any(|m| m.content == "reading 6"));
        assert!(msgs.iter().any(|m| m.content == "reading 7"));
        assert!(msgs.iter().any(|m| m.content == "done"));
    }

    // (5) WS-2/WS-3 retention preserved: drained read-type results are
    // dumped to context-<iter>.json AND excerpted into auto-notes.md.
    #[test]
    fn drained_read_results_dumped_and_auto_noted() {
        let tmp = std::env::temp_dir().join(format!(
            "prompt-compact-stable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mut msgs = vec![user_msg("read the files")];
        for i in 0..4 {
            msgs.push(tool_call_msg("filesystem_read", &format!("reading {i}")));
            msgs.push(tool_result(
                "filesystem_read",
                &format!("FILE CONTENT {i} ").repeat(50),
            ));
        }
        msgs.push(assistant_msg("done"));

        let outcome = compact_old_assistant_messages(&mut msgs, 2, Some(&tmp), 7, &settings());
        assert!(outcome.removed >= 2);
        assert_eq!(outcome.dump_file.as_deref(), Some("context-7.json"));
        // All turns share the same tool+args in this fixture, so append_dump
        // dedupes the identical (file, tool+args) digests — >= 1 proves the
        // durable dump path fires (dedupe itself is covered in dump.rs).
        assert!(outcome.dump_entries >= 1);

        let dump_text = std::fs::read_to_string(tmp.join("context-7.json"))
            .unwrap_or_else(|_| panic!("context dump missing"));
        assert!(dump_text.contains("filesystem_read"));
        let notes_text = std::fs::read_to_string(tmp.join("auto-notes.md"))
            .unwrap_or_else(|_| panic!("auto-notes missing"));
        assert!(notes_text.contains("[engine:auto-note filesystem_read]"));
        assert!(notes_text.contains("FILE CONTENT"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
