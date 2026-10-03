//! Live in-prompt token-usage telemetry (settings `token_usage_budget` +
//! `token_usage_telemetry_percent`).
//!
//! Informs the agent, inside its own prompt, how much provider-reported
//! cumulative token usage the thread has spent relative to a configured
//! budget. It is INFORMATIONAL ONLY: it never enforces a limit and never
//! interacts with the compaction budgets (`prompt_token_budget_hard` /
//! `prompt_token_budget_soft`).
//!
//! Cache safety (the critical constraint): the information is delivered as an
//! APPEND-ONLY tail block. Each threshold crossing pushes ONE new frozen
//! USER-role message at the very end of the array; an already written block is
//! never removed and never rewritten. Between crossings the array therefore
//! keeps a byte-identical prefix (the DeepSeek prefix cache rides it), and a
//! crossing only extends the tail - exactly the mechanism of
//! [`super::helpers::upsert_system_message`], minus the remove/replace (which
//! is only legal for blocks whose value changes every iteration).
//!
//! Disabled semantics (operator amendment 2026-09-17): a
//! `token_usage_telemetry_percent` of 0 - or empty/missing, which parses to 0 -
//! DISABLES telemetry completely: no `=== Token Usage ===` block is appended at
//! any time, regardless of the budget or of how much usage accumulates. Only a
//! positive percent enables the feature, and it additionally requires
//! `token_usage_budget > 0` (the budget the percent refers to).

use crate::llm::ChatMessage;

/// Marker prefix of a telemetry block. Matches the marker convention of the
/// other tail blocks (`=== Budget ===`, `=== Working Notes (durable) ===`), so
/// the blocks are identifiable and their count in the array is authoritative.
/// Header of the live token-usage telemetry block. Re-exported from the SINGLE
/// marker source ([`crate::agent::output_markers`]).
pub use crate::agent::output_markers::TOKEN_USAGE_MARKER;

/// Provider-reported cumulative token usage of a thread, aggregated from
/// `messages.token_usage` (SUM prompt_tokens / completion_tokens /
/// cached_tokens). `cached_tokens` is a COMPONENT of the input tokens, never
/// added on top of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

impl TokenUsageTotals {
    /// The billed/cumulative total the budget is compared against:
    /// input (prompt) + output (completion) tokens.
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Telemetry is ENABLED only when a positive threshold granularity is
/// configured AND there is a budget to measure against.
///
/// `percent == 0` covers both an explicit 0 and an empty/missing setting
/// (parsed to 0): telemetry is then disabled and no block is ever appended.
pub fn telemetry_enabled(budget: u64, percent: u32) -> bool {
    budget > 0 && percent > 0
}

/// Number of thresholds already crossed: `floor(cumulative * 100 / budget)`
/// divided by `percent` (integer floor). With percent=10 and a cumulative of
/// 20 % of the budget this returns 2 - the 10 % and the 20 % crossing.
pub fn thresholds_crossed(cumulative: u64, budget: u64, percent: u32) -> u64 {
    if !telemetry_enabled(budget, percent) {
        return 0;
    }
    let percent_reached = cumulative.saturating_mul(100) / budget;
    percent_reached / percent as u64
}

/// Count the telemetry blocks already present in the array. Only
/// system/user-role messages are considered (a tool result or assistant reply
/// could legitimately QUOTE the marker text).
pub fn emitted_block_count(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .filter(|m| {
            matches!(m.role.as_str(), "system" | "user")
                && m.content.starts_with(TOKEN_USAGE_MARKER)
        })
        .count() as u64
}

/// Frozen text of one telemetry block.
fn block_text(threshold_percent: u64, cumulative: &TokenUsageTotals, budget: u64) -> String {
    format!(
        "{TOKEN_USAGE_MARKER}\nthreshold: {threshold_percent}% of {budget}\ncumulative: {total} tokens (input {input} + output {output}; cached {cached})",
        threshold_percent = threshold_percent,
        budget = budget,
        total = cumulative.total(),
        input = cumulative.input_tokens,
        output = cumulative.output_tokens,
        cached = cumulative.cached_tokens,
    )
}

/// Append one frozen `=== Token Usage ===` USER-role block per threshold that
/// has been crossed but has no block yet, and return how many were appended.
///
/// APPEND-ONLY CONTRACT: existing messages are never removed or rewritten; new
/// blocks only ever extend the tail. Calling this repeatedly with the same
/// cumulative usage is a no-op (idempotent) because the number of blocks
/// already present determines how many crossings remain unannounced.
///
/// (After a restart or a compaction that dropped old blocks the missing
/// crossings are re-announced once, using the cumulative usage observed at that
/// moment; the cap is therefore 100/percent blocks per thread.)
pub fn append_usage_blocks(
    messages: &mut Vec<ChatMessage>,
    cumulative: &TokenUsageTotals,
    budget: u64,
    percent: u32,
) -> u64 {
    if !telemetry_enabled(budget, percent) {
        return 0;
    }
    let target = thresholds_crossed(cumulative.total(), budget, percent);
    let emitted = emitted_block_count(messages);
    if target <= emitted {
        return 0;
    }
    let mut appended = 0;
    for k in (emitted + 1)..=target {
        let threshold_percent = k.saturating_mul(percent as u64);
        messages.push(ChatMessage::user(&block_text(
            threshold_percent,
            cumulative,
            budget,
        )));
        appended += 1;
    }
    appended
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;

    fn totals(input: u64, output: u64, cached: u64) -> TokenUsageTotals {
        TokenUsageTotals {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        }
    }

    /// T6: threshold logic - cumulative 0->9 %, 10 %, 19 %, 20 % of the budget
    /// with percent=10 yields exactly 2 blocks (at 10 % and 20 %); percent=5
    /// yields 4; budget=0 yields none.
    #[test]
    fn t6_threshold_crossings_append_exactly_one_block_per_threshold() {
        let budget = 1_000_000u64;
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system("sys")];

        let steps: [(u64, u64); 4] = [
            (90_000, 0),  // 9 %
            (100_000, 1), // 10 % -> crossing 1
            (190_000, 0), // 19 %
            (200_000, 1), // 20 % -> crossing 2
        ];
        let mut appended_total = 0;
        for (cumulative, expected_new) in steps {
            let appended =
                append_usage_blocks(&mut messages, &totals(cumulative, 0, 0), budget, 10);
            assert_eq!(appended, expected_new, "cumulative {cumulative}");
            appended_total += appended;
        }
        assert_eq!(appended_total, 2, "percent=10 -> exactly 2 blocks");
        assert_eq!(emitted_block_count(&messages), 2);
        assert!(messages[1].content.contains("threshold: 10% of 1000000"));
        assert!(messages[2].content.contains("threshold: 20% of 1000000"));
        assert!(messages[1].content.starts_with(TOKEN_USAGE_MARKER));
        assert_eq!(messages[1].role, "user", "block is USER-role (cache-safe)");

        // percent=5 -> 4 blocks (5, 10, 15, 20 %).
        let mut messages5: Vec<ChatMessage> = vec![ChatMessage::system("sys")];
        let mut appended5 = 0;
        for (cumulative, _) in steps {
            appended5 += append_usage_blocks(&mut messages5, &totals(cumulative, 0, 0), budget, 5);
        }
        assert_eq!(appended5, 4, "percent=5 -> 4 blocks");

        // budget=0 -> disabled, no blocks.
        let mut m0: Vec<ChatMessage> = vec![ChatMessage::system("sys")];
        assert_eq!(
            append_usage_blocks(&mut m0, &totals(900_000, 0, 0), 0, 10),
            0
        );
        assert_eq!(emitted_block_count(&m0), 0);
    }

    /// Disabled gate: percent=0 (explicit, or empty/missing -> 0) never
    /// appends a block, even with a budget and usage far above the thresholds.
    #[test]
    fn percent_zero_or_empty_disables_telemetry_completely() {
        let budget = 1_000u64;
        assert!(!telemetry_enabled(budget, 0));
        assert_eq!(thresholds_crossed(500_000, budget, 0), 0);
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system("sys")];
        for _ in 0..5 {
            assert_eq!(
                append_usage_blocks(&mut messages, &totals(900_000, 100_000, 0), budget, 0),
                0
            );
        }
        assert_eq!(emitted_block_count(&messages), 0, "no block at any time");
        assert!(
            !messages.iter().any(|m| m.content.contains("Token Usage")),
            "no telemetry text anywhere in the array"
        );
        // Empty/missing setting path: the config parser maps it to 0.
        let empty_percent: u32 = "".trim().parse().unwrap_or(0);
        assert_eq!(empty_percent, 0);
        assert!(!telemetry_enabled(budget, empty_percent));
    }

    /// T7: cache property - with no crossing the array is byte-identical;
    /// after one crossing only the tail changed (prefix identical).
    #[test]
    fn t7_no_crossing_keeps_array_byte_identical_and_crossing_only_extends_tail() {
        let budget = 1_000u64;
        let mut messages: Vec<ChatMessage> = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi"),
        ];
        let before = serde_json::to_string(&messages).unwrap();

        // Same accumulated usage, twice: nothing changes at all.
        let cumulative = totals(50, 49, 10); // 9 %
        assert_eq!(
            append_usage_blocks(&mut messages, &cumulative, budget, 10),
            0
        );
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
        assert_eq!(
            append_usage_blocks(&mut messages, &cumulative, budget, 10),
            0
        );
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);

        // A crossing appends exactly one message at the tail: the prefix
        // (every pre-existing message, byte-identical) is untouched.
        let crossing = totals(100, 0, 40); // 10 %
        assert_eq!(append_usage_blocks(&mut messages, &crossing, budget, 10), 1);
        let after: Vec<ChatMessage> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        assert_eq!(after.len(), 4, "exactly one message appended");
        let before_msgs = before_messages(&before);
        assert_eq!(before_msgs.len(), 3);
        for (i, old) in before_msgs.iter().enumerate() {
            assert_eq!(
                serde_json::to_string(old).unwrap(),
                serde_json::to_string(&after[i]).unwrap(),
                "pre-existing message {i} must be byte-identical"
            );
        }
        assert!(after[3].content.starts_with(TOKEN_USAGE_MARKER));

        // Once announced, the same cumulative usage appends nothing more.
        let snapshot = serde_json::to_string(&messages).unwrap();
        assert_eq!(append_usage_blocks(&mut messages, &crossing, budget, 10), 0);
        assert_eq!(serde_json::to_string(&messages).unwrap(), snapshot);
    }

    fn before_messages(serialized: &str) -> Vec<ChatMessage> {
        serde_json::from_str(serialized).unwrap()
    }

    /// The block is unique per threshold and carries the plan's format.
    #[test]
    fn block_format_matches_the_plan() {
        let text = block_text(20, &totals(198_120, 16_410, 141_002), 1_000_000);
        assert_eq!(
            text,
            "=== Token Usage ===\nthreshold: 20% of 1000000\ncumulative: 214530 tokens (input 198120 + output 16410; cached 141002)"
        );
    }

    /// A message that merely QUOTES the marker (tool result / assistant) is not
    /// counted as an emitted block.
    #[test]
    fn quoted_marker_in_non_user_roles_is_not_counted() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::tool_result("id", "read", "=== Token Usage ===\nstale"),
            ChatMessage::assistant("=== Token Usage ===\nquoted"),
        ];
        assert_eq!(emitted_block_count(&messages), 0);
    }
}
