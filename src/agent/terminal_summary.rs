//! Terminal-summary content hygiene (thread 1596: interrupted threads must end
//! with a PROPER Summary-type last message).
//!
//! When a thread is interrupted (iteration limit reached / cap-hit) or ends
//! without a normal final answer, the engine generates a terminal summary
//! (msg_type=summary). Empirically (deepseek-v4-flash, 2026-09-09) the model
//! sometimes answers that terminal prompt with:
//!   (a) a raw DSML/XML tool-call block (`<tool_calls><invoke ...>`) - its
//!       text-mode way of requesting more tools even though tools are disabled
//!       for the call; threads 1550/1588 persisted exactly this and then
//!       failed with a DSML error at iteration 12;
//!   (b) continuation / plan prose ("I'll update the subtasks...", "I'm in a
//!       retry with fresh budget. Let me verify...") - the raw mid-work tail
//!       persisted as the "summary"; threads 1579/1581/1586 ended this way.
//!
//! Every terminal write point must run its content through
//! [`sanitize_terminal_content`] (strip DSML/XML tool-call envelopes and
//! markdown `tool_call` fences) and then [`is_continuation_intent`]; when the
//! cleaned content is empty or is only continuation intent, the caller falls
//! back to the deterministic digest-based summary from
//! [`deterministic_interrupted_summary`]. The operator then always reads a
//! real summary and never a malformed tool-call tail or a "let me now..."
//! opener.

/// Delimiter of DeepSeek text-mode tool-call markup ("DSML"): instead of XML
/// angle brackets the model writes special tokens with a FULL-WIDTH VERTICAL
/// BAR (U+FF5C), e.g. `<\u{FF5C}DSML\u{FF5C} calls>` or
/// `<\u{FF5C}DSML\u{FF5C} invoke name="...">`. Observed live on
/// deepseek-v4-flash answering the limit-reached summary prompt (tools
/// disabled) in omnidev on 2026-09-10: the entire terminal summary was such a
/// block, and the XML-only sanitizer below did not recognize it.
const DSML_BAR: char = '\u{FF5C}';

/// Token-name separator (U+2581) used inside DeepSeek special tokens
/// (`<\u{FF5C}tool\u{2581}calls\u{2581}begin\u{FF5C}>`).
const DSML_SEP: char = '\u{2581}';

/// True when `text` carries DeepSeek DSML special-token markup: the full-width
/// bar delimiter or the U+2581 token separator. A plain-prose summary contains
/// neither character.
pub(crate) fn contains_dsml_markup(text: &str) -> bool {
    text.contains(DSML_BAR) || text.contains(DSML_SEP)
}

/// Strip DSML/XML tool-call envelopes and markdown `tool_call` fences from a
/// raw terminal message.
///
/// Removes complete `<tool_calls>...</tool_calls>` blocks (dropping the tail
/// when the block is unterminated, i.e. the model was cut off mid-block),
/// markdown-fenced ```tool_call``` blocks, and any leftover lone XML tool-tag
/// lines (`<invoke...>`, `<parameter...>`, their closers, `<antml:*>`).
/// Ordinary prose around the blocks is preserved. Returns the cleaned text
/// (possibly empty when the input was only a tool-call block).
pub(crate) fn sanitize_terminal_content(raw: &str) -> String {
    // Pass 1: remove <tool_calls> ... </tool_calls> envelopes (byte-safe).
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        match rest.find("<tool_calls") {
            Some(start) => {
                out.push_str(&rest[..start]);
                if let Some(rel) = rest[start..].find("</tool_calls>") {
                    rest = &rest[start + rel + "</tool_calls>".len()..];
                } else {
                    // Unterminated block: the model was cut off mid-envelope.
                    // The remainder of `raw` is the malformed tail - drop it.
                    rest = "";
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }

    // Pass 2: drop markdown tool_call fences, lone XML tool-tag lines, and
    // collapse excess blank lines.
    let mut result = String::with_capacity(out.len());
    let mut in_tool_fence = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_tool_fence {
                in_tool_fence = false;
            } else if trimmed.contains("tool_call") || trimmed.contains("dsml") {
                in_tool_fence = true;
            } else {
                result.push_str(line);
                result.push('\n');
            }
            continue;
        }
        if in_tool_fence {
            continue;
        }
        if trimmed.is_empty()
            || trimmed.contains(DSML_BAR)
            || trimmed.contains(DSML_SEP)
            || trimmed.starts_with("<invoke")
            || trimmed.starts_with("</invoke>")
            || trimmed.starts_with("<parameter")
            || trimmed.starts_with("</parameter>")
            || trimmed.starts_with("<tool_calls")
            || trimmed.starts_with("</tool_calls>")
            || trimmed.starts_with("<antml:")
            || trimmed.starts_with("</antml:")
        {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }

    // Pass 3: collapse runs of blank lines, trim.
    let mut collapsed = String::with_capacity(result.len());
    let mut blank_run = 0usize;
    for line in result.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

/// Phrase openers that indicate the model is CONTINUING work rather than
/// summarizing it ("I'll update...", "Let me verify...", "I'm in a retry...").
const CONTINUATION_OPENERS: &[&str] = &[
    "let me ",
    "i'll ",
    "i will ",
    "i'm going to ",
    "i am going to ",
    "i need to ",
    "i must ",
    "i should ",
    "i want to ",
    "i'm in ",
    "i am in ",
    "i can now ",
    "i'd like to ",
    "i would like to ",
    "i have all the evidence",
    "i have the evidence",
    "i have enough",
    "proceeding to ",
    "first batch",
];

/// To-do / continuation verbs that, combined with an opener, mark the text as
/// a plan for further action rather than a report of what happened.
const CONTINUATION_TODO_MARKERS: &[&str] = &[
    " then ",
    " first ",
    " now ",
    " next ",
    " verify ",
    " inspect ",
    " check ",
    " update ",
    " close out ",
    " commit ",
    " push ",
    " run ",
    " proceed ",
    " continue ",
    " finish ",
    " deliver ",
    " gather ",
    " investigate ",
    " do the ",
    " begin ",
    " remaining work",
    " remaining steps",
    " one more ",
    " fresh budget",
    " final answer now",
    " give the final answer",
    " then give ",
];

/// Phrases that mark the text as a self-declared summary/report rather than a
/// continuation plan. When present at the head, the text is accepted even if it
/// also contains an opener like "let me" ("let me summarize what happened",
/// "let me know if you want me to continue").
const SUMMARY_INTROS: &[&str] = &[
    "summary:",
    "to summarize",
    "in summary",
    "summarize",
    "here's a summary",
    "here is a summary",
    "here's what happened",
    "here is what happened",
    "here's what was done",
    "here is what was done",
    "i summarize",
];

/// Heuristic gate for "this text is continuation intent, not a summary".
///
/// Looks at the first ~300 chars (the opening sentences, where a summary
/// states its subject and a continuation states its plan): if the text opens
/// with a plan opener AND contains a to-do marker, it is the model planning
/// further work - not a genuine summary - and callers should fall back to the
/// deterministic digest summary. Texts that explicitly summarize
/// ("let me summarize...", "summary:", ...) do not carry a to-do marker and
/// are accepted.
pub(crate) fn is_continuation_intent(text: &str) -> bool {
    let head: String = text.chars().take(300).collect();
    let lower = head.to_lowercase();
    // Self-declared summaries are always accepted: covers "let me summarize...",
    // "summary: ...", and polite tails like "let me know if you want me to
    // continue" when the body is a genuine report.
    if SUMMARY_INTROS.iter().any(|s| lower.contains(s)) {
        return false;
    }
    let let_me_know = lower.contains("let me know");
    let has_opener = CONTINUATION_OPENERS
        .iter()
        .any(|o| lower.contains(o) && !(let_me_know && *o == "let me "));
    if !has_opener {
        return false;
    }
    CONTINUATION_TODO_MARKERS.iter().any(|m| lower.contains(m))
}

/// Build a deterministic, honest terminal summary from the thread's own data:
/// what was requested (the cause), what tool activity was recorded (digest,
/// newest first), and an explicit "interrupted before completion" caveat.
/// Never claims completion it did not reach.
pub(crate) fn deterministic_interrupted_summary(
    cause_content: &str,
    digest: Option<&str>,
    current_iter: i32,
    iter_limit: i32,
) -> String {
    let requested: String = cause_content
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .take(400)
        .collect();
    let mut s = format!(
        "## Summary (thread interrupted)\n\n\
         The iteration limit ({current_iter}/{iter_limit}) was reached before the task produced a \
         final answer, so this summary is generated from the recorded tool activity.\n\n\
         **Requested:** {requested}\n\n\
         **Done / verified (from tool results, newest first):**\n"
    );
    match digest {
        Some(d) if !d.trim().is_empty() => {
            for line in d.lines() {
                s.push_str("- ");
                s.push_str(line);
                s.push('\n');
            }
        }
        _ => s.push_str("- No tool activity had completed when the thread was interrupted.\n"),
    }
    s.push_str(
        "\n**Remaining / not done:** the task was interrupted before completion; anything not shown \
         as done above was NOT completed or verified. Reply \"continue\" in this thread to resume \
         where it stopped.\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_envelope_stripped_keeps_surrounding_prose() {
        let raw = "Done the work.\n<tool_calls>\n<invoke name=\"git_run-command\">\n<parameter name=\"args\">[\"diff\",\"--stat\"]</parameter>\n</invoke>\n</tool_calls>\nMore text after.";
        let cleaned = sanitize_terminal_content(raw);
        assert!(cleaned.contains("Done the work."));
        assert!(cleaned.contains("More text after."));
        assert!(!cleaned.contains("<tool_calls"));
        assert!(!cleaned.contains("<invoke"));
        assert!(!cleaned.contains("<parameter"));
        assert!(!cleaned.contains("</tool_calls>"));
    }

    #[test]
    fn envelope_only_becomes_empty() {
        let raw = "<tool_calls>\n<invoke name=\"builtin_omniagent-api\">\n<parameter name=\"method\">POST</parameter>\n</invoke>\n</tool_calls>";
        assert!(sanitize_terminal_content(raw).is_empty());
    }

    #[test]
    fn unterminated_envelope_drops_malformed_tail() {
        let raw = "intro text <tool_calls>\n<invoke name=\"x\">\n<parameter name=\"y\">1</param";
        let cleaned = sanitize_terminal_content(raw);
        assert_eq!(cleaned, "intro text");
    }

    #[test]
    fn markdown_tool_call_fence_removed() {
        let raw = "start\n```tool_call\n{\"tool\": \"x\"}\n```\nend";
        let cleaned = sanitize_terminal_content(raw);
        assert!(cleaned.contains("start"));
        assert!(cleaned.contains("end"));
        assert!(!cleaned.contains("tool_call"));
        assert!(!cleaned.contains("{\"tool\": \"x\"}"));
    }

    #[test]
    fn lone_xml_tag_lines_dropped() {
        let raw = "line one\n<invoke name=\"x\">\n<parameter name=\"a\">b</parameter>\nline two";
        let cleaned = sanitize_terminal_content(raw);
        assert!(cleaned.contains("line one"));
        assert!(cleaned.contains("line two"));
        assert!(!cleaned.contains("<invoke"));
        assert!(!cleaned.contains("<parameter"));
    }

    #[test]
    fn real_interrupted_tails_are_continuation_intent() {
        // Thread 1579 last message (opening).
        let t1579 = "I'm in a retry with fresh budget. Let me verify what state my previous commits left things in, then finish the remaining work precisely. First batch: inspect the last commits in both repos, current omni.env content, and grep for the research-path references.";
        assert!(is_continuation_intent(t1579));
        // Thread 1581 last message (opening).
        let t1581 = "I'll update the subtasks with what I've established and deliver the final answer. Evidence gathered: Thread 1579 has DB status interrupted.";
        assert!(is_continuation_intent(t1581));
        // Thread 1586 last message (opening).
        let t1586 = "I have all the evidence I need. Let me close out all subtasks in this single round, then give the final answer. The key finding: an omnidev kanban task already exists.";
        assert!(is_continuation_intent(t1586));
        // Thread 1588/1550 style: XML-only content already sanitized to empty,
        // but prose + a plan after an opener is still flagged.
        assert!(is_continuation_intent("I MUST commit and push before final answer. Let me run git status, then commit and push the 3 repos."));
    }

    #[test]
    fn genuine_summaries_are_not_continuation_intent() {
        let legit = "The iteration limit (12/12) was reached before I could run the final check. I committed the fix (abc123) and pushed to origin/main; the remaining step is the deepseek reproduction, which was not run.";
        assert!(!is_continuation_intent(legit));
        let legit2 = "Summary: inspected threads 1579/1581/1586 and found their last messages are continuation openers persisted as summaries.";
        assert!(!is_continuation_intent(legit2));
        let legit3 = "Let me summarize what happened: the root cause is the raw mid-work tail being persisted; the fix strips tool-call markup and falls back deterministically.";
        assert!(!is_continuation_intent(legit3));
        let legit4 = "I verified the omnidev build is green and the reproduction run ended with a proper summary. Nothing remains.";
        assert!(!is_continuation_intent(legit4));
    }

    #[test]
    fn deterministic_summary_is_honest_and_grounded() {
        let digest = "[tool] git_commit-and-push pushed abc123\n[tool] cargo test 36 passed";
        let s = deterministic_interrupted_summary(
            "Fix the interrupted-thread summary bug",
            Some(digest),
            3,
            5,
        );
        assert!(s.contains("iteration limit (3/5)"));
        assert!(s.contains("Fix the interrupted-thread summary bug"));
        assert!(s.contains("- [tool] git_commit-and-push pushed abc123"));
        assert!(s.contains("- [tool] cargo test 36 passed"));
        assert!(s.contains("was interrupted before completion"));
        assert!(!s.contains("was completed")); // never claims completion
        assert!(s.contains("NOT completed"));
    }

    #[test]
    fn deterministic_summary_without_digest() {
        let s = deterministic_interrupted_summary("Do a thing", None, 1, 3);
        assert!(s.contains("No tool activity had completed"));
        assert!(s.contains("Reply \"continue\""));
    }

    #[test]
    fn deepseek_dsml_envelope_only_becomes_empty() {
        // Real terminal summary persisted by the omnidev repro (deepseek-v4-flash,
        // iteration limit reached, 2026-09-10): the model answered the summary
        // prompt (tools disabled) with its DSML text-tool syntax.
        let raw = "<\u{FF5C}DSML\u{FF5C} calls>\n\
                   <\u{FF5C}DSML\u{FF5C} invoke name=\"subtasks_manage-subtasks\">\n\
                   <\u{FF5C}DSML\u{FF5C} parameter name=\"action\" string=\"true\">update</\u{FF5C}DSML\u{FF5C} parameter>\n\
                   <\u{FF5C}DSML\u{FF5C} parameter name=\"subtask_id\" string=\"false\">1</\u{FF5C}DSML\u{FF5C} parameter>\n\
                   </\u{FF5C}DSML\u{FF5C} invoke>\n\
                   </\u{FF5C}DSML\u{FF5C} calls>";
        assert!(contains_dsml_markup(raw));
        assert!(sanitize_terminal_content(raw).is_empty());
    }

    #[test]
    fn deepseek_dsml_envelope_keeps_surrounding_prose() {
        let raw = "The task hit its iteration limit before the final check.\n\
                   <\u{FF5C}DSML\u{FF5C} invoke name=\"x\">\n\
                   </\u{FF5C}DSML\u{FF5C} invoke>\n\
                   Remaining: the reproduction run was not executed.";
        let cleaned = sanitize_terminal_content(raw);
        assert!(cleaned.contains("hit its iteration limit before the final check"));
        assert!(cleaned.contains("Remaining: the reproduction run was not executed."));
        assert!(!contains_dsml_markup(&cleaned));
    }

    #[test]
    fn dsml_detector_ignores_plain_prose() {
        assert!(!contains_dsml_markup(
            "Plain summary text: committed abc123, pushed to origin/main."
        ));
        assert!(contains_dsml_markup("x\u{FF5C}y"));
        assert!(contains_dsml_markup("x\u{2581}y"));
    }
}

/// Build a deterministic, honest terminal summary for the empty-final path: the
/// agent performed tool work but the thread ended without a final text answer.
/// Never claims completion it did not reach.
pub(crate) fn deterministic_activity_summary(cause_content: &str, digest: Option<&str>) -> String {
    let requested: String = cause_content
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .take(400)
        .collect();
    let mut s = format!(
        "## Summary (no final message)\n\n\
         The agent performed tool activity but the thread ended without a final text answer, \
         so this summary is generated from the recorded tool activity.\n\n\
         **Requested:** {requested}\n\n\
         **Done / verified (from tool results, newest first):**\n"
    );
    match digest {
        Some(d) if !d.trim().is_empty() => {
            for line in d.lines() {
                s.push_str("- ");
                s.push_str(line);
                s.push('\n');
            }
        }
        _ => s.push_str("- No tool activity had completed when the thread ended.\n"),
    }
    s.push_str(
        "\n**Remaining / not done:** the task was not completed; anything not shown \
         as done above was NOT completed or verified.\n",
    );
    s
}

#[cfg(test)]
mod activity_tests {
    use super::*;

    #[test]
    fn polite_tails_and_self_declared_summaries_not_flagged() {
        let polite = "The task hit its iteration limit before the final check ran. If you want \
            me to continue, let me know; otherwise this is what was done and verified.";
        assert!(!is_continuation_intent(polite));
        let will_summarize = "I'll summarize what was done: I committed the fix and pushed to \
            origin/main; the deepseek reproduction was not run yet.";
        assert!(!is_continuation_intent(will_summarize));
        let with_intro = "Summary: the root cause is the raw mid-work tail being persisted; the \
            fix strips tool-call markup and falls back deterministically.";
        assert!(!is_continuation_intent(with_intro));
    }

    #[test]
    fn activity_summary_is_honest_and_grounded() {
        let digest = "[tool] git_commit-and-push pushed abc123\n[tool] cargo test 36 passed";
        let s = deterministic_activity_summary(
            "Run the interrupted-summary reproduction",
            Some(digest),
        );
        assert!(s.contains("no final message"));
        assert!(s.contains("Run the interrupted-summary reproduction"));
        assert!(s.contains("- [tool] git_commit-and-push pushed abc123"));
        assert!(s.contains("was not completed"));
        assert!(!s.contains("iteration limit"));
    }
}
