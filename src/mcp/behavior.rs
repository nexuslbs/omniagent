//! Tool behaviour descriptors (audit V-2).
//!
//! Core agent behaviour must never be driven by hardcoded tool-NAME
//! allowlists: a tool registered under another id, or a new read-only tool,
//! would silently lose its protection. Instead every tool MAY declare its
//! behaviour in its plugin manifest (`plugins/tools/<plugin>/plugin.json`):
//!
//! ```json
//! {
//!   "name": "Filesystem",
//!   "type": "mcp",
//!   "tools": [
//!     { "name": "filesystem_read",
//!       "behavior": { "read_only": true, "repeat_guard": true } },
//!     { "name": "manage_subtasks", "family": "subtasks" },
//!     { "name": "docker_compose", "behavior": { "affects_own_stack": true } }
//!   ]
//! }
//! ```
//!
//! The manifest is read by the plugin scanner, attached to the server config
//! and carried onto every registered tool, so the core agent loop can build
//!
//! * the guarded-read set (exact-repeat read guard),
//! * the own-stack set (self-restart guard), and
//! * the subtask family set (proactive subtask reminder)
//!
//! from the registry - never from a name allowlist.
//!
//! Fail CLOSED BUT LOUD: a tool without a descriptor is treated as
//! non-read-only (no guard) and, when its name still looks like a read, a
//! warning is emitted once per process so a missing manifest entry is
//! visible instead of silently allowlisted.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Declared behaviour of a single tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBehavior {
    /// The tool only READS state: an identical repeat call returns the same
    /// result. Also used by the prompt plugin to keep a generous excerpt of
    /// the result when compaction drains it (read results are the agent's
    /// working memory).
    #[serde(default)]
    pub read_only: bool,
    /// Whether the exact-repeat read guard applies. When omitted the guard
    /// follows `read_only`; `false` opts a read-only tool out of the guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_guard: Option<bool>,
    /// Coordination family (e.g. `subtasks`): the core loop treats every
    /// tool of the family as the same capability, so a renamed or moved tool
    /// stays recognised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// The tool can change the stack the agent itself runs in (containers,
    /// images, networks): the self-restart guard must run before it executes.
    #[serde(default)]
    pub affects_own_stack: bool,
}

impl ToolBehavior {
    /// Effective repeat-guard flag: an explicit value wins, otherwise a
    /// read-only tool is guarded.
    pub fn repeat_guard_enabled(&self) -> bool {
        self.repeat_guard.unwrap_or(self.read_only)
    }

    /// True when the entry declares no flag at all (declared but neutral).
    pub fn is_empty(&self) -> bool {
        !self.read_only
            && self.repeat_guard.is_none()
            && self.family.is_none()
            && !self.affects_own_stack
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
    pub repeat_guard: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affects_own_stack: Option<bool>,
}

impl ToolManifestEntry {
    /// Merge the nested + flat declarations into one effective behaviour.
    pub fn resolved(&self) -> ToolBehavior {
        let mut behavior = self.behavior.clone().unwrap_or_default();
        if let Some(v) = self.read_only {
            behavior.read_only = v;
        }
        if let Some(v) = self.repeat_guard {
            behavior.repeat_guard = Some(v);
        }
        if let Some(v) = &self.family {
            behavior.family = Some(v.clone());
        }
        if let Some(v) = self.affects_own_stack {
            behavior.affects_own_stack = v;
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

/// One warning per tool per process (the registry set is rebuilt on every
/// agent iteration, the log must not be flooded).
static WARNED_MISSING_DESCRIPTOR: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

/// True when a tool NAME looks like a read (used only for the loud warning,
/// never for the guard decision itself).
pub fn looks_like_read_tool(name: &str) -> bool {
    const READ_SEGMENTS: [&str; 9] = [
        "read", "list", "search", "info", "view", "get", "wiki", "notes", "skills",
    ];
    name.split(['_', '-'])
        .any(|segment| READ_SEGMENTS.contains(&segment))
}

/// Fail CLOSED BUT LOUD: a tool without a descriptor is never silently
/// treated as read-only - warn once per process when the name still looks
/// like a read so the missing manifest entry is visible.
pub fn warn_missing_descriptor(tool: &str) {
    if !looks_like_read_tool(tool) {
        return;
    }
    if !WARNED_MISSING_DESCRIPTOR.lock().insert(tool.to_string()) {
        return;
    }
    tracing::warn!(
        "tool '{}' has no behavior descriptor in its plugin manifest; treating it as \
         non-read-only (repeat guard disabled). Declare it, e.g. \
         \"tools\": [{{ \"name\": \"{}\", \"behavior\": {{ \"read_only\": true }} }}]",
        tool,
        tool
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_flat_declarations() {
        let json = serde_json::json!({
            "tools": [
                { "name": "filesystem_read",
                  "behavior": { "read_only": true, "repeat_guard": true } },
                { "name": "manage_subtasks", "family": "subtasks" },
                { "name": "docker_compose", "behavior": { "affects_own_stack": true } },
                { "name": "notes_note-read", "read_only": true }
            ]
        });
        let entries: Vec<ToolManifestEntry> =
            serde_json::from_value(json["tools"].clone()).expect("tools array parses");
        let map = behavior_map(&entries);

        let read = map.get("filesystem_read").expect("entry present");
        assert!(read.read_only);
        assert!(read.repeat_guard_enabled());

        let subtasks = map.get("manage_subtasks").expect("entry present");
        assert_eq!(subtasks.family.as_deref(), Some("subtasks"));
        assert!(!subtasks.repeat_guard_enabled());

        let docker = map.get("docker_compose").expect("entry present");
        assert!(docker.affects_own_stack);
        assert!(!docker.read_only);

        // Flat form: read_only at the top level, no nested behavior block.
        let flat = map.get("notes_note-read").expect("entry present");
        assert!(flat.read_only);
        assert!(flat.repeat_guard_enabled());
    }

    #[test]
    fn explicit_repeat_guard_false_opts_out() {
        let entry: ToolManifestEntry = serde_json::from_value(serde_json::json!({
            "name": "search_x",
            "behavior": { "read_only": true, "repeat_guard": false }
        }))
        .unwrap();
        let behavior = entry.resolved();
        assert!(behavior.read_only);
        assert!(!behavior.repeat_guard_enabled());
    }

    #[test]
    fn lookup_accepts_raw_and_qualified_names() {
        let mut map = ToolBehaviorMap::new();
        map.insert(
            "note_read".to_string(),
            ToolBehavior {
                read_only: true,
                ..Default::default()
            },
        );
        // Raw plugin name.
        assert!(for_tool(&map, "notes", "note_read").read_only);
        // Qualified registry name (plugin reports the bare tool name).
        assert!(for_tool(&map, "notes", "notes__note_read").read_only);
        // Legacy (pre-flip) qualified name: the one-release alias window.
        assert!(for_tool(&map, "notes", "notes_note-read").read_only);
        // Unknown tool: fail closed (no declared behaviour).
        assert!(!for_tool(&map, "notes", "notes_note-write").read_only);
    }

    #[test]
    fn read_looking_names_are_detected_for_the_loud_warning() {
        assert!(looks_like_read_tool("notes_note-list"));
        assert!(looks_like_read_tool("cron_list-cron-jobs"));
        assert!(looks_like_read_tool("search_thread-messages"));
        assert!(!looks_like_read_tool("docker_compose"));
        assert!(!looks_like_read_tool("git_commit-and-push"));
        assert!(!looks_like_read_tool("subtasks_manage-subtasks"));
    }
}
