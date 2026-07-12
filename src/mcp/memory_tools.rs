//! Built-in memory MCP tools — manage_memory, promote/list/review memories.
//!
//! These provide always-available memory management without a subprocess dependency.

use crate::mcp::{AppContext, McpTool, McpToolResult};
use serde_json::Value;
use std::sync::Arc;

const ENTRY_DELIMITER: &str = "\n§\n";

// ---------------------------------------------------------------------------
// Helper: resolve profile + memories dir from args or context
// ---------------------------------------------------------------------------

fn resolve_profile(_ctx: &AppContext, args: &Value) -> String {
    let default = crate::profile::default_profile_name();
    args["profile"].as_str().unwrap_or(&default).to_string()
}

fn memories_dir(ctx: &AppContext, profile: &str) -> String {
    format!("{}/profiles/{}/memories", ctx.data_dir, profile)
}

// ---------------------------------------------------------------------------
// Tool: manage_memory
// ---------------------------------------------------------------------------

fn manage_memory_tool() -> McpTool {
    McpTool {
        name: "manage_memory".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "manage_memory"),
        description: "Manage persistent memory entries (MEMORY.md and USER.md). \
                      Actions: 'add' (prepend entry), 'remove' (entries matching substring), \
                      'clean' (remove all entries). \
                      The memory section is injected into the system prompt on every session. \
                      Use 'add' to save new facts, 'remove' to prune stale entries, \
                      and 'clean' to wipe the section entirely."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["memory", "user"],
                    "description": "Which file to manage: 'memory' (MEMORY.md — agent notes) or 'user' (USER.md — user profile)"
                },
                "action": {
                    "type": "string",
                    "enum": ["add", "remove", "clean"],
                    "description": "Action: 'add' prepends a new entry, 'remove' removes entries containing the substring, 'clean' wipes all entries"
                },
                "content": {
                    "type": "string",
                    "description": "For 'add': the entry content to prepend. For 'remove': substring to match for removal."
                },
                "profile": {
                    "type": "string",
                    "description": "Profile name (default: default)"
                }
            },
            "required": ["target", "action"]
        }),
        server_name: None,
        timeout_secs: crate::mcp::DEFAULT_TOOL_TIMEOUT_SECS,
        watchdog: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let target = match args["target"].as_str() {
                    Some("memory") => "MEMORY.md",
                    Some("user") => "USER.md",
                    Some(t) => return Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Invalid target '{}'. Must be 'memory' or 'user'", t),
                        is_error: true,
                    }),
                    None => return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Missing required argument: 'target'".to_string(),
                        is_error: true,
                    }),
                };

                let action = match args["action"].as_str() {
                    Some(a) => a,
                    None => return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Missing required argument: 'action'".to_string(),
                        is_error: true,
                    }),
                };

                let profile = resolve_profile(&ctx, &args);
                let dir = memories_dir(&ctx, &profile);
                let path = std::path::Path::new(&dir).join(target);

                std::fs::create_dir_all(&dir).ok();

                match action {
                    "add" => {
                        let content = args["content"].as_str().unwrap_or("");
                        if content.is_empty() {
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: "Content is required for 'add' action".to_string(),
                                is_error: true,
                            });
                        }
                        let existing = if path.exists() {
                            std::fs::read_to_string(&path).unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let existing = existing.trim();
                        let new_content = if existing.is_empty() {
                            content.to_string()
                        } else {
                            format!("{}\n§\n{}", content, existing)
                        };
                        std::fs::write(&path, &new_content).map_err(|e| {
                            crate::error::Error::Message(format!("Failed to write {}: {}", target, e))
                        })?;
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("Entry added to {} (profile: {}). {} total chars.",
                                target, profile, new_content.len()),
                            is_error: false,
                        })
                    }
                    "remove" => {
                        let substring = args["content"].as_str().unwrap_or("");
                        if substring.is_empty() {
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: "Substring is required for 'remove' action to match entries".to_string(),
                                is_error: true,
                            });
                        }
                        if !path.exists() {
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!("No {} file found — nothing to remove.", target),
                                is_error: false,
                            });
                        }
                        let existing = std::fs::read_to_string(&path).unwrap_or_default();
                        let entries: Vec<String> = existing
                            .split(ENTRY_DELIMITER)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        let before = entries.len();
                        let removed_count = entries.iter().filter(|e| e.contains(substring)).count();
                        let kept: Vec<&str> = entries
                            .iter()
                            .filter(|e| !e.contains(substring))
                            .map(|s| s.as_str())
                            .collect();
                        if kept.is_empty() {
                            std::fs::remove_file(&path).ok();
                        } else {
                            let new_content = kept.join(ENTRY_DELIMITER);
                            std::fs::write(&path, &new_content).ok();
                        }
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("Removed {}/{} entries from {} matching '{}'. {} remaining.",
                                removed_count, before, target, substring, kept.len()),
                            is_error: false,
                        })
                    }
                    "clean" => {
                        if path.exists() {
                            std::fs::remove_file(&path).ok();
                        }
                        Ok(McpToolResult {
                            call_id: String::new(),
                            content: format!("{} cleared — all entries removed (profile: {}).", target, profile),
                            is_error: false,
                        })
                    }
                    _ => Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Invalid action '{}'. Must be 'add', 'remove', or 'clean'", action),
                        is_error: true,
                    }),
                }
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool: promote_to_memory
// ---------------------------------------------------------------------------

fn promote_to_memory_tool() -> McpTool {
    McpTool {
        name: "promote_to_memory".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "promote_to_memory"),
        description: "Promote a validated fact to long-term memory by writing it to the wiki. \
                      Memories are stored as markdown files under Memory/Promoted/ with frontmatter \
                      containing provenance, confidence, and expiry information. \
                      Only promote facts that have been directly validated through conversation or tool output. \
                      The memory becomes available for future retrieval via wiki search."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short, descriptive name for the memory (used as filename)"
                },
                "content": {
                    "type": "string",
                    "description": "The validated fact(s) to store as memory"
                },
                "confidence": {
                    "type": "string",
                    "enum": ["high", "medium", "low"],
                    "description": "Confidence in the fact's accuracy"
                },
                "source_message_ids": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Message IDs that support this fact"
                },
                "source_tool_outputs": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tool call IDs whose outputs provide evidence"
                },
                "expires_in_days": {
                    "type": "integer",
                    "description": "Days until this memory expires (default: 30)"
                },
                "profile": {
                    "type": "string",
                    "description": "Profile name (default: default)"
                }
            },
            "required": ["name", "content", "confidence"]
        }),
        server_name: None,
        timeout_secs: crate::mcp::DEFAULT_TOOL_TIMEOUT_SECS,
        watchdog: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let name = match args["name"].as_str() {
                    Some(n) => n,
                    None => return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Missing required argument: 'name'".to_string(),
                        is_error: true,
                    }),
                };
                let content = match args["content"].as_str() {
                    Some(c) => c,
                    None => return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Missing required argument: 'content'".to_string(),
                        is_error: true,
                    }),
                };
                let confidence = args["confidence"].as_str().unwrap_or("medium");
                if !matches!(confidence, "high" | "medium" | "low") {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Invalid confidence: '{}'. Must be: high, medium, low", confidence),
                        is_error: true,
                    });
                }

                let source_message_ids: Vec<i64> = args["source_message_ids"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                    .unwrap_or_default();
                let source_tool_outputs: Vec<String> = args["source_tool_outputs"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let expires_in_days = args["expires_in_days"].as_i64().unwrap_or(30).max(1);
                let profile = resolve_profile(&ctx, &args);

                let wiki_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", ctx.data_dir, profile);
                std::fs::create_dir_all(&wiki_dir).ok();

                let sanitized: String = name.chars()
                    .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                    .collect();
                if sanitized.is_empty() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "Name resulted in empty filename after sanitization".to_string(),
                        is_error: true,
                    });
                }

                let filepath = std::path::Path::new(&wiki_dir).join(format!("{}.md", sanitized));
                if filepath.exists() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: format!("Memory '{}' already exists at {}. Use a different name or review the existing entry.",
                            name, filepath.display()),
                        is_error: true,
                    });
                }

                let now = chrono::Utc::now();
                let now_iso = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
                let expires_iso = (now + chrono::Duration::days(expires_in_days))
                    .format("%Y-%m-%dT%H:%M:%SZ").to_string();

                let source_ids_str: Vec<String> = source_message_ids.iter().map(|id| id.to_string()).collect();
                let source_tools_str: Vec<String> = source_tool_outputs.iter().map(|s| format!("\"{}\"", s)).collect();

                let frontmatter = format!(
                    r#"---
type: memory
confidence: {}
source_message_ids: [{}]
source_tool_outputs: [{}]
last_verified_at: {}
created_at: {}
expires_at: {}
---"#,
                    confidence,
                    source_ids_str.join(", "),
                    source_tools_str.join(", "),
                    now_iso, now_iso, expires_iso,
                );

                let full_content = format!("{}# Memory: {}\n\n{}", frontmatter, name, content);
                std::fs::write(&filepath, &full_content).map_err(|e| {
                    crate::error::Error::Message(format!("Failed to write memory file: {}", e))
                })?;

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: format!("Memory '{}' promoted to wiki at {} (confidence: {}, expires: {})",
                        name, filepath.display(), confidence, expires_iso),
                    is_error: false,
                })
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool: list_memories
// ---------------------------------------------------------------------------

fn list_memories_tool() -> McpTool {
    McpTool {
        name: "list_memories".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "list_memories"),
        description: "List all promoted memories with their title, confidence, and expiry status. \
                      Optionally include expired entries. Reads from the wiki Memory/Promoted directory."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "include_expired": {
                    "type": "boolean",
                    "description": "Whether to include expired memories (default: false)"
                },
                "profile": {
                    "type": "string",
                    "description": "Profile name (default: default)"
                }
            },
            "required": []
        }),
        server_name: None,
        timeout_secs: crate::mcp::DEFAULT_TOOL_TIMEOUT_SECS,
        watchdog: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let profile = resolve_profile(&ctx, &args);
                let include_expired = args["include_expired"].as_bool().unwrap_or(false);
                let wiki_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", ctx.data_dir, profile);
                let dir_path = std::path::Path::new(&wiki_dir);

                if !dir_path.exists() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "No promoted memories found.".to_string(),
                        is_error: false,
                    });
                }

                let now_iso = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                let mut entries = Vec::new();

                for entry in std::fs::read_dir(dir_path).map_err(|e| {
                    crate::error::Error::Message(format!("Failed to read memories directory: {}", e))
                })? {
                    let entry = entry.map_err(|e| {
                        crate::error::Error::Message(format!("Failed to read entry: {}", e))
                    })?;
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }

                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");

                    let title = content.lines()
                        .find(|l| l.starts_with("# Memory:"))
                        .map(|l| l.trim_start_matches("# Memory:").trim())
                        .unwrap_or(filename);
                    let confidence = content.lines()
                        .find(|l| l.starts_with("confidence:"))
                        .map(|l| l.trim_start_matches("confidence:").trim())
                        .unwrap_or("unknown");
                    let expires_at = content.lines()
                        .find(|l| l.starts_with("expires_at:"))
                        .map(|l| l.trim_start_matches("expires_at:").trim())
                        .unwrap_or("");

                    let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();
                    if is_expired && !include_expired {
                        continue;
                    }

                    let status = if is_expired { "EXPIRED" } else { "active" };
                    entries.push(format!("- **{}** (confidence: {}, status: **{}**, expires: {})",
                        title, confidence, status, expires_at));
                }

                if entries.is_empty() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "No active promoted memories found.".to_string(),
                        is_error: false,
                    });
                }

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: format!("## Promoted Memories ({})\n\n{}", entries.len(), entries.join("\n")),
                    is_error: false,
                })
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool: review_memories
// ---------------------------------------------------------------------------

fn review_memories_tool() -> McpTool {
    McpTool {
        name: "review_memories".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "review_memories"),
        description: "Review the status of all promoted memories, categorizing them as expired, \
                      expiring soon, or active. Provides a summary report with recommended actions."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "expiring_soon_days": {
                    "type": "integer",
                    "description": "Days threshold for 'expiring soon' (default: 7)"
                },
                "profile": {
                    "type": "string",
                    "description": "Profile name (default: default)"
                }
            },
            "required": []
        }),
        server_name: None,
        timeout_secs: crate::mcp::DEFAULT_TOOL_TIMEOUT_SECS,
        watchdog: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let profile = resolve_profile(&ctx, &args);
                let expiring_soon_days = args["expiring_soon_days"].as_i64().unwrap_or(7).max(1);
                let wiki_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", ctx.data_dir, profile);
                let dir_path = std::path::Path::new(&wiki_dir);

                if !dir_path.exists() {
                    return Ok(McpToolResult {
                        call_id: String::new(),
                        content: "No promoted memories to review.".to_string(),
                        is_error: false,
                    });
                }

                let now = chrono::Utc::now();
                let now_iso = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
                let soon_iso = (now + chrono::Duration::days(expiring_soon_days))
                    .format("%Y-%m-%dT%H:%M:%SZ").to_string();

                let mut expired = Vec::new();
                let mut expiring_soon = Vec::new();
                let mut active_list = Vec::new();
                let mut total = 0u32;

                for entry in std::fs::read_dir(dir_path).map_err(|e| {
                    crate::error::Error::Message(format!("Failed to read directory: {}", e))
                })? {
                    let entry = entry.map_err(|e| {
                        crate::error::Error::Message(format!("Failed to read entry: {}", e))
                    })?;
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }

                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    total += 1;
                    let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");

                    let title = content.lines()
                        .find(|l| l.starts_with("# Memory:"))
                        .map(|l| l.trim_start_matches("# Memory:").trim())
                        .unwrap_or(filename);
                    let confidence = content.lines()
                        .find(|l| l.starts_with("confidence:"))
                        .map(|l| l.trim_start_matches("confidence:").trim())
                        .unwrap_or("unknown");
                    let source_ids = content.lines()
                        .find(|l| l.starts_with("source_message_ids:"))
                        .map(|l| l.trim_start_matches("source_message_ids:").trim())
                        .unwrap_or("[]");
                    let expires_at = content.lines()
                        .find(|l| l.starts_with("expires_at:"))
                        .map(|l| l.trim_start_matches("expires_at:").trim())
                        .unwrap_or("");
                    let created_at = content.lines()
                        .find(|l| l.starts_with("created_at:"))
                        .map(|l| l.trim_start_matches("created_at:").trim())
                        .unwrap_or("");

                    let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();
                    let is_expiring_soon = !is_expired && !expires_at.is_empty() && expires_at < soon_iso.as_str();

                    if is_expired {
                        expired.push(format!("- **{}** (confidence: {}, expired: {}, created: {}, sources: {})",
                            title, confidence, expires_at, created_at, source_ids));
                    } else if is_expiring_soon {
                        expiring_soon.push(format!("- **{}** (confidence: {}, expires: {}, sources: {})",
                            title, confidence, expires_at, source_ids));
                    } else {
                        active_list.push(format!("- **{}** (confidence: {}, expires: {})",
                            title, confidence, expires_at));
                    }
                }

                let mut report = format!("# Memory Review Report\n\nTotal entries: **{}**\n\n", total);
                if !expired.is_empty() {
                    report.push_str(&format!("## ⚠️ Expired ({}):\n{}\n\n", expired.len(), expired.join("\n")));
                }
                if !expiring_soon.is_empty() {
                    report.push_str(&format!("## ⏳ Expiring soon (within {} days) ({}):\n{}\n\n",
                        expiring_soon_days, expiring_soon.len(), expiring_soon.join("\n")));
                }
                if !active_list.is_empty() {
                    report.push_str(&format!("## ✅ Active ({}):\n{}\n\n", active_list.len(), active_list.join("\n")));
                }
                report.push_str("### Recommended Actions:\n");
                if !expired.is_empty() {
                    report.push_str("- **Renew**: Re-verify expired facts and call `promote_to_memory` with updated content\n");
                }
                if !expiring_soon.is_empty() {
                    report.push_str("- **Review soon**: Check expiring memories for continued accuracy\n");
                }
                report.push_str("- **Keep**: Active memories are current and valid\n");

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: report,
                    is_error: false,
                })
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// Return all built-in memory tools for registration in the MCP registry.
pub fn all_memory_tools() -> Vec<McpTool> {
    vec![
        manage_memory_tool(),
        promote_to_memory_tool(),
        list_memories_tool(),
        review_memories_tool(),
    ]
}