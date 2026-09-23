//! Audit V-2 acceptance tests: core agent behaviour (self-restart guard,
//! proactive subtask reminder, read-only excerpt set) is resolved from tool
//! BEHAVIOUR DESCRIPTORS declared in the plugin manifests, never from a
//! hardcoded tool-name allowlist.
//!
//! This is an integration target on purpose: the descriptor-derived sets are
//! part of the crate's public contract, and the shipped manifests are parsed
//! through the same public types the plugin scanner uses.
//!
//! NOTE (efficiency contract, incident 2874): the core does NOT classify an
//! INVOCATION as read-only and does not guard repeats of one; a repeated
//! invocation is handled generically by the invocation ledger in
//! `omniagent::agent::efficiency`. The read-only DESCRIPTOR below only drives
//! the excerpt policy of the prompt/compaction plugin.

use std::sync::Arc;

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

/// A tool exactly as the external-MCP scanner registers it: the registry name
/// is `{server}_{tool-name-with-dashes}`.
fn mcp_tool(server: &str, raw_name: &str) -> McpTool {
    McpTool {
        name: tool_qualify(server, raw_name),
        description: format!("Tool: {}", raw_name),
        input_schema: json!({"type": "object", "properties": {}}),
        server_name: Some(server.to_string()),
        timeout_secs: None,
        behavior: ToolBehavior::default(),
        handler: stub_handler(),
    }
}

/// (a) A read-only tool registered under an id nobody ever hardcoded IS
/// recognised as read-only; a tool that declares nothing is NOT (fail closed).
#[test]
fn read_only_tool_under_a_new_id_is_recognised() {
    let mut registry = McpRegistry::new();

    let mut new_read = mcp_tool("zorp", "inspect_widget");
    new_read.behavior.read_only = true;
    registry.register(new_read);
    registry.register(mcp_tool("zorp", "mutate_widget"));

    let reads = registry.read_only_tools();
    assert!(
        reads.contains(&"zorp__inspect_widget".to_string()),
        "a declared read-only tool must be recognised whatever its id: {reads:?}"
    );
    assert!(
        !reads.contains(&"zorp__mutate_widget".to_string()),
        "an undeclared tool must not be treated as read-only"
    );
}

/// (b) The self-restart guard follows the descriptor: a container tool renamed
/// to anything else still lands in the own-stack set, and a container tool
/// without the descriptor does not.
#[test]
fn renamed_own_stack_tool_still_triggers_the_self_restart_guard() {
    let mut registry = McpRegistry::new();

    let mut compose = mcp_tool("docker", "compose");
    compose.behavior.affects_own_stack = true;
    registry.register(compose);

    // Hypothetical rename of the very same capability.
    let mut renamed = mcp_tool("containers", "compose_stack");
    renamed.behavior.affects_own_stack = true;
    registry.register(renamed);

    // A container-ish tool that declares nothing stays out (fail closed).
    registry.register(mcp_tool("containers", "ps"));

    let own_stack = registry.own_stack_tools();
    assert!(own_stack.contains("docker__compose"));
    assert!(
        own_stack.contains("containers__compose_stack"),
        "the self-restart guard must follow the declared behaviour, not the name"
    );
    assert!(!own_stack.contains("containers__ps"));
}

/// (c) A coordination tool declared with `"family": "subtasks"` is recognised
/// under any id, so the proactive reminder is not tied to one tool name.
#[test]
fn subtask_family_tools_are_recognised_under_any_id() {
    let mut registry = McpRegistry::new();

    let mut manage = mcp_tool("subtasks", "manage_subtasks");
    manage.behavior.family = Some("subtasks".to_string());
    registry.register(manage);

    let mut renamed = mcp_tool("coordination", "zap_thread_items");
    renamed.behavior = ToolBehavior {
        family: Some("subtasks".to_string()),
        ..Default::default()
    };
    registry.register(renamed);

    let family = registry.family_tools("subtasks");
    assert!(family.contains("subtasks__manage_subtasks"));
    assert!(
        family.contains("coordination__zap_thread_items"),
        "a renamed subtask tool must still reset the reminder counter: {family:?}"
    );
    assert!(registry.family_tools("kanban").is_empty());
}
