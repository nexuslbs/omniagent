//! Deterministic tool-result pruning (head/middle/tail) - task 2.
//!
//! Complements the tool-result spill (task 1): spill preserves FULL fidelity
//! on disk (preview + `[full output: <path>]` locator in the message); the
//! pruner replaces over-budget tool-result content in the in-memory
//! conversation with a bounded head/middle/tail preview BEFORE the content is
//! sent to the LLM (summary generation, context assembly, provider retries).
//!
//! Contract (from the task decision - do NOT re-litigate):
//! - only `role == "tool"` message CONTENT is replaced; tool-CALL messages
//!   (`role == "assistant"` carrying `tool_calls`) are never touched, so the
//!   tool-call/result pairing stays intact (`tool_call_id`/`name` preserved),
//! - the spill locator line (`[full output: <path>]`) from task 1, when
//!   present at the end of the content, is preserved verbatim so the model
//!   can still read the full output via `filesystem_read`,
//! - results at or under `min_chars` (Unicode code points) are never touched,
//! - already-pruned contents (spill or prune previews) are never re-pruned
//!   (idempotent - no compounding across iterations),
//! - every replacement is accounted (chars before → after) and logged.
//!
//! The pruner is pure slicing: zero LLM calls, deterministic output, and
//! Unicode-safe (`str::chars()` iterates complete scalar values, so slicing
//! on char boundaries never splits a surrogate pair / combining sequence).

use crate::agent::config::AgentConfig;
use crate::llm::ChatMessage;

/// Default head chars kept in a pruned preview (settings `prune_head_chars`).
pub const DEFAULT_PRUNE_HEAD_CHARS: usize = 12_000;
/// Default tail chars kept in a pruned preview (settings `prune_tail_chars`).
pub const DEFAULT_PRUNE_TAIL_CHARS: usize = 8_000;
/// Default trigger: only results over this many chars are pruned
/// (settings `prune_min_chars`). Defaults to the head+tail budget so any
/// result larger than the preview budget is shrunk down to it.
pub const DEFAULT_PRUNE_MIN_CHARS: usize = 20_000;

/// Substring shared by the task-1 spill preview and this pruner's preview.
/// Presence of this marker means the content is ALREADY a bounded preview
/// with a locator - never re-prune it (idempotency).
const ALREADY_PRUNED_MARKER: &str = "omitted - see full output below";

/// Prune thresholds (from settings, with defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneParams {
    /// Max chars kept from the head of an over-budget result.
    pub head_chars: usize,
    /// Max chars kept from the tail of an over-budget result.
    pub tail_chars: usize,
    /// Results with at most this many chars (Unicode code points) are left
    /// untouched.
    pub min_chars: usize,
}

impl PruneParams {
    /// Default thresholds: head 12K + tail 8K, trigger 20K.
    pub fn defaults() -> Self {
        Self {
            head_chars: DEFAULT_PRUNE_HEAD_CHARS,
            tail_chars: DEFAULT_PRUNE_TAIL_CHARS,
            min_chars: DEFAULT_PRUNE_MIN_CHARS,
        }
    }

    /// From an agent config snapshot (settings `prune_head_chars`,
    /// `prune_tail_chars`, `prune_min_chars`).
    pub fn from_config(cfg: &AgentConfig) -> Self {
        Self {
            head_chars: cfg.prune_head_chars,
            tail_chars: cfg.prune_tail_chars,
            min_chars: cfg.prune_min_chars,
        }
    }
}

/// Accounting record for one pruned tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneEntry {
    /// The tool_call_id of the tool-result message (pairing key; preserved).
    pub tool_call_id: String,
    /// The tool name (ChatMessage.name; preserved).
    pub tool_name: String,
    /// Chars in the original content (Unicode code points).
    pub chars_before: usize,
    /// Chars in the pruned preview.
    pub chars_after: usize,
}

/// Aggregate report of one prune pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub entries: Vec<PruneEntry>,
    pub chars_before: usize,
    pub chars_after: usize,
}

impl PruneReport {
    /// True when no message was pruned in this pass.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total chars removed by this pass (before − after).
    pub fn chars_saved(&self) -> usize {
        self.chars_before.saturating_sub(self.chars_after)
    }
}

/// If `content` ends with a spill locator line (`[full output: <path>]`,
/// task 1 - nothing but whitespace after the closing bracket), return
/// `(content-without-locator, locator)` so the locator can be re-appended
/// after the preview. Otherwise return `(content, None)`.
fn split_spill_locator(content: &str) -> (&str, Option<&str>) {
    const PREFIX: &str = "[full output: ";
    let Some(start) = content.rfind(PREFIX) else {
        return (content, None);
    };
    let after = &content[start + PREFIX.len()..];
    let Some(close) = after.find(']') else {
        return (content, None);
    };
    let path = &after[..close];
    if path.trim().is_empty() || path.contains('\n') {
        return (content, None);
    }
    if !after[close + 1..].trim().is_empty() {
        return (content, None);
    }
    (&content[..start], Some(&content[start..]))
}

/// Deterministically prune an over-budget tool result into a bounded
/// head/middle/tail preview. Returns `None` when nothing should change:
/// - content at or under `params.min_chars` chars (under threshold),
/// - content already containing the omission marker (already a preview),
/// - head+tail would already cover the whole content (nothing to omit),
/// - empty content.
///
/// The head keeps the first `head_chars` chars, the tail the last
/// `tail_chars` chars, joined by an explicit omitted-chars marker. A trailing
/// spill locator (task 1) is preserved verbatim at the end. Slicing is done
/// on char boundaries, so multi-byte Unicode is never split.
pub fn prune_tool_result_content(content: &str, params: &PruneParams) -> Option<String> {
    // Never re-prune an existing spill/prune preview (idempotency).
    if content.contains(ALREADY_PRUNED_MARKER) {
        return None;
    }
    let total = content.chars().count();
    if total <= params.min_chars {
        return None;
    }
    // Preserve a trailing spill locator (`[full output: <path>]`) if present.
    let (body, locator) = split_spill_locator(content);
    let body_chars: Vec<char> = body.chars().collect();
    let body_total = body_chars.len();
    if body_total == 0 {
        return None;
    }
    let head_chars = params.head_chars.min(body_total);
    let tail_chars = params.tail_chars.min(body_total.saturating_sub(head_chars));
    if head_chars + tail_chars >= body_total {
        // Head+tail already cover everything: leave it untouched rather than
        // duplicating it.
        return None;
    }
    let head: String = body_chars[..head_chars].iter().collect();
    let tail: String = body_chars[body_total - tail_chars..].iter().collect();
    let omitted = body_total - head_chars - tail_chars;
    let mut preview =
        format!("{head}\n\n[… {omitted} chars omitted - see full output below …]\n\n{tail}");
    if let Some(loc) = locator {
        preview.push_str("\n\n");
        preview.push_str(loc);
    }
    Some(preview)
}

/// Prune over-budget tool results in an in-memory conversation, in place.
/// Only `role == "tool"` message contents are replaced; assistant tool-call
/// messages (with `tool_calls`) and all other messages are never touched, so
/// the tool-call/result pairing stays intact. Returns the accounting report.
pub fn prune_messages(messages: &mut [ChatMessage], params: &PruneParams) -> PruneReport {
    let mut report = PruneReport::default();
    for m in messages.iter_mut() {
        if m.role != "tool" {
            continue; // never prune the tool-call message or anything else
        }
        if let Some(pruned) = prune_tool_result_content(&m.content, params) {
            let before = m.content.chars().count();
            let after = pruned.chars().count();
            report.entries.push(PruneEntry {
                tool_call_id: m.tool_call_id.clone().unwrap_or_default(),
                tool_name: m.name.clone().unwrap_or_default(),
                chars_before: before,
                chars_after: after,
            });
            report.chars_before += before;
            report.chars_after += after;
            m.content = pruned;
        }
    }
    report
}

/// Clone the conversation, prune over-budget tool results in the clone, and
/// return (pruned_messages, report). For callers that hold `&[ChatMessage]`
/// (e.g. the summary path) and must not mutate the original.
pub fn prune_messages_owned(
    messages: &[ChatMessage],
    params: &PruneParams,
) -> (Vec<ChatMessage>, PruneReport) {
    let mut cloned = messages.to_vec();
    let report = prune_messages(&mut cloned, params);
    (cloned, report)
}

/// Detect provider "context too long" errors (task-3 hook): when an LLM
/// provider rejects the request because the context exceeds its window, the
/// caller prunes over-budget tool results so the retry's context fits.
/// Pure string classification (lowercased substring match) - no LLM cost.
pub fn is_context_length_error(provider_error: &str) -> bool {
    let msg = provider_error.to_lowercase();
    const MARKERS: &[&str] = &[
        "context length",
        "context_length",
        "context_length_exceeded",
        "maximum context",
        "max context",
        "context window",
        "too many tokens",
        "prompt is too long",
        "prompt too long",
        "input is too long",
        "reduce the length of the messages",
        "exceeds the maximum context",
        "maximum content length",
    ];
    MARKERS.iter().any(|m| msg.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> PruneParams {
        PruneParams {
            head_chars: 12_000,
            tail_chars: 8_000,
            min_chars: 20_000,
        }
    }

    // ── threshold ──────────────────────────────────────────────────────────

    #[test]
    fn under_threshold_untouched() {
        let content = "a".repeat(19_999);
        assert_eq!(prune_tool_result_content(&content, &params()), None);
        // Exactly at the threshold → untouched.
        let at = "b".repeat(20_000);
        assert_eq!(prune_tool_result_content(&at, &params()), None);
    }

    #[test]
    fn over_budget_produces_head_middle_tail() {
        let content = "line {}\n".repeat(10_000); // ≈ 90K chars
        let pruned = prune_tool_result_content(&content, &params()).expect("pruned");
        // Head present: the first 100 chars of the original survive verbatim.
        assert!(pruned.starts_with(&content[..100]));
        // Tail present: the last 100 chars of the original survive verbatim.
        assert!(pruned.ends_with(&content[content.len() - 100..]));
        // Explicit middle marker with the omitted count.
        assert!(pruned.contains("chars omitted - see full output below"));
        // Bounded: head + tail + marker overhead.
        assert!(pruned.chars().count() <= 20_000 + 200);
        // Deterministic: same input, same output.
        assert_eq!(
            prune_tool_result_content(&content, &params()).unwrap(),
            pruned
        );
    }

    #[test]
    fn head_tail_cover_everything_untouched() {
        // When min_chars is below the head+tail budget, a small over-budget
        // result is fully covered by head+tail: leave it untouched rather than
        // duplicating it.
        let small_params = PruneParams {
            head_chars: 12_000,
            tail_chars: 8_000,
            min_chars: 5_000,
        };
        let content = "x".repeat(10_000);
        assert_eq!(prune_tool_result_content(&content, &small_params), None);
        // A genuinely large result is still pruned under the same params.
        let big = "y".repeat(40_000);
        let pruned = prune_tool_result_content(&big, &small_params).expect("pruned");
        assert!(pruned.contains("chars omitted"));
    }

    // ── locator preservation (task 1) ──────────────────────────────────────

    #[test]
    fn spill_locator_preserved() {
        let body = "y".repeat(50_000);
        let content =
            format!("{body}\n\n[full output: /opt/omni/data/spill/7/call_1-filesystem_read.txt]");
        let pruned = prune_tool_result_content(&content, &params()).expect("pruned");
        assert!(
            pruned.ends_with("[full output: /opt/omni/data/spill/7/call_1-filesystem_read.txt]"),
            "locator must survive pruning: {}",
            pruned
        );
        assert!(pruned.contains("chars omitted"));
        assert!(pruned.chars().count() < content.chars().count());
    }

    #[test]
    fn fake_locator_not_mistaken() {
        // A path-looking line in the MIDDLE must not be treated as a locator
        // (only a trailing, self-contained `[full output: …]` line counts).
        let content = format!(
            "see [full output: /a/b.txt] mid-line\n{}",
            "z".repeat(30_000)
        );
        let pruned = prune_tool_result_content(&content, &params()).expect("pruned");
        assert!(pruned.contains("chars omitted"));
        // Head still starts with the original head.
        assert!(pruned.starts_with(&content[..60]));
    }

    // ── Unicode safety ─────────────────────────────────────────────────────

    #[test]
    fn unicode_boundaries_safe() {
        // Multi-byte scalars incl. emoji (4-byte): slicing must never split.
        let content = "héllo wörld - 日本語テスト 🚀🔥\n".repeat(4_000);
        let pruned = prune_tool_result_content(&content, &params()).expect("pruned");
        // Output must be valid UTF-8 (it is by construction of String, but the
        // real check is that char-boundary slicing never panicked).
        assert!(String::from_utf8(pruned.clone().into_bytes()).is_ok());
        // Head/tail match the original on full-char boundaries: compare the
        // first and last 50 chars of each.
        let orig: Vec<char> = content.chars().collect();
        let prev: Vec<char> = pruned.chars().collect();
        assert_eq!(
            prev[..50].iter().collect::<String>(),
            orig[..50].iter().collect::<String>()
        );
        let tail_orig: String = orig[orig.len() - 50..].iter().collect();
        assert!(pruned.ends_with(&tail_orig));
        assert!(pruned.contains("chars omitted"));
    }

    #[test]
    fn unicode_under_threshold_untouched() {
        let content = "日本語🚀".repeat(100); // 500 chars
        assert_eq!(prune_tool_result_content(&content, &params()), None);
    }

    // ── idempotency ────────────────────────────────────────────────────────

    #[test]
    fn already_pruned_never_repruned() {
        let content = "a".repeat(40_000);
        let first = prune_tool_result_content(&content, &params()).expect("first prune");
        // Second pass must be a no-op: the preview already has the marker.
        assert_eq!(prune_tool_result_content(&first, &params()), None);
        // A task-1 spill preview (different marker style) is also skipped.
        let spill_preview = format!(
            "{}\n\n[... 999 chars omitted - see full output below ...]\n\n{}\n\n[full output: /tmp/spill/1/call.txt]",
            "h".repeat(300),
            "t".repeat(200),
        );
        assert_eq!(prune_tool_result_content(&spill_preview, &params()), None);
    }

    // ── pairing intact ─────────────────────────────────────────────────────

    #[test]
    fn pairing_never_broken() {
        let mut msgs = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("let me look"),
            {
                let mut am = ChatMessage::assistant("");
                am.tool_calls = Some(vec![crate::llm::ToolCallData {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: crate::llm::ToolCallFunction {
                        name: "filesystem_read".to_string(),
                        arguments: r#"{"path": "/x"}"#.to_string(),
                    },
                }]);
                am
            },
            ChatMessage::tool_result("call_1", "filesystem_read", &"r".repeat(30_000)),
            ChatMessage::system("sys"),
        ];
        let report = prune_messages(&mut msgs, &params());
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].tool_call_id, "call_1");
        assert_eq!(report.entries[0].tool_name, "filesystem_read");
        // The tool CALL message is untouched: pairing intact.
        let call = msgs.iter().find(|m| m.tool_calls.is_some()).unwrap();
        assert_eq!(call.role, "assistant");
        assert!(call.tool_calls.is_some());
        assert!(call.content.is_empty());
        // The tool RESULT message keeps its pairing keys, only content shrinks.
        let result = msgs.iter().find(|m| m.role == "tool").unwrap();
        assert_eq!(result.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(result.name.as_deref(), Some("filesystem_read"));
        assert!(result.content.contains("chars omitted"));
        // User/system messages untouched.
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[4].content, "sys");
    }

    #[test]
    fn non_tool_messages_never_pruned() {
        let big = "s".repeat(50_000);
        let mut msgs = vec![
            ChatMessage::user(&big),
            ChatMessage::system(&big),
            ChatMessage::assistant(&big),
        ];
        let report = prune_messages(&mut msgs, &params());
        assert!(report.is_empty());
        assert_eq!(
            msgs.iter()
                .map(|m| m.content.chars().count())
                .sum::<usize>(),
            big.chars().count() * 3
        );
    }

    // ── accounting ─────────────────────────────────────────────────────────

    #[test]
    fn report_accounts_chars_before_after() {
        let big = "q".repeat(50_000);
        let mut msgs = vec![
            ChatMessage::tool_result("call_a", "tool_a", &big),
            ChatMessage::tool_result("call_b", "tool_b", "small"),
        ];
        let report = prune_messages(&mut msgs, &params());
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.chars_before, 50_000);
        let after = msgs[0].content.chars().count();
        assert_eq!(report.chars_after, after);
        assert_eq!(report.chars_saved(), 50_000 - after);
        assert!(report.chars_saved() > 20_000);
        // Under-threshold message untouched.
        assert_eq!(msgs[1].content, "small");
    }

    #[test]
    fn owned_variant_does_not_mutate_source() {
        let big = "w".repeat(50_000);
        let msgs = vec![ChatMessage::tool_result("call_1", "t", &big)];
        let (pruned, report) = prune_messages_owned(&msgs, &params());
        assert_eq!(report.entries.len(), 1);
        assert_eq!(msgs[0].content, big, "source slice unchanged");
        assert!(pruned[0].content.contains("chars omitted"));
    }

    // ── context-length classification ──────────────────────────────────────

    #[test]
    fn context_length_markers_detected() {
        assert!(is_context_length_error(
            "This model's maximum context length is 128000 tokens."
        ));
        assert!(is_context_length_error(
            "Error code: 400 - {'error': {'message': 'This model's maximum context length is 16385 tokens. However, you requested 20000 tokens'}}"
        ));
        assert!(is_context_length_error(
            "context_length_exceeded: prompt is too long"
        ));
        assert!(is_context_length_error(
            "The prompt was too long. Reduce the length of the messages."
        ));
        assert!(is_context_length_error("INPUT_TOO_LONG: input is too long"));
    }

    #[test]
    fn unrelated_errors_not_classified() {
        assert!(!is_context_length_error("connection reset by peer"));
        assert!(!is_context_length_error("HTTP 429 rate limited"));
        assert!(!is_context_length_error("invalid api key"));
        assert!(!is_context_length_error(""));
    }
}
