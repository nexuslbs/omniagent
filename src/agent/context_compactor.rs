//! Context-overflow forced compaction + retry — task 3 (kill the death spiral).
//!
//! When an LLM provider rejects a request because the accumulated thread
//! context exceeds the model's context window (a "context-length error"),
//! retrying the SAME oversized context can never succeed — the thread dies
//! with no way back (iteration-budget / output-budget exhaustion follows).
//! This module implements the recovery half of context economy:
//!
//!   1. prune over-budget tool results deterministically (task 2, zero LLM
//!      cost — wired in `main_loop.rs`),
//!   2. if pruning is exhausted, FORCE a summary compaction of the OLDEST
//!      portion of the thread and REPLACE it in the in-memory history with a
//!      single "Compact Checkpoint" user message (mirroring dsh's
//!      `user/message` with `surfaceOp: replace`),
//!   3. retry the failed LLM request with the compacted context, bounded by
//!      `max_compaction_retries` (default 2) so a hopeless thread fails
//!      honestly instead of looping forever.
//!
//! Only the in-memory conversation is compacted: the thread history in the
//! DB is never deleted, so full fidelity is preserved. The system prompt
//! (leading `system` messages: memory, template, context, plan, output
//! limit) is NEVER compacted — it stays verbatim for every retry. The newest
//! message always stays verbatim so the model keeps the current request
//! context. Tool-call/result pairing is preserved: a range that contains an
//! assistant tool-call is extended over the paired tool result so no
//! dangling `tool` message survives the replacement.

use crate::llm::{ChatMessage, CompletionRequest, LLMClient};
use std::ops::Range;

/// Max tokens for the compact-checkpoint summary LLM call. The summary
/// replaces the oldest segment, so a small bounded budget is enough; the
/// retried request is the one that needs context headroom.
pub const COMPACT_SUMMARY_MAX_TOKENS: u32 = 1024;

/// Rough token estimate from char count (chars/4 — the common heuristic).
pub fn estimate_tokens(chars: usize) -> usize {
    chars / 4
}

/// Task-3 decision function: after a provider context-length error with the
/// task-2 prune result, should we run a forced summary compaction instead of
/// a bare retry?
///
/// - `max_compactions == 0` disables forced compaction entirely (opt-out).
/// - The first overflow with a NON-empty prune report retries once with the
///   pruned context (`pruned_retry_done == false` → no compact yet): the
///   cheaper deterministic fix gets a chance first.
/// - Compact when pruning changed nothing (tool results are not the culprit)
///   or when a pruned retry already overflowed again (`pruned_retry_done`).
/// - `compactions_used` bounds the total per overflow chain.
pub fn should_force_compact(
    prune_changed: bool,
    pruned_retry_done: bool,
    compactions_used: u32,
    max_compactions: u32,
) -> bool {
    if max_compactions == 0 {
        return false;
    }
    compactions_used < max_compactions && (!prune_changed || pruned_retry_done)
}

/// Select the OLDEST compactable segment of the in-memory conversation.
///
/// Rules (deterministic, Unicode-safe char counting):
/// - leading `system` messages (the system prompt: memory, template,
///   context, plan, output limit) are never compacted,
/// - the oldest messages are covered until at least half of the non-system
///   chars are inside the range (balanced: older half summarized, newer half
///   verbatim),
/// - the newest message always stays verbatim (the model needs the current
///   request context),
/// - the range is extended over any tool result paired with a tool call
///   inside it, so replacing the range never orphans a `tool` message.
///
/// Returns `None` when there is nothing worth compacting (tiny thread).
pub fn select_compaction_range(messages: &[ChatMessage]) -> Option<Range<usize>> {
    // Keep the leading system prompt(s) verbatim.
    let start = messages
        .iter()
        .position(|m| m.role != "system")
        .unwrap_or(messages.len());
    if start >= messages.len() {
        return None;
    }
    let total: usize = messages[start..]
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    if total == 0 {
        return None;
    }
    // Cover the oldest messages until at least half of the non-system chars
    // are inside the range.
    let mut acc = 0usize;
    let mut end = start;
    while end < messages.len() {
        acc += messages[end].content.chars().count();
        end += 1;
        if acc >= total / 2 {
            break;
        }
    }
    // Never compact the newest message — the retry needs it verbatim.
    let max_end = messages.len().saturating_sub(1).max(start);
    let mut end = end.min(max_end);
    // Extend over dangling tool results: an assistant tool-call inside the
    // range whose paired `tool` result lies outside would be orphaned when
    // the range is replaced by the summary checkpoint.
    loop {
        let dangling = messages[start..end].iter().any(|m| {
            m.role == "assistant"
                && m.tool_calls.as_ref().is_some_and(|calls| {
                    calls.iter().any(|c| {
                        !messages[start..end].iter().any(|r| {
                            r.role == "tool" && r.tool_call_id.as_deref() == Some(c.id.as_str())
                        })
                    })
                })
        });
        if !dangling || end >= max_end {
            break;
        }
        end += 1;
    }
    if end <= start {
        return None;
    }
    Some(start..end)
}

/// Build the LLM request that summarizes the compacted range. Tool results
/// are stripped (their content is already pruned/persisted; the summary only
/// needs the gist) and assistant `tool_calls` are nulled (providers such as
/// DeepSeek require complete tool-call chains — same convention as the
/// response-handler summary path).
pub fn build_compact_summary_request(range_messages: &[ChatMessage]) -> CompletionRequest {
    let mut summary_msgs: Vec<ChatMessage> = range_messages
        .iter()
        .filter(|m| m.role != "tool")
        .map(|m| {
            let mut cloned = m.clone();
            if cloned.role == "assistant" && cloned.tool_calls.is_some() {
                cloned.tool_calls = None;
            }
            cloned
        })
        .collect();
    summary_msgs.push(ChatMessage::system(
        "The messages above are the OLDEST segment of an ongoing conversation that exceeded the \
         model's context window. Write a concise but complete summary of this segment: the task \
         context, what was decided, what was done, and what remains. The summary will REPLACE \
         this segment in the conversation as a 'Compact Checkpoint', so it must preserve every \
         fact a reader needs to continue the task without re-reading the original messages.",
    ));
    CompletionRequest {
        messages: summary_msgs,
        max_tokens: Some(COMPACT_SUMMARY_MAX_TOKENS),
        temperature: 0.3,
        stream: false,
        tools: None,
    }
}

/// The `user`-role message that replaces the compacted range in the in-memory
/// conversation (mirrors dsh's `user/message` with `surfaceOp: replace`).
pub fn compact_checkpoint_message(summary: &str) -> ChatMessage {
    ChatMessage::user(&format!(
        "=== Compact Checkpoint (earlier conversation summarized — full fidelity preserved in thread history) ===\n\n{}",
        summary
    ))
}

/// Accounting for one compaction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// Number of messages shadowed by the checkpoint.
    pub msgs_compacted: usize,
    /// Chars of the replaced messages (Unicode code points).
    pub chars_before: usize,
    /// Chars of the checkpoint message (Unicode code points).
    pub chars_after: usize,
}

impl CompactionReport {
    /// Estimated tokens freed by this compaction (chars/4 heuristic).
    pub fn est_tokens_saved(&self) -> usize {
        estimate_tokens(self.chars_before.saturating_sub(self.chars_after))
    }
}

/// Replace `messages[range]` with a single compact-checkpoint user message
/// carrying `summary`. Returns the accounting report.
pub fn replace_range(
    messages: &mut Vec<ChatMessage>,
    range: Range<usize>,
    summary: &str,
) -> CompactionReport {
    let msgs_compacted = range.end.saturating_sub(range.start);
    let chars_before: usize = messages[range.clone()]
        .iter()
        .map(|m| m.content.chars().count())
        .sum();
    let checkpoint = compact_checkpoint_message(summary);
    let chars_after = checkpoint.content.chars().count();
    messages.splice(range.clone(), std::iter::once(checkpoint));
    CompactionReport {
        msgs_compacted,
        chars_before,
        chars_after,
    }
}

/// Forced compaction recovery (task 3): summarize the OLDEST segment of the
/// in-memory conversation with an LLM call and replace it with a Compact
/// Checkpoint. Returns `Ok(None)` when there is nothing compactable (tiny
/// thread), `Err` when the summary model call failed (caller falls through to
/// the standard provider-retry path). On success the caller retries the
/// failed LLM request with the compacted `messages`.
pub async fn compact_oldest_segment(
    llm: &LLMClient,
    messages: &mut Vec<ChatMessage>,
    thread_id: i64,
) -> Result<Option<CompactionReport>, String> {
    let Some(range) = select_compaction_range(messages) else {
        tracing::warn!(
            "[compact] Thread {}: context-length error but no compactable range (tiny thread)",
            thread_id
        );
        return Ok(None);
    };
    let request = build_compact_summary_request(&messages[range.clone()]);
    let resp = llm
        .completion(request)
        .await
        .map_err(|e| format!("summary model call failed: {:?}", e))?;
    let summary = if resp.content.trim().is_empty() {
        resp.reasoning.clone().unwrap_or_default()
    } else {
        resp.content
    };
    if summary.trim().is_empty() {
        return Err("summary model returned empty content and no reasoning".to_string());
    }
    let report = replace_range(messages, range, &summary);
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCallData;

    fn asst_with_tool_calls(ids: &[&str]) -> ChatMessage {
        let mut m = ChatMessage::assistant("");
        m.tool_calls = Some(
            ids.iter()
                .map(|id| ToolCallData {
                    id: id.to_string(),
                    call_type: "function".to_string(),
                    function: crate::llm::ToolCallFunction {
                        name: "filesystem_read".to_string(),
                        arguments: "{}".to_string(),
                    },
                })
                .collect(),
        );
        m
    }

    // ── select_compaction_range ────────────────────────────────────────────

    #[test]
    fn range_skips_system_prefix_and_compacts_oldest_half() {
        // 0=system, 1..6 = six 100-char non-system messages.
        let msgs: Vec<ChatMessage> = std::iter::once(ChatMessage::system("sys"))
            .chain((0..6).map(|_| ChatMessage::user(&"x".repeat(100))))
            .collect();
        let range = select_compaction_range(&msgs).expect("range");
        // start = first non-system = 1; total = 600; half = 300 → cover
        // messages 1..=3 (100+100+100), so end = 4.
        assert_eq!(range, 1..4);
        // The newest messages stay verbatim (indices 4,5 untouched).
        assert!(range.end < msgs.len());
        assert_eq!(msgs[5].content.chars().count(), 100);
    }

    #[test]
    fn range_never_compacts_the_newest_message() {
        // 2 system + 10 non-system messages of equal size: half-cover lands
        // at end=7 (index 7), but cap leaves the newest (index 11) verbatim.
        let msgs: Vec<ChatMessage> = vec![ChatMessage::system("sys1"), ChatMessage::system("sys2")]
            .into_iter()
            .chain((0..10).map(|_| ChatMessage::user(&"y".repeat(50))))
            .collect();
        let range = select_compaction_range(&msgs).expect("range");
        assert_eq!(range.start, 2);
        assert!(range.end < msgs.len());
        assert_eq!(msgs[range.end].content, msgs[11].content);
    }

    #[test]
    fn tiny_thread_has_no_compactable_range() {
        // system + single user message: nothing to shadow.
        let msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hello")];
        assert_eq!(select_compaction_range(&msgs), None);
        // Only system messages.
        let sys_only = vec![ChatMessage::system("sys")];
        assert_eq!(select_compaction_range(&sys_only), None);
        // Empty.
        assert_eq!(select_compaction_range(&[]), None);
    }

    #[test]
    fn range_extends_over_dangling_tool_result() {
        // 0=system, 1=user(10), 2=assistant with tool_call call_1(10),
        // 3=user(100), 4=tool result call_1(50), 5=assistant(10).
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(&"u".repeat(10)),
            asst_with_tool_calls(&["call_1"]),
            ChatMessage::user(&"v".repeat(100)),
            ChatMessage::tool_result("call_1", "filesystem_read", &"r".repeat(50)),
            ChatMessage::assistant("done"),
        ];
        let range = select_compaction_range(&msgs).expect("range");
        // Half of 180 = 90: cover user(10)+call(10)+user(100) → end=4.
        // call_1 is in the range but its result (index 4) is not → extend to 5.
        assert_eq!(range, 1..5);
        // The tool result is inside the range → no orphaned tool message.
        assert_eq!(msgs[4].role, "tool");
        assert!(range.contains(&4));
        // The final assistant message stays verbatim.
        assert_eq!(range.end, 5);
    }

    // ── should_force_compact (retry bounding) ─────────────────────────────

    #[test]
    fn compaction_disabled_when_max_zero() {
        assert!(!should_force_compact(false, false, 0, 0));
        assert!(!should_force_compact(true, true, 0, 0));
    }

    #[test]
    fn first_overflow_with_prune_retries_once_before_compacting() {
        // Prune changed something and no pruned retry happened yet → bare retry.
        assert!(!should_force_compact(true, false, 0, 2));
    }

    #[test]
    fn no_prune_means_compact_immediately() {
        // Nothing left to prune → tool results are not the culprit → compact.
        assert!(should_force_compact(false, false, 0, 2));
    }

    #[test]
    fn second_overflow_after_pruned_retry_compacts() {
        // The pruned retry overflowed again → pruning is exhausted → compact.
        assert!(should_force_compact(true, true, 0, 2));
        assert!(should_force_compact(false, true, 1, 2));
    }

    #[test]
    fn compaction_budget_is_bounded() {
        // used == max → never compact again (fail honestly instead).
        assert!(!should_force_compact(false, false, 2, 2));
        assert!(!should_force_compact(true, true, 1, 1));
        // used below max → still allowed.
        assert!(should_force_compact(false, false, 1, 2));
    }

    // ── checkpoint + summary request ───────────────────────────────────────

    #[test]
    fn checkpoint_message_shape() {
        let msg = compact_checkpoint_message("we did things");
        assert_eq!(msg.role, "user");
        assert!(msg.content.starts_with("=== Compact Checkpoint"));
        assert!(msg.content.contains("we did things"));
    }

    #[test]
    fn summary_request_strips_tool_messages_and_tool_calls() {
        let range = vec![
            ChatMessage::user("task"),
            asst_with_tool_calls(&["call_1"]),
            ChatMessage::tool_result("call_1", "filesystem_read", &"r".repeat(10_000)),
            ChatMessage::assistant("next"),
        ];
        let req = build_compact_summary_request(&range);
        assert!(req.messages.iter().all(|m| m.role != "tool"));
        assert!(req.messages.iter().all(|m| m.tool_calls.is_none()));
        assert_eq!(req.max_tokens, Some(COMPACT_SUMMARY_MAX_TOKENS));
        assert_eq!(req.temperature, 0.3);
        assert!(!req.stream);
        assert!(req.tools.is_none());
        // Last message is the summarizing instruction.
        assert!(req
            .messages
            .last()
            .unwrap()
            .content
            .contains("Compact Checkpoint"));
        // The gist of the range is preserved for the summarizer.
        assert!(req.messages.iter().any(|m| m.content == "task"));
    }

    // ── replace_range ──────────────────────────────────────────────────────

    #[test]
    fn replace_range_collapses_segment_and_preserves_newest() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("a"),
            ChatMessage::user("b"),
            ChatMessage::user("c"),
            ChatMessage::user("newest"),
        ];
        let report = replace_range(&mut msgs, 1..4, "summary text");
        assert_eq!(report.msgs_compacted, 3);
        assert_eq!(msgs.len(), 3); // system + checkpoint + newest
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].content.starts_with("=== Compact Checkpoint"));
        assert!(msgs[1].content.contains("summary text"));
        assert_eq!(msgs[2].content, "newest");
    }

    #[test]
    fn replace_range_report_accounts_chars() {
        let mut msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(&"a".repeat(1_000)),
            ChatMessage::user(&"b".repeat(1_000)),
            ChatMessage::user("tail"),
        ];
        let report = replace_range(&mut msgs, 1..3, "short summary");
        assert_eq!(report.chars_before, 2_000);
        assert_eq!(report.msgs_compacted, 2);
        assert_eq!(report.chars_after, msgs[1].content.chars().count());
        assert!(report.est_tokens_saved() > 400);
    }

    // ── estimate_tokens ────────────────────────────────────────────────────

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        assert_eq!(estimate_tokens(1_000), 250);
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 0);
        assert_eq!(estimate_tokens(4_000), 1_000);
    }
}
