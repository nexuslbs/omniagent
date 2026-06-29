//! mcp-server-memory — standalone MCP server for memory promotion, listing,
//! review, and management.
//!
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: promote_to_memory, list_memories, review_memories, manage_memory
//!
//! Memories are validated facts promoted from conversations to long-term wiki
//! storage. Each memory entry is a markdown file in:
//! `<data_dir>/profiles/<profile>/wiki/Memory/Promoted/<name>.md`
//!
//! Frontmatter fields:
//! - `type`: "memory"
//! - `confidence`: "high" | "medium" | "low"
//! - `source_message_ids`: [int] — message IDs that support this fact
//! - `source_tool_outputs`: [string] — tool call IDs that produced evidence
//! - `last_verified_at`: ISO timestamp
//! - `created_at`: ISO timestamp
//! - `expires_at`: ISO timestamp (default: 30 days)

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::db;
use serde_json::Value;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate an ISO timestamp string from the given offset.
fn iso_timestamp(offset_days: i64) -> String {
    let now = chrono::Utc::now();
    let ts = if offset_days == 0 {
        now
    } else {
        now + chrono::Duration::days(offset_days)
    };
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Validate confidence level string.
fn valid_confidence(s: &str) -> bool {
    matches!(s, "high" | "medium" | "low")
}

/// Sanitize a string for use as a filename (alphanumeric, hyphens, underscores).
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool: promote_to_memory
// ---------------------------------------------------------------------------

async fn handle_promote(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'content'"))?;
    let confidence = args["confidence"].as_str().unwrap_or("medium");

    if !valid_confidence(confidence) {
        anyhow::bail!(
            "Invalid confidence: '{}'. Must be one of: high, medium, low",
            confidence
        );
    }

    let source_message_ids: Vec<i64> = args["source_message_ids"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    let source_tool_outputs: Vec<String> = args["source_tool_outputs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let expires_in_days = args["expires_in_days"].as_i64().unwrap_or(30).max(1);
    let profile = args["profile"].as_str().unwrap_or("default");

    // Build the wiki path
    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);
    std::fs::create_dir_all(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to create memories directory: {}", e))?;

    let sanitized = sanitize_filename(name);
    if sanitized.is_empty() {
        anyhow::bail!("Name resulted in empty filename after sanitization");
    }

    let filepath = dir_path.join(format!("{}.md", sanitized));
    if filepath.exists() {
        anyhow::bail!(
            "Memory '{}' already exists at {}. Use a different name or review the existing entry.",
            name,
            filepath.display()
        );
    }

    let now_iso = iso_timestamp(0);
    let expires_iso = iso_timestamp(expires_in_days);

    let source_ids_json: String = source_message_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let source_tools_json: String = source_tool_outputs
        .iter()
        .map(|s| format!("\"{}\"", s))
        .collect::<Vec<_>>()
        .join(", ");

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
        confidence, source_ids_json, source_tools_json, now_iso, now_iso, expires_iso,
    );

    let full_content = format!("{}# Memory: {}\n\n{}", frontmatter, name, content);

    std::fs::write(&filepath, &full_content)
        .map_err(|e| anyhow::anyhow!("Failed to write memory file: {}", e))?;

    Ok((
        format!(
            "Memory '{}' promoted to wiki at {} (confidence: {}, expires: {})",
            name,
            filepath.display(),
            confidence,
            expires_iso
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_memories
// ---------------------------------------------------------------------------

async fn handle_list(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let profile = args["profile"].as_str().unwrap_or("default");
    let include_expired = args["include_expired"].as_bool().unwrap_or(false);

    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);

    if !dir_path.exists() {
        return Ok(("No promoted memories found.".to_string(), false));
    }

    let mut entries = Vec::new();
    let now_iso = iso_timestamp(0);

    for entry in std::fs::read_dir(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to read memories directory: {}", e))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let title = content
            .lines()
            .find(|l| l.starts_with("# Memory:"))
            .map(|l| l.trim_start_matches("# Memory:").trim())
            .unwrap_or(filename);

        let confidence = content
            .lines()
            .find(|l| l.starts_with("confidence:"))
            .map(|l| l.trim_start_matches("confidence:").trim())
            .unwrap_or("unknown");

        let expires_at = content
            .lines()
            .find(|l| l.starts_with("expires_at:"))
            .map(|l| l.trim_start_matches("expires_at:").trim())
            .unwrap_or("");

        let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();

        if is_expired && !include_expired {
            continue;
        }

        let status = if is_expired { "EXPIRED" } else { "active" };
        entries.push(format!(
            "- **{}** (confidence: {}, status: **{}**, expires: {})",
            title, confidence, status, expires_at
        ));
    }

    if entries.is_empty() {
        Ok(("No active promoted memories found.".to_string(), false))
    } else {
        let result = format!(
            "## Promoted Memories ({})\n\n{}",
            entries.len(),
            entries.join("\n")
        );
        Ok((result, false))
    }
}

// ---------------------------------------------------------------------------
// Tool: review_memories
// ---------------------------------------------------------------------------

async fn handle_review(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let profile = args["profile"].as_str().unwrap_or("default");
    let expiring_soon_days = args["expiring_soon_days"].as_i64().unwrap_or(7).max(1);

    let wiki_memories_dir = format!("{}/profiles/{}/wiki/Memory/Promoted", data_dir, profile);
    let dir_path = Path::new(&wiki_memories_dir);

    if !dir_path.exists() {
        return Ok(("No promoted memories to review.".to_string(), false));
    }

    let now_iso = iso_timestamp(0);
    let soon_iso = iso_timestamp(expiring_soon_days);

    let mut expired = Vec::new();
    let mut expiring_soon = Vec::new();
    let mut active = Vec::new();
    let mut total = 0u32;

    for entry in std::fs::read_dir(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to read memories directory: {}", e))?
    {
        let entry = entry.map_err(|e| anyhow::anyhow!("Failed to read entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        total += 1;

        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let title = content
            .lines()
            .find(|l| l.starts_with("# Memory:"))
            .map(|l| l.trim_start_matches("# Memory:").trim())
            .unwrap_or(filename);

        let confidence = content
            .lines()
            .find(|l| l.starts_with("confidence:"))
            .map(|l| l.trim_start_matches("confidence:").trim())
            .unwrap_or("unknown");

        let source_ids = content
            .lines()
            .find(|l| l.starts_with("source_message_ids:"))
            .map(|l| l.trim_start_matches("source_message_ids:").trim())
            .unwrap_or("[]");

        let expires_at = content
            .lines()
            .find(|l| l.starts_with("expires_at:"))
            .map(|l| l.trim_start_matches("expires_at:").trim())
            .unwrap_or("");

        let created_at = content
            .lines()
            .find(|l| l.starts_with("created_at:"))
            .map(|l| l.trim_start_matches("created_at:").trim())
            .unwrap_or("");

        let is_expired = !expires_at.is_empty() && expires_at < now_iso.as_str();
        let is_expiring_soon =
            !is_expired && !expires_at.is_empty() && expires_at < soon_iso.as_str();

        if is_expired {
            expired.push(format!(
                "- **{}** (confidence: {}, expired: {}, created: {}, sources: {})",
                title, confidence, expires_at, created_at, source_ids
            ));
        } else if is_expiring_soon {
            expiring_soon.push(format!(
                "- **{}** (confidence: {}, expires: {}, sources: {})",
                title, confidence, expires_at, source_ids
            ));
        } else {
            active.push(format!(
                "- **{}** (confidence: {}, expires: {})",
                title, confidence, expires_at
            ));
        }
    }

    let mut report = format!("# Memory Review Report\n\nTotal entries: **{}**\n\n", total);

    if !expired.is_empty() {
        report.push_str(&format!(
            "## ⚠️ Expired ({}):\n{}\n\n",
            expired.len(),
            expired.join("\n")
        ));
    }

    if !expiring_soon.is_empty() {
        report.push_str(&format!(
            "## ⏳ Expiring soon (within {} days) ({}):\n{}\n\n",
            expiring_soon_days,
            expiring_soon.len(),
            expiring_soon.join("\n")
        ));
    }

    if !active.is_empty() {
        report.push_str(&format!(
            "## ✅ Active ({}):\n{}\n\n",
            active.len(),
            active.join("\n")
        ));
    }

    report.push_str("### Recommended Actions:\n");
    if !expired.is_empty() {
        report.push_str("- **Renew**: Re-verify expired facts and call `promote_to_memory` with updated content\n");
    }
    if !expiring_soon.is_empty() {
        report.push_str("- **Review soon**: Check expiring memories for continued accuracy\n");
    }
    report.push_str("- **Keep**: Active memories are current and valid\n");

    Ok((report, false))
}

// ---------------------------------------------------------------------------
// Tool: manage_memory
// ---------------------------------------------------------------------------

const ENTRY_DELIMITER: &str = "\n§\n";

async fn handle_manage(data_dir: &str, args: &Value) -> Result<(String, bool)> {
    let target = args["target"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'target'"))?;
    let action = args["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'action'"))?;
    let content = args["content"].as_str().unwrap_or("");
    let profile = args["profile"].as_str().unwrap_or("default");

    let memories_dir = format!("{}/profiles/{}/memories", data_dir, profile);
    let dir_path = std::path::Path::new(&memories_dir);
    std::fs::create_dir_all(dir_path)
        .map_err(|e| anyhow::anyhow!("Failed to create memories directory: {}", e))?;

    let filename = match target {
        "memory" => "MEMORY.md",
        "user" => "USER.md",
        _ => anyhow::bail!("Invalid target '{}'. Must be 'memory' or 'user'", target),
    };
    let filepath = dir_path.join(filename);

    match action {
        "add" => {
            if content.is_empty() {
                anyhow::bail!("Content is required for 'add' action");
            }
            let existing = if filepath.exists() {
                std::fs::read_to_string(&filepath)
                    .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", filepath.display(), e))?
            } else {
                String::new()
            };
            let existing = existing.trim();
            let new_content = if existing.is_empty() {
                content.to_string()
            } else {
                format!("{}\n§\n{}", content, existing)
            };
            std::fs::write(&filepath, &new_content)
                .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", filepath.display(), e))?;
            Ok((
                format!(
                    "Entry added to {} (profile: {}). {} total chars.",
                    filename,
                    profile,
                    new_content.len()
                ),
                false,
            ))
        }
        "remove" => {
            if content.is_empty() {
                anyhow::bail!("Substring is required for 'remove' action to match entries");
            }
            if !filepath.exists() {
                return Ok((
                    format!("No {} file found — nothing to remove.", filename),
                    false,
                ));
            }
            let existing = std::fs::read_to_string(&filepath)
                .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", filepath.display(), e))?;
            let entries: Vec<String> = existing
                .split(ENTRY_DELIMITER)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let before = entries.len();
            let removed: Vec<&String> = entries.iter().filter(|e| e.contains(content)).collect();
            let removed_count = removed.len();
            let kept: Vec<&str> = entries
                .iter()
                .filter(|e| !e.contains(content))
                .map(|s| s.as_str())
                .collect();
            if kept.is_empty() {
                std::fs::remove_file(&filepath).map_err(|e| {
                    anyhow::anyhow!("Failed to remove {}: {}", filepath.display(), e)
                })?;
            } else {
                let new_content = kept.join(ENTRY_DELIMITER);
                std::fs::write(&filepath, &new_content).map_err(|e| {
                    anyhow::anyhow!("Failed to write {}: {}", filepath.display(), e)
                })?;
            }
            Ok((
                format!(
                    "Removed {}/{} entries from {} matching '{}'. {} remaining.",
                    removed_count,
                    before,
                    filename,
                    content,
                    kept.len()
                ),
                false,
            ))
        }
        "clean" => {
            if filepath.exists() {
                std::fs::remove_file(&filepath).map_err(|e| {
                    anyhow::anyhow!("Failed to remove {}: {}", filepath.display(), e)
                })?;
            }
            Ok((
                format!(
                    "{} cleared — all entries removed (profile: {}).",
                    filename, profile
                ),
                false,
            ))
        }
        _ => anyhow::bail!(
            "Invalid action '{}'. Must be 'add', 'remove', or 'clean'",
            action
        ),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let data_dir = std::env::var("OMNI_DATA_DIR").context("OMNI_DATA_DIR must be set")?;

    let _pool = db::connect(&database_url)
        .await
        .context("Failed to connect to database")?;

    // Wrap each handler to capture clones of data_dir
    let dd_promote = data_dir.clone();
    let promote_handler: ToolHandler = Box::new(move |args: Value| {
        Box::pin({
            let value = dd_promote.clone();
            async move { handle_promote(&value, &args).await }
        })
    });

    let dd_list = data_dir.clone();
    let list_handler: ToolHandler =
        Box::new(move |args: Value| Box::pin({
            let value = dd_list.clone();
            async move { handle_list(&value, &args).await }
        }));

    let dd_review = data_dir.clone();
    let review_handler: ToolHandler = Box::new(move |args: Value| {
        Box::pin({
            let value = dd_review.clone();
            async move { handle_review(&value, &args).await }
        })
    });

    let dd_manage = data_dir.clone();
    let manage_handler: ToolHandler = Box::new(move |args: Value| {
        Box::pin({
            let value = dd_manage.clone();
            async move { handle_manage(&value, &args).await }
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "promote_to_memory".to_string(),
                description:
                    "Promote a validated fact to long-term memory by writing it to the wiki. \
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
                            "description": "The validated fact(s) to store as memory. Be precise and concise."
                        },
                        "confidence": {
                            "type": "string",
                            "enum": ["high", "medium", "low"],
                            "description": "Confidence in the fact's accuracy"
                        },
                        "source_message_ids": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "description": "Message IDs that support this fact from the conversation"
                        },
                        "source_tool_outputs": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tool call IDs whose outputs provide evidence"
                        },
                        "expires_in_days": {
                            "type": "integer",
                            "description": "Days until this memory expires and needs review (default: 30)"
                        },
                        "profile": {
                            "type": "string",
                            "description": "Profile name for the wiki (default: 'default')"
                        }
                    },
                    "required": ["name", "content", "confidence"]
                }),
            },
            handler: promote_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_memories".to_string(),
                description:
                    "List all promoted memory entries in the wiki. Returns filenames, titles, \
                     confidence levels, and expiry dates for each memory."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        },
                        "include_expired": {
                            "type": "boolean",
                            "description": "Whether to include expired memories (default: false)"
                        }
                    }
                }),
            },
            handler: list_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "review_memories".to_string(),
                description:
                    "Review promoted memory entries for expiry, verifying factual accuracy. \
                     Returns a report of expired or soon-to-expire memories that need \
                     re-validation or renewal."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        },
                        "expiring_soon_days": {
                            "type": "integer",
                            "description": "Days threshold for 'expiring soon' warning (default: 7)"
                        }
                    }
                }),
            },
            handler: review_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "manage_memory".to_string(),
                description:
                    "Manage profile memory files (MEMORY.md and USER.md). Supports add, remove, and clean \
                     operations on the agent's persistent memory entries. Use on explicit user request only. \
                     'add' prepends a new entry, 'remove' deletes entries matching a substring, \
                     'clean' clears all entries."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "enum": ["memory", "user"],
                            "description": "Which file: 'memory' for MEMORY.md, 'user' for USER.md"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["add", "remove", "clean"],
                            "description": "Operation: 'add' prepends a new entry, 'remove' deletes entries matching substring, 'clean' clears all entries"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content for 'add' action. For 'remove', a substring to match against entries."
                        },
                        "profile": {
                            "type": "string",
                            "description": "Profile name (default: 'default')"
                        }
                    },
                    "required": ["target", "action"]
                }),
            },
            handler: manage_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-memory".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
