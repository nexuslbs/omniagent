//! Structured, actionable errors for unresolved MCP tool names.
//!
//! Operator requirement (telegram 2039/2040, 2026-09-15): an unknown /
//! unregistered / disabled tool must never surface as a raw `502` with a bare
//! `Unknown tool: X`. The dashboard-facing `/mcp/execute` endpoint classifies
//! the failure here and answers with a machine-readable code, the requested
//! tool name, the REASON (plugin not installed / plugin disabled / tool
//! unknown) and a short remediation hint, plus the fuzzy "did you mean"
//! suggestions the API already produced.
//!
//! This is the core MCP/HTTP layer, so EVERY dashboard proxy that executes a
//! tool by name benefits - not only the database page.

use serde_json::{json, Value};

use crate::mcp::{levenshtein_distance, tool_qualify};

pub(crate) const CODE_NAME_REQUIRED: &str = "tool_name_required";
pub(crate) const CODE_UNKNOWN: &str = "tool_unknown";
pub(crate) const CODE_PLUGIN_DISABLED: &str = "tool_plugin_disabled";
pub(crate) const CODE_PLUGIN_NOT_INSTALLED: &str = "tool_plugin_not_installed";
pub(crate) const CODE_PLUGIN_UNAVAILABLE: &str = "tool_plugin_unavailable";
pub(crate) const CODE_EXECUTION_FAILED: &str = "tool_execution_failed";

/// A tool DECLARED by a configured plugin: (plugin name, declared tool name,
/// plugin status as reported by `plugins_yaml`).
pub(crate) type DeclaredTool = (String, String, String);

/// The core read-only DB API needs NO plugin: this is the remediation hint
/// offered whenever a database tool cannot be resolved.
pub(crate) const CORE_DB_API_HINT: &str =
    "The core read-only DB API needs no plugin: POST /db/query and GET /db/tables.";

/// Classified failure to resolve a tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolLookupFailure {
    pub status: u16,
    pub code: &'static str,
    pub tool: String,
    pub reason: String,
    pub remediation: String,
    pub plugin: Option<String>,
    pub suggestions: Vec<String>,
}

impl ToolLookupFailure {
    /// JSON body returned to the caller.
    pub fn to_json(&self) -> Value {
        let error_text = if self.code == CODE_EXECUTION_FAILED {
            format!("Tool '{}' failed: {}", self.tool, self.reason)
        } else {
            format!("Tool '{}' is not available: {}", self.tool, self.reason)
        };
        let mut body = json!({
            "success": false,
            "error": error_text,
            "error_code": self.code,
            "tool": self.tool,
            "reason": self.reason,
            "remediation": self.remediation,
            "suggestions": self.suggestions,
        });
        if let Some(plugin) = &self.plugin {
            body["plugin"] = json!(plugin);
        }
        body
    }
}

/// Empty tool name: a bad request, not a lookup failure.
pub(crate) fn name_required() -> ToolLookupFailure {
    ToolLookupFailure {
        status: 400,
        code: CODE_NAME_REQUIRED,
        tool: String::new(),
        reason: "no tool name was provided".to_string(),
        remediation: "Send a non-empty tool name in the request body ({\"name\": \"...\"})."
            .to_string(),
        plugin: None,
        suggestions: Vec::new(),
    }
}

/// A registered tool whose execution failed (status stays 200 + success:false
/// for backward compatibility with existing dashboard proxies).
pub(crate) fn execution_failed(tool: &str, error: &str) -> ToolLookupFailure {
    ToolLookupFailure {
        status: 200,
        code: CODE_EXECUTION_FAILED,
        tool: tool.to_string(),
        reason: error.to_string(),
        remediation: "Check the tool's arguments (see its input schema) and the plugin logs."
            .to_string(),
        plugin: tool.split_once("__").map(|(p, _)| p.to_string()),
        suggestions: Vec::new(),
    }
}

/// Fuzzy suggestions for a name: registered + declared candidates within
/// Levenshtein distance 3 of the request (shortest first), bounded to 3.
fn suggestions_for(requested: &str, candidates: &[String]) -> Vec<String> {
    let mut scored: Vec<(usize, String)> = candidates
        .iter()
        .map(|c| (levenshtein_distance(requested, c), c.clone()))
        .filter(|(dist, c)| *dist <= 3 && *dist < requested.len() && !c.is_empty())
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, c)| c).take(3).collect()
}

/// Classify an unresolved tool name.
///
/// * `requested` - the fully-qualified name the caller asked for.
/// * `registered` - fully-qualified names currently registered (running tools).
/// * `declared` - tools declared by CONFIGURED plugins (even if not running).
/// * `known_plugins` - plugin names known to the config (`plugins.yml`).
pub(crate) fn classify(
    requested: &str,
    registered: &[String],
    declared: &[DeclaredTool],
    known_plugins: &[String],
) -> ToolLookupFailure {
    let req_lc = requested.to_ascii_lowercase();

    // 1) The name matches a tool DECLARED by a configured plugin: the plugin is
    //    disabled, failed to start, or is otherwise not running.
    for (plugin, tool, status) in declared {
        let declared_lc = tool.to_ascii_lowercase();
        let qualified_lc = tool_qualify(plugin, tool).to_ascii_lowercase();
        if req_lc == declared_lc || req_lc == qualified_lc {
            let status_lc = status.to_ascii_lowercase();
            let (code, reason, remediation) = match status_lc.as_str() {
                "disabled" => (
                    CODE_PLUGIN_DISABLED,
                    format!(
                        "the plugin '{plugin}' is DISABLED, so its tool '{tool}' is not registered"
                    ),
                    format!(
                        "Enable the '{plugin}' plugin (Dashboard > Plugins, or config/plugins.yml) and retry. {CORE_DB_API_HINT}"
                    ),
                ),
                "enabled" => {
                    // The runtime liveness record knows WHY: a child that ran and
                    // then died (exit status + stderr tail), a supervised restart
                    // in progress, a handshake that never completed, or a give-up
                    // after N attempts (2026-09-27).
                    let status = crate::mcp::external::supervisor::runtime_status(plugin);
                    let reason = match &status {
                        Some(s)
                            if s.state
                                != crate::mcp::external::supervisor::LivenessState::Running =>
                        {
                            format!(
                                "the plugin '{plugin}' is enabled but its tool server is not serving tools: {} (tool '{tool}' not registered)",
                                s.message
                            )
                        }
                        _ => format!(
                            "the plugin '{plugin}' is enabled but its tool server is not running (tool '{tool}' not registered)"
                        ),
                    };
                    let remediation = match &status {
                        Some(_) => crate::mcp::external::supervisor::remediation(plugin),
                        None => format!(
                            "Restart/reinstall the '{plugin}' plugin and check its logs (bundle may not be compiled)."
                        ),
                    };
                    (
                        CODE_PLUGIN_UNAVAILABLE,
                        reason,
                        format!("{remediation} {CORE_DB_API_HINT}"),
                    )
                }
                other => (
                    CODE_PLUGIN_UNAVAILABLE,
                    format!(
                        "the plugin '{plugin}' is not serving tools (status: '{other}'), so tool '{tool}' is not registered"
                    ),
                    format!(
                        "Fix the '{plugin}' plugin state (Dashboard > Plugins) and retry. {CORE_DB_API_HINT}"
                    ),
                ),
            };
            return ToolLookupFailure {
                status: 503,
                code,
                tool: requested.to_string(),
                reason,
                remediation,
                plugin: Some(plugin.clone()),
                suggestions: Vec::new(),
            };
        }
    }

    // 2) `{plugin}__{tool}` name whose plugin prefix is not registered at all:
    //    the plugin is missing / not installed.
    if let Some((prefix, _tool)) = requested.split_once("__") {
        let prefix_lc = prefix.to_ascii_lowercase();
        if !prefix.is_empty()
            && !prefix_lc.eq_ignore_ascii_case("core")
            && !known_plugins
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&prefix_lc))
        {
            return ToolLookupFailure {
                status: 503,
                code: CODE_PLUGIN_NOT_INSTALLED,
                tool: requested.to_string(),
                reason: format!("the plugin '{prefix}' is not installed or not listed in config/plugins.yml"),
                remediation: format!(
                    "Install and enable the '{prefix}' plugin (Dashboard > Plugins), or use a core tool. {CORE_DB_API_HINT}"
                ),
                plugin: Some(prefix.to_string()),
                suggestions: Vec::new(),
            };
        }
    }

    // 3) Genuinely unknown tool: keep the fuzzy "did you mean" suggestions.
    let mut candidates: Vec<String> = registered.to_vec();
    for (plugin, tool, _status) in declared {
        candidates.push(tool.clone());
        candidates.push(tool_qualify(plugin, tool));
    }
    candidates.sort();
    candidates.dedup();
    let suggestions = suggestions_for(requested, &candidates);

    let reason = if suggestions.is_empty() {
        format!("no tool named '{requested}' is registered or declared by any configured plugin")
    } else {
        format!(
            "no tool named '{requested}' is registered; did you mean {}?",
            suggestions
                .iter()
                .map(|s| format!("'{s}'"))
                .collect::<Vec<_>>()
                .join(" or ")
        )
    };

    ToolLookupFailure {
        status: 404,
        code: CODE_UNKNOWN,
        tool: requested.to_string(),
        reason,
        remediation: format!(
            "Check the tool name (GET /mcp/tools lists registered tools). {CORE_DB_API_HINT}"
        ),
        plugin: None,
        suggestions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> Vec<String> {
        vec![
            "core__fail_thread".to_string(),
            "search__messages".to_string(),
        ]
    }

    #[test]
    fn disabled_plugin_tool_is_reported_as_disabled() {
        let declared = vec![(
            "search".to_string(),
            "search_database".to_string(),
            "disabled".to_string(),
        )];
        let f = classify(
            "search_database",
            &registered(),
            &declared,
            &["search".to_string()],
        );
        assert_eq!(f.status, 503);
        assert_eq!(f.code, CODE_PLUGIN_DISABLED);
        assert_eq!(f.plugin.as_deref(), Some("search"));
        assert!(f.reason.contains("DISABLED"));
        assert!(f.remediation.contains("core read-only DB API"));
    }

    #[test]
    fn declared_enabled_but_unregistered_is_unavailable() {
        let declared = vec![(
            "search".to_string(),
            "search_database".to_string(),
            "enabled".to_string(),
        )];
        let f = classify(
            "search__database",
            &registered(),
            &declared,
            &["search".to_string()],
        );
        assert_eq!(f.status, 503);
        assert_eq!(f.code, CODE_PLUGIN_UNAVAILABLE);
    }

    #[test]
    fn unknown_name_reports_suggestions() {
        let f = classify("core__fail_thred", &registered(), &[], &[]);
        assert_eq!(f.status, 404);
        assert_eq!(f.code, CODE_UNKNOWN);
        assert_eq!(f.suggestions, vec!["core__fail_thread".to_string()]);
    }

    #[test]
    fn unknown_plugin_prefix_is_not_installed() {
        let f = classify("nosuch__thing", &registered(), &[], &["search".to_string()]);
        assert_eq!(f.status, 503);
        assert_eq!(f.code, CODE_PLUGIN_NOT_INSTALLED);
        assert_eq!(f.plugin.as_deref(), Some("nosuch"));
    }

    #[test]
    fn empty_name_is_a_bad_request() {
        let f = name_required();
        assert_eq!(f.status, 400);
        assert_eq!(f.code, CODE_NAME_REQUIRED);
    }

    #[test]
    fn json_body_carries_code_tool_reason_and_remediation() {
        let declared = vec![(
            "search".to_string(),
            "search_database".to_string(),
            "disabled".to_string(),
        )];
        let f = classify(
            "search_database",
            &registered(),
            &declared,
            &["search".to_string()],
        );
        let body = f.to_json();
        assert_eq!(body["error_code"], json!(CODE_PLUGIN_DISABLED));
        assert_eq!(body["tool"], json!("search_database"));
        assert!(body["reason"].as_str().unwrap().contains("DISABLED"));
        assert!(body["remediation"].as_str().unwrap().len() > 10);
        assert_eq!(body["plugin"], json!("search"));
        assert_eq!(body["success"], json!(false));
    }
}
