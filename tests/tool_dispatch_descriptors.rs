//! Audit HV-A1 / T5.1 acceptance: the background-dispatch policy of a tool
//! comes from the tool's OWN descriptor (plugin manifest `dispatch:`), never
//! from a core-side tool-name list. A tool registered under another id keeps
//! its declared policy; a tool that declares nothing keeps the legacy
//! fail-open list policy, so removing nothing changes behaviour for tools that
//! never declared one.
//!
//! This is an integration target on purpose: `declared_dispatches()` and
//! `dispatch_mode_with()` are the crate's public contract for the decision, and
//! the registry below is populated through the same public types the plugin
//! scanner uses.

use std::sync::Arc;

use omniagent::agent::background_dispatch::{
    dispatch_mode_with, mode_from_declaration, DispatchMode,
};
use omniagent::mcp::behavior::ToolBehavior;
use omniagent::mcp::{
    tool_qualify, AppContext, McpRegistry, McpTool, McpToolHandler, McpToolResult,
};
use serde_json::{json, Value};

fn stub_handler() -> McpToolHandler {
    Arc::new(|_args: Value, _ctx: AppContext| {
        Box::pin(async {
            Ok(McpToolResult {
                call_id: String::new(),
                content: "ok".to_string(),
                is_error: false,
            })
        })
    })
}

fn tool(server: &str, raw_name: &str, dispatch: Option<&str>) -> McpTool {
    let mut behavior = ToolBehavior::default();
    behavior.dispatch = dispatch.map(str::to_string);
    McpTool {
        name: tool_qualify(server, raw_name),
        description: format!("Tool: {}", raw_name),
        input_schema: json!({"type": "object", "properties": {}}),
        server_name: Some(server.to_string()),
        timeout_secs: None,
        behavior,
        handler: stub_handler(),
    }
}

/// A tool that declares `dispatch: immediate` is backgrounded on the first
/// call even though its name appears in no core list.
#[test]
fn declared_immediate_policy_is_honoured_for_an_unknown_name() {
    let mut registry = McpRegistry::new();
    registry.register(tool("zorp", "slow_thing", Some("immediate")));

    let declared = registry.declared_dispatches();
    assert_eq!(
        declared
            .get(&tool_qualify("zorp", "slow_thing"))
            .map(String::as_str),
        Some("immediate")
    );
    assert_eq!(
        dispatch_mode_with(
            &tool_qualify("zorp", "slow_thing"),
            declared
                .get(&tool_qualify("zorp", "slow_thing"))
                .map(String::as_str)
        ),
        DispatchMode::Immediate,
        "a declared policy must win over the absence of a core list entry"
    );
}

/// The declared policy also OVERRIDES a legacy list entry: a listed tool that
/// declares `threshold` is not eagerly backgrounded.
#[test]
fn declared_policy_overrides_a_legacy_list_entry() {
    let mut registry = McpRegistry::new();
    registry.register(tool("ssh", "run", Some("threshold")));
    let declared = registry.declared_dispatches();
    assert_eq!(
        dispatch_mode_with("ssh__run", declared.get("ssh__run").map(String::as_str)),
        DispatchMode::Threshold
    );
}

/// Tools that declare NO dispatch policy are unaffected: they keep exactly the
/// legacy list-based classification (fail-open default preserved).
#[test]
fn undeclared_tools_keep_the_legacy_policy() {
    let mut registry = McpRegistry::new();
    registry.register(tool("ssh", "run", None));
    registry.register(tool("git", "commit_and_push", None));
    registry.register(tool("filesystem", "read", None));

    let declared = registry.declared_dispatches();
    assert!(
        declared.is_empty(),
        "no manifest declared a dispatch policy"
    );

    assert_eq!(
        dispatch_mode_with("ssh__run", declared.get("ssh__run").map(String::as_str)),
        DispatchMode::Immediate
    );
    assert_eq!(
        dispatch_mode_with(
            "git__commit_and_push",
            declared.get("git__commit_and_push").map(String::as_str)
        ),
        DispatchMode::Immediate
    );
    assert_eq!(
        dispatch_mode_with(
            "filesystem__read",
            declared.get("filesystem__read").map(String::as_str)
        ),
        DispatchMode::Threshold
    );
}

/// An unrecognised declaration never invents a policy: it falls back to the
/// legacy classification instead of, say, disabling backgrounding.
#[test]
fn unknown_declaration_falls_back_to_the_list() {
    assert_eq!(mode_from_declaration("background"), None);
    assert_eq!(
        dispatch_mode_with("ssh__run", Some("background")),
        DispatchMode::Immediate
    );
    assert_eq!(
        dispatch_mode_with("filesystem__read", Some("background")),
        DispatchMode::Threshold
    );
}

/// The SHIPPED manifests declare the dispatch policy of their long-running
/// tools, so those tools no longer depend on the core's legacy name list.
#[test]
fn shipped_manifests_declare_their_dispatch_policy() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/tools");
    let read = |rel: &str| -> Vec<omniagent::mcp::behavior::ToolManifestEntry> {
        let text = std::fs::read_to_string(root.join(rel)).expect("manifest present");
        let value: Value = serde_json::from_str(&text).expect("manifest is JSON");
        serde_json::from_value(value["tools"].clone()).expect("tools array parses")
    };

    let docker = read("docker/plugin.json");
    let compose = docker
        .iter()
        .find(|e| e.name == "compose")
        .expect("docker compose entry present");
    assert_eq!(
        compose.resolved().dispatch.as_deref(),
        Some("immediate"),
        "docker compose must declare its dispatch policy in its own manifest"
    );

    let git = read("git/plugin.json");
    for name in ["run_command", "git_sync"] {
        let entry = git
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("git entry '{name}' present"));
        assert_eq!(
            entry.resolved().dispatch.as_deref(),
            Some("immediate"),
            "{name} must declare its dispatch policy in its own manifest"
        );
    }
}
