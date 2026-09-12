//! Audit V-2 acceptance tests: core agent behaviour (exact-repeat read guard,
//! self-restart guard, proactive subtask reminder) is resolved from tool
//! BEHAVIOUR DESCRIPTORS declared in the plugin manifests, never from a
//! hardcoded tool-name allowlist.
//!
//! This is an integration target on purpose: the descriptor-derived sets are
//! part of the crate's public contract, and the shipped manifests are parsed
//! through the same public types the plugin scanner uses.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use omniagent::agent::helpers::is_guarded_read_only;
use omniagent::mcp::behavior::{
    behavior_map, for_tool, looks_like_read_tool, ToolBehavior, ToolManifestEntry,
};
use omniagent::mcp::{
    tool_qualify, AppContext, McpRegistry, McpTool, McpToolHandler, McpToolResult,
};
use omniagent::plugin::PluginManifest;
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
/// guarded; a tool that declares nothing is NOT (fail closed).
#[test]
fn read_only_tool_under_a_new_id_is_guarded() {
    let mut registry = McpRegistry::new();

    let mut new_read = mcp_tool("zorp", "inspect_widget");
    new_read.behavior.read_only = true;
    registry.register(new_read);
    registry.register(mcp_tool("zorp", "mutate_widget"));

    let guarded = registry.guarded_read_only_tools();
    assert!(
        guarded.contains("zorp__inspect_widget"),
        "a declared read-only tool must be guarded whatever its id: {guarded:?}"
    );
    assert!(is_guarded_read_only(&guarded, "zorp__inspect_widget"));
    assert!(
        !is_guarded_read_only(&guarded, "zorp__mutate_widget"),
        "an undeclared tool must not be treated as read-only"
    );
    assert!(registry
        .read_only_tools()
        .contains(&"zorp__inspect_widget".to_string()));
    assert!(!registry
        .read_only_tools()
        .contains(&"zorp__mutate_widget".to_string()));
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

/// (d) No behaviour change: the descriptors shipped in `plugins/tools/*` must
/// derive EXACTLY the guarded-read set the removed name allowlist produced.
///
/// The builtin Rust memory plugin was removed (remote Python plugin only), so
/// `memory__list_memories` is no longer declared by any manifest shipped in
/// this repository and is intentionally absent from the expected set.
#[test]
fn shipped_manifests_reproduce_the_legacy_guarded_read_set() {
    let expected: BTreeSet<&str> = [
        "filesystem__read",
        "filesystem__info",
        "filesystem__list",
        "filesystem__search",
        "git__status",
        "git__run_command",
        "git__sync",
        "notes__note_read",
        "search__messages",
        "search__wiki",
        "search__database",
        "search__thread_messages",
        "search__channel_prompts",
        "skills__list_skills",
        "skills__view_skill",
    ]
    .into_iter()
    .collect();

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/tools");
    let mut guarded: BTreeSet<String> = BTreeSet::new();
    let mut declaring_plugins = 0usize;
    let mut declared_tools = 0usize;

    for dir in std::fs::read_dir(&root).expect("plugins/tools is readable") {
        let dir = dir.expect("dir entry").path();
        let manifest_path = dir.join("plugin.json");
        if !manifest_path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("{}: {e}", manifest_path.display()));
        let manifest: PluginManifest = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", manifest_path.display()));
        if manifest.tools.is_empty() {
            continue;
        }
        let server = dir
            .file_name()
            .expect("plugin dir name")
            .to_string_lossy()
            .to_string();
        let map = behavior_map(&manifest.tools);
        assert_eq!(
            map.len(),
            manifest.tools.len(),
            "{server}: duplicate descriptor names"
        );
        declaring_plugins += 1;
        for entry in &manifest.tools {
            declared_tools += 1;
            let qualified = tool_qualify(&server, &entry.name);
            // Resolve exactly like the registry does (raw or qualified name).
            let behavior = for_tool(&map, &server, &qualified);
            if behavior.repeat_guard_enabled() {
                guarded.insert(qualified);
            }
        }
    }

    assert!(
        declaring_plugins >= 7,
        "most tool plugins must declare descriptors, only {declaring_plugins} did"
    );
    assert!(
        declared_tools >= 16,
        "too few declared tools: {declared_tools}"
    );
    let guarded_refs: BTreeSet<&str> = guarded.iter().map(String::as_str).collect();
    assert_eq!(
        guarded_refs, expected,
        "descriptor-derived guarded set must equal the legacy allowlist"
    );
}

/// Fail CLOSED BUT LOUD: an undeclared read-looking tool is not guarded, and
/// the heuristic that drives the one-per-process warning still sees it.
#[test]
fn undeclared_read_looking_tool_fails_closed_and_is_flagged() {
    let entries: Vec<ToolManifestEntry> = serde_json::from_value(json!([
        { "name": "search_widgets", "behavior": { "read_only": true } },
        { "name": "zap_widgets" }
    ]))
    .expect("descriptor entries parse");
    let map = behavior_map(&entries);

    assert!(for_tool(&map, "zorp", "search_widgets").repeat_guard_enabled());
    assert!(!for_tool(&map, "zorp", "zap_widgets").read_only);
    assert!(!for_tool(&map, "zorp", "search_undeclared").read_only);
    assert!(looks_like_read_tool("search_undeclared"));
    assert!(!looks_like_read_tool("zap_widgets"));
}
