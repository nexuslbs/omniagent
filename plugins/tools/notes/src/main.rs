//! mcp-server-notes: standalone MCP server for durable working-memory notes.
//!
//! Tools (registered under the `notes` server name → `notes_note-*`):
//! - `note-write`: overwrite a note file in the thread's notes dir
//! - `note-append`: append a line to a note file
//! - `note-read`: read a note file (context-*.json dumps are read-once, rule 12)
//! - `note-list`: list note files
//! - `note-rm`: remove a note file
//!
//! This is a SEPARATE plugin from the prompt plugin on purpose: the notes
//! toolset must be available regardless of which prompt plugin (builtin rust
//! or remote python) is enabled. Previously the note tools were registered
//! inside the prompt plugin under the `prompt_` server prefix, so switching
//! the prompt plugin to the remote omni-plugins implementation would have
//! silently stripped the agent's durable-note tools.
//!
//! Notes are written to `{omni_dir}/data/threads/{thread_id}/`. Config is
//! received from the omniagent via the `configure` message at startup.
//! Plugins never read env vars for config (user rule).

#![allow(dead_code, unused_imports)]

mod notes;

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Plugin-level config - received via configure message, never from env vars.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub omni_dir: String,
}

impl PluginConfig {
    fn default() -> Self {
        Self {
            omni_dir: String::new(),
        }
    }

    fn from_json(json: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(obj) = json.as_object() {
            if let Some(v) = obj.get("omni_dir").and_then(|v| v.as_str()) {
                cfg.omni_dir = v.to_string();
            }
        }
        cfg
    }
}

fn extract_i64(args: &Value, meta: &Option<McpMeta>, key: &str) -> Option<i64> {
    args[key].as_i64().or_else(|| {
        meta.as_ref().and_then(|m| match key {
            // channel ids are strings now (channel NAMES); notes uses thread_id only.
            "channel_id" => None,
            "thread_id" => m.thread_id,
            _ => None,
        })
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let plugin_config = Arc::new(RwLock::new(PluginConfig::default()));

    // WS-1: durable working-memory notes toolset (thread-dir sandboxed).
    let note_handler = |tool: &'static str| -> ToolHandler {
        let note_cfg = plugin_config.clone();
        Box::new(move |args: Value, meta: Option<McpMeta>| {
            let cfg = note_cfg.clone();
            Box::pin(async move {
                let config = cfg.read().await.clone();
                let omni_dir = notes::omni_dir_from(&config.omni_dir);
                let thread_id = match extract_i64(&args, &meta, "thread_id") {
                    Some(t) => t,
                    None => return Ok(("note tools require thread_id in _meta".to_string(), true)),
                };
                let dir = notes::thread_dir(&omni_dir, thread_id);
                let name = args["name"].as_str().unwrap_or("").to_string();
                let (content, is_error) = match tool {
                    "note_append" => {
                        notes::note_append(&dir, &name, args["content"].as_str().unwrap_or(""))
                    }
                    "note_read" => notes::note_read(&dir, &name, thread_id),
                    "note_write" => {
                        notes::note_write(&dir, &name, args["content"].as_str().unwrap_or(""))
                    }
                    "note_list" => notes::note_list(&dir),
                    "note_rm" => notes::note_rm(&dir, &name),
                    _ => (format!("unknown note tool {tool}"), true),
                };
                Ok((content, is_error))
            })
        })
    };

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "note_append".to_string(),
                description:
                    "Append a line to a durable working-memory note file in this thread's notes dir                      (data/threads/<thread_id>/). Notes survive compaction and thread death - the retry                      thread starts with them. Use for facts, paths, line numbers, commands, root causes,                      and decisions."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Note file name (plain filename, e.g. notes.md)"},
                        "content": {"type": "string", "description": "Content to append"}
                    },
                    "required": ["name", "content"]
                }),
            },
            handler: note_handler("note_append"),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "note_read".to_string(),
                description:
                    "Read a note file from this thread's notes dir. Output is capped at ~8KB. context-*.json                      dump files are READ-ONCE per thread: a second read returns a '[duplicate read ...]' marker                      (rule 12) instead of content."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Note file name (plain filename)"}
                    },
                    "required": ["name"]
                }),
            },
            handler: note_handler("note_read"),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "note_write".to_string(),
                description:
                    "Overwrite a note file in this thread's notes dir (creating it if needed). Use for the                      canonical notes.md working memory of this thread. Notes survive compaction and thread                      death - the retry thread starts with them."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Note file name (plain filename, e.g. notes.md)"},
                        "content": {"type": "string", "description": "Full content to write"}
                    },
                    "required": ["name", "content"]
                }),
            },
            handler: note_handler("note_write"),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "note_list".to_string(),
                description:
                    "List note files in this thread's notes dir (sorted)."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            handler: note_handler("note_list"),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "note_rm".to_string(),
                description:
                    "Remove a note file from this thread's notes dir."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Note file name (plain filename)"}
                    },
                    "required": ["name"]
                }),
            },
            handler: note_handler("note_rm"),
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-notes".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };

    // Use run_server_with_config so the omniagent can pass plugin config
    // via the configure message instead of env vars.
    let on_configure = {
        let cfg = plugin_config.clone();
        Some(move |params: Value| {
            let new_config = PluginConfig::from_json(&params);
            let cfg_c = cfg.clone();
            tokio::spawn(async move {
                let mut locked = cfg_c.write().await;
                *locked = new_config.clone();
                tracing::info!(
                    "Notes plugin configured: omni_dir present={}",
                    !new_config.omni_dir.is_empty()
                );
            });
        })
    };

    run_server_with_config(server_info, tools, on_configure).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses_omni_dir() {
        let cfg = PluginConfig::from_json(&serde_json::json!({
            "omni_dir": "/opt/omni"
        }));
        assert_eq!(cfg.omni_dir, "/opt/omni");
    }

    #[test]
    fn config_defaults_empty() {
        let cfg = PluginConfig::from_json(&serde_json::json!({}));
        assert_eq!(cfg.omni_dir, "");
    }
}
