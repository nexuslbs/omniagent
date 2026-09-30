//! Shared background-dispatch policy for tool calls.
//!
//! Tool execution must be decoupled from the LLM turn: a tool whose call can
//! block for a long time must be dispatched as a BACKGROUND tool task (tracked
//! by the shared [`crate::agent::task_registry::TaskRegistry`]) so the agent
//! can follow it with `core__wait_task` / `core__poll_task`, read progress
//! with `core__read_task_logs` and abort it with `core__cancel_task`.
//!
//! The decision lives HERE and nowhere else - one shared policy, no per-plugin
//! ad-hoc handling. There are exactly three modes:
//!
//! * [`DispatchMode::Sync`] - the core control-plane tools that ARE the
//!   interface TO the background-task system, plus the other fast core
//!   coordination tools. They must never be backgrounded: the agent is blocked
//!   on their result and a backgrounded `wait_task` would return a NEW task id
//!   instead of the awaited result (deploy Groups 13/14 regression).
//! * [`DispatchMode::Immediate`] - long-running tools that must not block the
//!   turn AT ALL: they are registered in the task registry on the FIRST call
//!   and answered immediately with `{"status":"processing","task_id":...}`.
//! * [`DispatchMode::Threshold`] - everything else (fast tools): the call runs
//!   inline for at most the configured `tool_bg_secs`; if it is still running
//!   it is handed to the SAME registry and answered with the processing
//!   envelope.
//!
//! Tool names are the EXPOSED names (`{plugin}__{tool}`) produced by
//! [`crate::mcp::tool_qualify`]; the unit tests assert every entry against
//! `tool_qualify`, so a future rename cannot silently drop a tool from its
//! mode.

/// How a tool call is dispatched relative to the agent turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Run inline, block the turn: core control-plane / fast core tools.
    Sync,
    /// Register as a background task immediately, never block the turn.
    Immediate,
    /// Run inline for up to `tool_bg_secs`, then become a background task.
    Threshold,
}

/// Core control-plane tools (never backgrounded). These are the interface TO
/// the background-task system plus the fast core coordination tools the agent
/// needs an inline answer from.
pub const SYNC_CONTROL_TOOLS: &[&str] = &[
    // The background-task control plane itself.
    "core__wait_task",
    "core__poll_task",
    "core__cancel_task",
    "core__read_task_logs",
    "core__wait_for_status",
    // Call a tool + immediately wait for its background task in one call:
    // must stay synchronous (it IS the wait - backgrounding it would return a
    // NEW task id instead of the awaited result).
    "core__call_and_wait",
    // Fast core coordination tools.
    "core__read_attached_file",
    "core__fail_thread",
    "core__omniagent_api",
    "core__list_tool_details",
];

/// Long-running tools dispatched to the background on the FIRST call: the
/// answer is `status=processing` + a task id, the work continues as a tracked
/// background task.
pub const IMMEDIATE_BACKGROUND_TOOLS: &[&str] = &[
    "ssh__run",
    "ssh__copy",
    "docker__compose",
    "workbench__tool",
    "workstation__tool",
    "fetch__fetch",
    "git__clone_repo",
    "git__commit_and_push",
    "git__run_command",
    "git__sync",
];

/// Tools whose dispatch is decided by the shared policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchClass {
    /// The core control-plane / fast core tool list (never backgrounded).
    Sync,
    /// The long-running tool list (backgrounded immediately).
    ImmediateBackground,
    /// Any other tool (fast path, backgrounded only past the threshold).
    DefaultThreshold,
}

/// Classify a tool by its EXPOSED name. Unknown/absent tools fall into the
/// default threshold class (fast path), never into a priority class.
pub fn classify(tool_name: &str) -> DispatchClass {
    if SYNC_CONTROL_TOOLS.contains(&tool_name) {
        DispatchClass::Sync
    } else if IMMEDIATE_BACKGROUND_TOOLS.contains(&tool_name) {
        DispatchClass::ImmediateBackground
    } else {
        DispatchClass::DefaultThreshold
    }
}

/// The dispatch mode for a tool by its EXPOSED name.
pub fn dispatch_mode(tool_name: &str) -> DispatchMode {
    match classify(tool_name) {
        DispatchClass::Sync => DispatchMode::Sync,
        DispatchClass::ImmediateBackground => DispatchMode::Immediate,
        DispatchClass::DefaultThreshold => DispatchMode::Threshold,
    }
}

/// True for the core control-plane tools that must stay synchronous.
pub fn is_sync_control_tool(tool_name: &str) -> bool {
    classify(tool_name) == DispatchClass::Sync
}

/// True for the long-running tools that must be backgrounded immediately.
pub fn is_immediate_background_tool(tool_name: &str) -> bool {
    classify(tool_name) == DispatchClass::ImmediateBackground
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{tool_qualify, CORE_PLUGIN_NAME};

    /// Split an exposed name into `(plugin, tool)` the way `tool_qualify`
    /// composes it: `{plugin}__{tool}`.
    fn split(name: &str) -> (&str, &str) {
        name.split_once("__")
            .unwrap_or_else(|| panic!("'{}' is not a qualified plugin__tool name", name))
    }

    /// Every listed name must be exactly what `tool_qualify` produces for its
    /// `(plugin, tool)` components: a rename cannot silently drop a tool from
    /// its dispatch class.
    #[test]
    fn every_listed_name_is_a_qualified_exposed_name() {
        for name in SYNC_CONTROL_TOOLS.iter().chain(IMMEDIATE_BACKGROUND_TOOLS) {
            let (plugin, tool) = split(name);
            assert_eq!(
                &tool_qualify(plugin, tool),
                name,
                "listed name '{}' is not the exposed name produced by tool_qualify",
                name
            );
        }
    }

    /// The core control-plane tools are exactly the ones that must stay
    /// synchronous: the task interface plus the fast core coordination tools.
    #[test]
    fn core_control_plane_tools_are_sync() {
        for tool in [
            "wait_task",
            "poll_task",
            "cancel_task",
            "read_task_logs",
            "wait_for_status",
            "call_and_wait",
            "read_attached_file",
            "fail_thread",
            "omniagent_api",
            "list_tool_details",
        ] {
            let name = tool_qualify(CORE_PLUGIN_NAME, tool);
            assert_eq!(
                dispatch_mode(&name),
                DispatchMode::Sync,
                "{} must stay synchronous",
                name
            );
            assert!(is_sync_control_tool(&name));
            assert!(!is_immediate_background_tool(&name));
        }
        // No core tool is backgrounded immediately.
        for name in IMMEDIATE_BACKGROUND_TOOLS {
            assert!(!name.starts_with("core__"), "{}", name);
        }
        // The legacy dashed spelling is NOT recognised: it can never match the
        // `__` grammar and once silently backgrounded `wait_task` (regression,
        // deploy Groups 13/14).
        assert!(!is_sync_control_tool("core__wait-task"));
        assert_eq!(dispatch_mode("core__wait-task"), DispatchMode::Threshold);
        // The retired core namespace is not a core tool name any more (built
        // via format! on purpose).
        let retired = format!("{}__{}", "builtin", "wait_task");
        assert!(
            !is_sync_control_tool(&retired),
            "the retired core namespace must not be recognised"
        );
    }

    /// The long-running tools are backgrounded immediately, and NOT sync.
    #[test]
    fn long_running_tools_are_immediate_background() {
        let expected = [
            ("ssh", "run"),
            ("ssh", "copy"),
            ("docker", "compose"),
            ("workbench", "tool"),
            ("workstation", "tool"),
            ("fetch", "fetch"),
            ("git", "clone_repo"),
            ("git", "commit_and_push"),
            ("git", "run_command"),
            ("git", "sync"),
        ];
        for (plugin, tool) in expected {
            let name = tool_qualify(plugin, tool);
            assert_eq!(
                dispatch_mode(&name),
                DispatchMode::Immediate,
                "{} must be backgrounded immediately",
                name
            );
            assert!(is_immediate_background_tool(&name));
            assert!(!is_sync_control_tool(&name));
        }
        // Exact set: no extra entry sneaked into the immediate list.
        let mut listed: Vec<String> = IMMEDIATE_BACKGROUND_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut wanted: Vec<String> = expected.iter().map(|(p, t)| tool_qualify(p, t)).collect();
        listed.sort();
        wanted.sort();
        assert_eq!(listed, wanted);
    }

    /// Fast, unknown and non-core tools keep the threshold (fast-path) mode:
    /// they are not blocked from the turn until they actually run long.
    #[test]
    fn fast_tools_use_the_threshold_fast_path() {
        for name in [
            "filesystem__read",
            "search__messages",
            "notes__note_write",
            "subtasks__manage_subtasks",
            "memory__save_summary",
            "skills__view_skill",
        ] {
            assert_eq!(dispatch_mode(name), DispatchMode::Threshold, "{}", name);
        }
        // An unregistered/renamed tool never inherits a priority class.
        assert_eq!(dispatch_mode("nope__nope"), DispatchMode::Threshold);
        assert_eq!(dispatch_mode(""), DispatchMode::Threshold);
    }

    /// The two lists never overlap: a tool cannot be both the control plane and
    /// backgrounded.
    #[test]
    fn the_two_lists_do_not_overlap() {
        for name in IMMEDIATE_BACKGROUND_TOOLS {
            assert!(
                !SYNC_CONTROL_TOOLS.contains(name),
                "{} appears in both dispatch lists",
                name
            );
        }
    }
}
