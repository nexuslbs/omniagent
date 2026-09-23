//! Tool behaviour descriptors (audit V-2).
//!
//! Core agent behaviour must never be driven by hardcoded tool-NAME
//! allowlists: a tool registered under another id would silently lose its
//! declared protection. Instead every tool MAY declare its behaviour in its
//! plugin manifest (`plugins/tools/<plugin>/plugin.json`):
//!
//! ```json
//! {
//!   "name": "Filesystem",
//!   "type": "mcp",
//!   "tools": [
//!     { "name": "filesystem_read", "behavior": { "read_only": true } },
//!     { "name": "manage_subtasks", "family": "subtasks" },
//!     { "name": "docker_compose", "behavior": { "affects_own_stack": true } }
//!   ]
//! }
//! ```
//!
//! The manifest is read by the plugin scanner, attached to the server config
//! and carried onto every registered tool, so the core agent loop can build
//!
//! * the read-only set (the prompt plugin keeps a generous excerpt of those
//!   results when compaction drains them),
//! * the own-stack set (self-restart guard), and
//! * the subtask family set (proactive subtask reminder)
//!
//! from the registry - never from a name allowlist.
//!
//! NOTE (efficiency contract, incident 2874): the core does NOT classify an
//! INVOCATION (no subcommand/argv inspection, no name matching). A repeated
//! invocation is handled generically by the invocation ledger in
//! `crate::agent::efficiency`, which is keyed on the opaque tool id plus the
//! canonical arguments. The ledger asks each tool's OWN descriptor whether
//! executing it may have changed the world
//! ([`ToolBehavior::may_change_state`]): after such a call the thread's
//! recorded invocations are invalidated, so a later identical call is a
//! genuine repeat and executes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Declared behaviour of a single tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBehavior {
    /// The tool only READS state: an identical repeat call returns the same
    /// result. Also used by the prompt plugin to keep a generous excerpt of
    /// the result when compaction drains it (read results are the agent's
    /// working memory).
    #[serde(default)]
    pub read_only: bool,
    /// Coordination family (e.g. `subtasks`): the core loop treats every
    /// tool of the family as the same capability, so a renamed or moved tool
    /// stays recognised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// The tool can change the stack the agent itself runs in (containers,
    /// images, networks): the self-restart guard must run before it executes.
    #[serde(default)]
    pub affects_own_stack: bool,
    /// Structured declaration that executing this tool may CHANGE the world
    /// (write a file, mutate a repo, deploy something). The invocation ledger
    /// uses it to decide whether a thread's recorded invocations are still
    /// valid: after such a call the next identical invocation is a GENUINE
    /// repeat and must execute. Declared positively; when absent the tool's
    /// own `read_only` declaration is inverted, and a tool that declares
    /// neither is assumed state-changing (safe default: execute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_state: Option<bool>,
}

impl ToolBehavior {
    /// True when the entry declares no flag at all (declared but neutral).
    pub fn is_empty(&self) -> bool {
        !self.read_only
            && self.family.is_none()
            && !self.affects_own_stack
            && self.changes_state.is_none()
    }

    /// May executing this tool have changed the world?
    ///
    /// The positive declaration wins; otherwise the tool's own `read_only`
    /// declaration is inverted; a tool that declares nothing is assumed
    /// state-changing. The safe default is therefore EXECUTE: an unknown or
    /// undeclared tool invalidates the ledger instead of suppressing a call.
    pub fn may_change_state(&self) -> bool {
        self.changes_state.unwrap_or(!self.read_only)
    }
}

/// One entry of a plugin manifest's `tools` array.
///
/// Both a nested `behavior` object and flat top-level keys are accepted
/// (`"family": "subtasks"`, `"read_only": true`, ...); flat keys override the
/// nested block so a manifest can use whichever form reads better.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifestEntry {
    /// Tool name as the plugin's MCP server reports it (e.g. `note_read`) or
    /// its qualified registry name (e.g. `notes_note-read`).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<ToolBehavior>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affects_own_stack: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_state: Option<bool>,
}

impl ToolManifestEntry {
    /// Merge the nested + flat declarations into one effective behaviour.
    pub fn resolved(&self) -> ToolBehavior {
        let mut behavior = self.behavior.clone().unwrap_or_default();
        if let Some(v) = self.read_only {
            behavior.read_only = v;
        }
        if let Some(v) = &self.family {
            behavior.family = Some(v.clone());
        }
        if let Some(v) = self.affects_own_stack {
            behavior.affects_own_stack = v;
        }
        if let Some(v) = self.changes_state {
            behavior.changes_state = Some(v);
        }
        behavior
    }
}

/// Tool behaviour descriptors of one plugin, keyed by the tool name used in
/// its manifest.
pub type ToolBehaviorMap = HashMap<String, ToolBehavior>;

/// Serialize a manifest's `tools` array into a name -> behaviour map.
pub fn behavior_map(entries: &[ToolManifestEntry]) -> ToolBehaviorMap {
    let mut map = ToolBehaviorMap::new();
    for entry in entries {
        map.insert(entry.name.clone(), entry.resolved());
    }
    map
}

/// Resolve a tool's declared behaviour: the descriptor's own name wins,
/// otherwise the qualified registry name (`{plugin}__{tool}`) is matched so a
/// manifest may declare either form. During the one-release alias window the
/// PRE-FLIP qualified name (`{plugin}_{tool-with-dashes}`) also resolves.
pub fn for_tool(map: &ToolBehaviorMap, server: &str, tool_name: &str) -> ToolBehavior {
    if let Some(behavior) = map.get(tool_name) {
        return behavior.clone();
    }
    for (name, behavior) in map {
        if crate::mcp::tool_qualify(server, name) == tool_name {
            return behavior.clone();
        }
        // LEGACY ALIAS WINDOW (one release): a name produced by the old
        // grammar still resolves, so descriptors keep working for callers
        // and configs written before the separator flip.
        if crate::mcp::tool_legacy_alias(server, name) == tool_name {
            return behavior.clone();
        }
    }
    ToolBehavior::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_flat_declarations() {
        let json = serde_json::json!({
            "tools": [
                { "name": "zorp_read", "behavior": { "read_only": true } },
                { "name": "zorp_coord", "family": "coordination" },
                { "name": "zorp_stack", "behavior": { "affects_own_stack": true } },
                { "name": "zorp_flat", "read_only": true }
            ]
        });
        let entries: Vec<ToolManifestEntry> =
            serde_json::from_value(json["tools"].clone()).expect("tools array parses");
        let map = behavior_map(&entries);

        assert!(map.get("zorp_read").expect("entry present").read_only);
        assert_eq!(
            map.get("zorp_coord")
                .expect("entry present")
                .family
                .as_deref(),
            Some("coordination")
        );

        let stack = map.get("zorp_stack").expect("entry present");
        assert!(stack.affects_own_stack);
        assert!(!stack.read_only);

        // Flat form: read_only at the top level, no nested behavior block.
        assert!(map.get("zorp_flat").expect("entry present").read_only);
    }

    #[test]
    fn lookup_accepts_raw_and_qualified_names() {
        let mut map = ToolBehaviorMap::new();
        map.insert(
            "inspect_widget".to_string(),
            ToolBehavior {
                read_only: true,
                ..Default::default()
            },
        );
        // Raw plugin name.
        assert!(for_tool(&map, "zorp", "inspect_widget").read_only);
        // Qualified registry name (plugin reports the bare tool name).
        assert!(for_tool(&map, "zorp", "zorp__inspect_widget").read_only);
        // Legacy (pre-flip) qualified name: the one-release alias window.
        assert!(for_tool(&map, "zorp", "zorp_inspect-widget").read_only);
        // Unknown tool: fail closed (no declared behaviour).
        assert!(!for_tool(&map, "zorp", "mutate_widget").read_only);
    }

    /// The ledger's state-change signal comes from the tool's OWN descriptor:
    /// declared positively, else inverted from the read-only declaration, else
    /// the safe default (state-changing -> the next identical call executes).
    #[test]
    fn state_change_signal_defaults_to_safe_execute() {
        // A positive declaration wins, even next to a read-only flag.
        let declared = ToolBehavior {
            changes_state: Some(true),
            read_only: true,
            ..Default::default()
        };
        assert!(declared.may_change_state());
        // A tool that declares a stable result never invalidates the ledger.
        let stable = ToolBehavior {
            changes_state: Some(false),
            ..Default::default()
        };
        assert!(!stable.may_change_state());
        // Fallback: the tool's own read-only declaration, inverted.
        let read = ToolBehavior {
            read_only: true,
            ..Default::default()
        };
        assert!(!read.may_change_state());
        // Nothing declared (unknown tool): state-changing, never suppressed.
        assert!(ToolBehavior::default().may_change_state());
        assert!(ToolBehavior::default().is_empty());

        // The flat manifest form parses and carries the declaration.
        let json = serde_json::json!([
            { "name": "zorp_write", "changes_state": true },
            { "name": "zorp_inspect", "read_only": true }
        ]);
        let entries: Vec<ToolManifestEntry> =
            serde_json::from_value(json).expect("tools array parses");
        let map = behavior_map(&entries);
        assert!(map.get("zorp_write").expect("entry").may_change_state());
        assert!(!map.get("zorp_inspect").expect("entry").may_change_state());
        assert!(!map.get("zorp_write").expect("entry").is_empty());
    }
}
