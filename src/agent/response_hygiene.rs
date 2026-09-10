//! Provider-neutral terminal-response hygiene.
//!
//! Historical context (threads 1550/1588/1579/1581/1586/1596): when a thread is
//! interrupted (iteration limit reached) or ends without a normal final answer,
//! the engine asks the LLM for a terminal summary. Some providers answer that
//! prompt with a MALFORMED assistant message - their own text-mode tool-call
//! markup (DeepSeek "DSML": `<|DSML| calls>` built from U+FF5C / U+2581 special
//! tokens, XML `<tool_calls>` envelopes, markdown ```tool_call``` fences) or
//! with continuation prose ("I'll update the subtasks..."). Persisting that as
//! the thread's last message is wrong: the terminal message must be a genuine
//! summary.
//!
//! The SHAPE of "malformed" is provider-specific and must not live in the core.
//! This module therefore delegates the decision to an MCP tool named by the
//! global setting `malformed_response_tool` (empty default), following the
//! existing `redaction_tool` precedent:
//!
//! * setting EMPTY -> the built-in provider-neutral heuristic
//!   ([`sanitize_terminal_content`] + [`is_continuation_intent`]) is used,
//!   byte-for-byte as before this change (back-compat default);
//! * setting SET -> the tool is called with `{"text": <raw assistant message>}`
//!   and must answer with a JSON object
//!   `{"malformed": bool, "reason": str, "cleaned": str, "continuation_intent": bool}`;
//!   `malformed` is the PRIMARY verdict (the operator: "the core calls the tool
//!   mainly to detect that wrongly formatted response"), `cleaned` (when
//!   non-empty) is the sanitized text, `continuation_intent` gates the digest
//!   fallback;
//! * tool error / timeout / invalid JSON -> a warning is logged and the
//!   built-in heuristic is used; the thread is NEVER failed because of the tool
//!   (same fail-open semantics as `redaction_tool`).
//!
//! The reference implementation of the detector is the omni-plugins python
//! plugin `tools/llm-response-hygiene` (tool `llm-response-hygiene_classify`),
//! which ports the DSML/XML/markdown detection and the continuation heuristic.
//! A tool verdict must reproduce the built-in decision on the same input
//! (reference equivalence, covered by tests on both sides).

use serde_json::Value;

use crate::agent::config;
use crate::agent::terminal_summary::{
    contains_dsml_markup, is_continuation_intent, sanitize_terminal_content,
};
use crate::mcp::AppContext;

/// Verdict returned by the configured `malformed_response_tool`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolVerdict {
    /// PRIMARY verdict: the raw message is a provider-specific malformed
    /// assistant message (text-mode tool-call markup or the like).
    pub malformed: bool,
    /// Short machine-readable reason code (diagnostics / logs).
    pub reason: String,
    /// Sanitized text with the malformed markup removed. `None` or whitespace
    /// only means "nothing coherent remains".
    pub cleaned: Option<String>,
    /// Optional extra signal: the text is continuation/plan prose rather than a
    /// summary (same notion as the built-in [`is_continuation_intent`]).
    pub continuation_intent: Option<bool>,
}

impl ToolVerdict {
    /// Parse the tool's JSON result.
    ///
    /// A payload WITHOUT the `malformed` boolean is not a verdict: it is
    /// reported as an error so the caller falls back to the built-in heuristic
    /// (a tool that cannot answer must never change behaviour).
    pub(crate) fn from_json(raw: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw.trim())
            .map_err(|e| format!("malformed-response tool returned invalid JSON: {}", e))?;
        let malformed = value
            .get("malformed")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                "malformed-response tool JSON is missing the boolean `malformed` field".to_string()
            })?;
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let cleaned = value
            .get("cleaned")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .map(str::to_string);
        let continuation_intent = value.get("continuation_intent").and_then(Value::as_bool);
        Ok(Self {
            malformed,
            reason,
            cleaned,
            continuation_intent,
        })
    }
}

/// Outcome of the hygiene decision for one raw assistant message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hygiene {
    /// Text the caller should persist (already cleaned).
    pub cleaned: String,
    /// True: the caller must ignore `cleaned` and use its deterministic
    /// digest-based summary instead.
    pub fallback: bool,
    /// Malformed-message verdict (from the tool, or from the built-in detector).
    pub malformed: bool,
    /// True when the configured MCP tool produced the verdict.
    pub via_tool: bool,
}

/// Built-in provider-neutral heuristic: exactly the behaviour the core had
/// before `malformed_response_tool` existed (the back-compat default).
pub(crate) fn builtin_hygiene(raw: &str, continuation_gate: bool) -> Hygiene {
    let cleaned = sanitize_terminal_content(raw);
    let fallback =
        cleaned.trim().is_empty() || (continuation_gate && is_continuation_intent(&cleaned));
    Hygiene {
        cleaned,
        fallback,
        malformed: contains_dsml_markup(raw),
        via_tool: false,
    }
}

/// Apply a tool verdict to a raw message.
///
/// * the tool's `cleaned` is authoritative when non-empty; when the tool
///   returns no usable text, the built-in cleaner is applied as a best effort so
///   residual markup can never be persisted;
/// * `continuation_intent` falls back to the built-in heuristic when absent; the
///   gate itself is only consulted for the summary paths
///   (`continuation_gate = true`), which keeps the normal-final path exactly as
///   it was before this change.
pub(crate) fn hygiene_from_verdict(
    raw: &str,
    verdict: &ToolVerdict,
    continuation_gate: bool,
) -> Hygiene {
    let cleaned = match verdict.cleaned.as_deref() {
        Some(c) => c.to_string(),
        None => sanitize_terminal_content(raw),
    };
    let continuation = verdict.continuation_intent.unwrap_or_else(|| {
        if continuation_gate {
            is_continuation_intent(&cleaned)
        } else {
            false
        }
    });
    Hygiene {
        fallback: cleaned.trim().is_empty() || (continuation_gate && continuation),
        malformed: verdict.malformed,
        cleaned,
        via_tool: true,
    }
}

/// Decide the terminal-content hygiene for `raw`.
///
/// `continuation_gate`: the summary paths (interrupted / empty-final) also
/// reject continuation prose through the digest fallback; the normal-final path
/// only rejects content that is empty after cleaning (pre-change behaviour).
///
/// Never fails: when the configured tool is unreachable or answers garbage, the
/// built-in heuristic takes over after a warning (fail-open, same contract as
/// `redaction_tool`).
pub(crate) async fn assess(ctx: &AppContext, raw: &str, continuation_gate: bool) -> Hygiene {
    let tool = config::get_global()
        .map(|g| g.read().malformed_response_tool.trim().to_string())
        .unwrap_or_default();
    if tool.is_empty() {
        return builtin_hygiene(raw, continuation_gate);
    }
    match call_malformed_response_tool(ctx, &tool, raw).await {
        Ok(verdict) => {
            let hygiene = hygiene_from_verdict(raw, &verdict, continuation_gate);
            tracing::info!(
                "[hygiene] malformed_response_tool '{}': malformed={} reason='{}' fallback={}",
                tool,
                hygiene.malformed,
                verdict.reason,
                hygiene.fallback
            );
            hygiene
        }
        Err(e) => {
            tracing::warn!(
                "malformed-response tool '{}' failed ({}); using the built-in terminal hygiene heuristic",
                tool,
                e
            );
            builtin_hygiene(raw, continuation_gate)
        }
    }
}

/// Call `{server}_{tool}` with `{"text": text}` and parse its verdict.
async fn call_malformed_response_tool(
    ctx: &AppContext,
    qualified_tool: &str,
    text: &str,
) -> Result<ToolVerdict, String> {
    let (server, tool) = qualified_tool.split_once('_').ok_or_else(|| {
        format!(
            "malformed-response tool '{}' must be of the form '{{server}}_{{tool}}'",
            qualified_tool
        )
    })?;
    let args = serde_json::json!({ "text": text });
    let result = ctx
        .external_clients
        .call_tool(server, tool, &args, None)
        .await
        .map_err(|e| e.to_string())?;
    if result.is_error {
        return Err(format!(
            "malformed-response tool '{}' returned an error: {}",
            qualified_tool, result.content
        ));
    }
    ToolVerdict::from_json(&result.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real DSML terminal summary persisted by the omnidev repro
    /// (deepseek-v4-flash, iteration limit reached, 2026-09-10) - the same
    /// fixture as `terminal_summary::tests::deepseek_dsml_envelope_only_becomes_empty`.
    const DSML_ENVELOPE: &str = "<\u{FF5C}DSML\u{FF5C} calls>\n\
         <\u{FF5C}DSML\u{FF5C} invoke name=\"subtasks_manage-subtasks\">\n\
         <\u{FF5C}DSML\u{FF5C} parameter name=\"action\" string=\"true\">update</\u{FF5C}DSML\u{FF5C} parameter>\n\
         </\u{FF5C}DSML\u{FF5C} invoke>\n\
         </\u{FF5C}DSML\u{FF5C} calls>";

    const DSML_WITH_PROSE: &str = "The task hit its iteration limit before the final check.\n\
         <\u{FF5C}DSML\u{FF5C} invoke name=\"x\">\n\
         </\u{FF5C}DSML\u{FF5C} invoke>\n\
         Remaining: the reproduction run was not executed.";

    const MARKDOWN_FENCE: &str = "start\n```tool_call\n{\"tool\": \"x\"}\n```\nend";

    const XML_ENVELOPE: &str = "<tool_calls>\n<invoke name=\"git_run-command\">\n<parameter name=\"args\">[\"diff\"]</parameter>\n</invoke>\n</tool_calls>";

    const CONTINUATION: &str = "I'll update the subtasks with what I've established and deliver the final answer. First batch: inspect the last commits.";

    const PLAIN_SUMMARY: &str =
        "Committed abc123 and pushed to origin/main; the reproduction run was not executed.";

    fn verdict(malformed: bool, cleaned: &str, continuation: bool) -> ToolVerdict {
        ToolVerdict {
            malformed,
            reason: "test".to_string(),
            cleaned: if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.to_string())
            },
            continuation_intent: Some(continuation),
        }
    }

    #[test]
    fn empty_setting_keeps_the_builtin_behaviour() {
        // Envelope only -> empty -> digest fallback.
        let h = builtin_hygiene(DSML_ENVELOPE, true);
        assert!(h.fallback);
        assert!(h.cleaned.is_empty());
        assert!(h.malformed);
        assert!(!h.via_tool);

        // Envelope + prose -> prose kept, no fallback.
        let h = builtin_hygiene(DSML_WITH_PROSE, true);
        assert!(!h.fallback);
        assert!(h.cleaned.contains("hit its iteration limit"));
        assert!(h.malformed);

        // Continuation prose -> digest fallback on the summary paths.
        assert!(builtin_hygiene(CONTINUATION, true).fallback);
        // ... but accepted on the normal-final path (pre-change behaviour).
        assert!(!builtin_hygiene(CONTINUATION, false).fallback);

        // Plain summary -> accepted.
        let h = builtin_hygiene(PLAIN_SUMMARY, true);
        assert!(!h.fallback);
        assert_eq!(h.cleaned, PLAIN_SUMMARY);
        assert!(!h.malformed);
    }

    #[test]
    fn tool_verdict_reproduces_the_builtin_decision() {
        // Reference equivalence, diffed not eyeballed: the JSON below is the
        // ACTUAL output of the omni-plugins `llm-response-hygiene` `classify`
        // tool for each fixture (captured from its own selftest corpus, which
        // asserts the same expectations). Applying the tool verdict must yield
        // the same decision as the built-in provider-neutral heuristic.
        let corpus: [(&str, &str); 6] = [
            (
                DSML_ENVELOPE,
                r#"{"malformed": true, "reason": "dsml_markup", "cleaned": "", "continuation_intent": false}"#,
            ),
            (
                DSML_WITH_PROSE,
                r#"{"malformed": true, "reason": "dsml_markup", "cleaned": "The task hit its iteration limit before the final check.\nRemaining: the reproduction run was not executed.", "continuation_intent": false}"#,
            ),
            (
                XML_ENVELOPE,
                r#"{"malformed": true, "reason": "xml_tool_call_envelope", "cleaned": "", "continuation_intent": false}"#,
            ),
            (
                MARKDOWN_FENCE,
                r#"{"malformed": true, "reason": "markdown_tool_call_fence", "cleaned": "start\nend", "continuation_intent": false}"#,
            ),
            (
                CONTINUATION,
                r#"{"malformed": false, "reason": "continuation_intent", "cleaned": "I'll update the subtasks with what I've established and deliver the final answer. First batch: inspect the last commits.", "continuation_intent": true}"#,
            ),
            (
                PLAIN_SUMMARY,
                r#"{"malformed": false, "reason": "clean", "cleaned": "Committed abc123 and pushed to origin/main; the reproduction run was not executed.", "continuation_intent": false}"#,
            ),
        ];
        for (raw, plugin_json) in corpus {
            let verdict = ToolVerdict::from_json(plugin_json).expect("plugin verdict parses");
            for gate in [true, false] {
                let via_tool = hygiene_from_verdict(raw, &verdict, gate);
                let builtin = builtin_hygiene(raw, gate);
                assert_eq!(
                    via_tool.fallback, builtin.fallback,
                    "fallback mismatch for {:?} (gate={})",
                    raw, gate
                );
                assert_eq!(
                    via_tool.cleaned, builtin.cleaned,
                    "cleaned mismatch for {:?} (gate={})",
                    raw, gate
                );
                assert!(via_tool.via_tool);
            }
        }
        // The tool's PRIMARY verdict flags the malformed-markup cases only.
        assert!(ToolVerdict::from_json(corpus[0].1).unwrap().malformed);
        assert!(ToolVerdict::from_json(corpus[1].1).unwrap().malformed);
        assert!(ToolVerdict::from_json(corpus[2].1).unwrap().malformed);
        assert!(ToolVerdict::from_json(corpus[3].1).unwrap().malformed);
        assert!(!ToolVerdict::from_json(corpus[4].1).unwrap().malformed);
        assert!(!ToolVerdict::from_json(corpus[5].1).unwrap().malformed);
    }

    #[test]
    fn tool_continuation_verdict_drives_the_summary_paths() {
        let v = verdict(false, CONTINUATION, true);
        assert!(hygiene_from_verdict(CONTINUATION, &v, true).fallback);
        assert!(!hygiene_from_verdict(CONTINUATION, &v, false).fallback);
    }

    #[test]
    fn tool_malformed_verdict_with_empty_cleaned_falls_back() {
        let v = verdict(true, "", false);
        let h = hygiene_from_verdict(DSML_ENVELOPE, &v, true);
        assert!(h.fallback);
        assert!(h.malformed);
        assert!(h.via_tool);
    }

    #[test]
    fn missing_tool_cleaned_falls_back_to_the_builtin_cleaner() {
        // A malformed message whose envelope the tool stripped but whose
        // surrounding prose it failed to return: the built-in cleaner still
        // guarantees no markup is persisted.
        let v = verdict(true, "", false);
        let h = hygiene_from_verdict(DSML_WITH_PROSE, &v, true);
        assert!(!h.fallback);
        assert!(h.cleaned.contains("hit its iteration limit"));
        assert!(!contains_dsml_markup(&h.cleaned));
    }

    #[test]
    fn verdict_json_parsing() {
        let v = ToolVerdict::from_json(
            "{\"malformed\":true,\"reason\":\"dsml_markup\",\"cleaned\":\"prose\",\"continuation_intent\":false}",
        )
        .expect("valid verdict");
        assert!(v.malformed);
        assert_eq!(v.reason, "dsml_markup");
        assert_eq!(v.cleaned.as_deref(), Some("prose"));
        assert_eq!(v.continuation_intent, Some(false));

        // Whitespace-only cleaned is "nothing remains".
        let v = ToolVerdict::from_json(
            "{\"malformed\":true,\"reason\":\"dsml_markup\",\"cleaned\":\"  \"}",
        )
        .expect("valid verdict");
        assert_eq!(v.cleaned, None);
        assert_eq!(v.continuation_intent, None);

        // Not JSON -> error (caller logs a warning and keeps the old behaviour).
        assert!(ToolVerdict::from_json("not json").is_err());
        // JSON without the `malformed` verdict -> error, never a silent "clean".
        assert!(ToolVerdict::from_json("{\"reason\":\"ok\",\"cleaned\":\"x\"}").is_err());
    }
}
