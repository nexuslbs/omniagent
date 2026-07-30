//! mcp-server-skills: standalone MCP server for creating and listing reusable skill files.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_skill, list_skills

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Tool: create_skill
// ---------------------------------------------------------------------------

fn handle_create_skill(args: Value, config: &Config) -> Result<(String, bool)> {
    let data_dir = &config.omni_dir;

    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    let description = args["description"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'description'"))?;
    let content = args["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'content'"))?;
    let category = args["category"].as_str().unwrap_or("general");

    // Validate name
    if name.is_empty() {
        anyhow::bail!("Skill name must not be empty");
    }
    if name.len() > 64 {
        anyhow::bail!(
            "Skill name must be 64 characters or less (got {})",
            name.len()
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Skill name must match pattern: lowercase alphanumeric, hyphens, underscores only"
        );
    }
    if description.is_empty() {
        anyhow::bail!("Skill description must not be empty");
    }
    if content.is_empty() {
        anyhow::bail!("Skill content must not be empty");
    }

    // Normalize name
    let normalized = name.to_lowercase().replace(' ', "-");

    // Build file path: <data_dir>/skills/<category>/SKILL.md
    let skill_dir = Path::new(&data_dir).join("skills").join(category);
    let skill_path = skill_dir.join(format!("{}.md", normalized));

    // Check if already exists
    if skill_path.exists() {
        anyhow::bail!(
            "Skill '{}' already exists at {}. Use a different name or category.",
            normalized,
            skill_path.display()
        );
    }

    // Create dirs
    fs::create_dir_all(&skill_dir).map_err(|e| {
        anyhow::anyhow!(
            "Failed to create skill directory '{}': {}",
            skill_dir.display(),
            e
        )
    })?;

    // Write the file
    let file_content = format!(
        "---\nname: {}\ndescription: \"{}\"\nversion: 0.1.0\nauthor: omniagent\n---\n\n{}",
        normalized, description, content
    );

    let safe_path = skill_path.to_string_lossy().to_string();
    fs::write(&skill_path, &file_content)
        .map_err(|e| anyhow::anyhow!("Failed to write skill file '{}': {}", safe_path, e))?;

    Ok((
        format!(
            "Skill '{}' created successfully at {}",
            normalized, safe_path
        ),
        false,
    ))
}

// ---------------------------------------------------------------------------
// Tool: list_skills
// ---------------------------------------------------------------------------

fn handle_skills_list(_args: Value, config: &Config) -> Result<(String, bool)> {
    let data_dir = &config.omni_dir;
    let skills_root = Path::new(&data_dir).join("skills");

    if !skills_root.exists() {
        return Ok(("[]".to_string(), false));
    }

    // Load usage data for enabled/disabled/archived state
    let usage_path = skills_root.join(".usage.json");
    let usage_data: HashMap<String, Value> = fs::read_to_string(&usage_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut results: Vec<Value> = Vec::new();

    let mut category_entries: Vec<_> = fs::read_dir(&skills_root)
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to read skills directory '{}': {}",
                skills_root.display(),
                e
            )
        })?
        .filter_map(|e| e.ok())
        .collect();
    category_entries.sort_by_key(|e| e.file_name());

    for category_entry in category_entries {
        let category_path = category_entry.path();
        let category_name = category_entry.file_name().to_string_lossy().to_string();

        // Skip hidden entries and non-directories
        if category_name.starts_with('.') || !category_path.is_dir() {
            continue;
        }

        let mut skill_entries: Vec<_> = fs::read_dir(&category_path)
            .map_err(|e| anyhow::anyhow!("Failed to read category '{}': {}", category_name, e))?
            .filter_map(|e| e.ok())
            .collect();
        skill_entries.sort_by_key(|e| e.file_name());

        for skill_entry in skill_entries {
            let skill_path = skill_entry.path();
            let entry_name = skill_entry.file_name().to_string_lossy().to_string();

            if entry_name.starts_with('.') {
                continue;
            }

            // Pattern 1: <skill-name>/SKILL.md (existing skill directory format)
            if skill_path.is_dir() {
                let skill_file = skill_path.join("SKILL.md");
                if skill_file.exists() {
                    collect_skill_info(
                        &skill_file,
                        &entry_name,
                        &category_name,
                        &usage_data,
                        &mut results,
                    )?;
                    continue;
                }
            }

            // Pattern 2: <name>.md flat file (for create_skill output format compatibility)
            if skill_path.is_file() && entry_name.ends_with(".md") {
                let stem = entry_name.strip_suffix(".md").unwrap();
                if !stem.is_empty() && !stem.starts_with('.') {
                    collect_skill_info(
                        &skill_path,
                        stem,
                        &category_name,
                        &usage_data,
                        &mut results,
                    )?;
                }
            }
        }
    }

    Ok((serde_json::to_string_pretty(&results)?, false))
}

/// Read a skill file, extract metadata, and push it into results.
fn collect_skill_info(
    skill_file: &Path,
    default_name: &str,
    category: &str,
    usage_data: &HashMap<String, Value>,
    results: &mut Vec<Value>,
) -> Result<()> {
    let metadata = fs::metadata(skill_file).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read metadata for '{}': {}",
            skill_file.display(),
            e
        )
    })?;
    let file_size = metadata.len();

    let content = fs::read_to_string(skill_file)
        .map_err(|e| anyhow::anyhow!("Failed to read '{}': {}", skill_file.display(), e))?;
    let line_count = content.lines().count();

    // Extract name from frontmatter, fall back to directory/file name
    let name =
        extract_frontmatter_field(&content, "name").unwrap_or_else(|| default_name.to_string());

    // Look up enabled status: try category/name first, then bare name
    let enabled = get_skill_enabled(usage_data, &name, category);

    results.push(serde_json::json!({
        "name": name,
        "category": category,
        "enabled": enabled,
        "file_size": file_size,
        "line_count": line_count,
    }));

    Ok(())
}

/// Extract a named field from YAML frontmatter (delimited by --- markers).
/// Returns `None` if no frontmatter or field not found.
fn extract_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let after_first = content.strip_prefix("---")?.trim_start();
    let end = after_first.find("\n---")?;
    let frontmatter = &after_first[..end];

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(&format!("{}:", field)) {
            let value = value.trim();
            // Strip surrounding quotes if present
            let unquoted = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            return Some(unquoted.to_string());
        }
    }
    None
}

/// Check if a skill is enabled by looking up its state in usage.json.
/// Returns true (enabled) by default if not found.
fn get_skill_enabled(usage_data: &HashMap<String, Value>, name: &str, category: &str) -> bool {
    // Try qualified key first (category/name), then bare name
    for key in &[format!("{}/{}", category, name), name.to_string()] {
        if let Some(entry) = usage_data.get(key) {
            if let Some(state) = entry.get("state").and_then(|v| v.as_str()) {
                return state == "active";
            }
        }
    }
    true // default to enabled
}

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    omni_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            omni_dir: "/opt/omni".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let config: Arc<Mutex<Config>> = Arc::new(Mutex::new(Config::default()));

    let on_configure = {
        let config = config.clone();
        Some(move |params: Value| {
            if let Ok(mut cfg) = config.lock() {
                if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                    if !dir.is_empty() {
                        cfg.omni_dir = dir.to_string();
                    }
                }
            }
        })
    };

    let c1 = config.clone();
    let create_skill_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let c = c1.clone();
        Box::pin(async move {
            let config = c.lock().unwrap().clone();
            handle_create_skill(args, &config)
        })
    });

    let c2 = config.clone();
    let list_skills_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let c = c2.clone();
        Box::pin(async move {
            let config = c.lock().unwrap().clone();
            handle_skills_list(args, &config)
        })
    });

    let tools = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "create_skill".to_string(),
                description:
                    "Create a new skill (SKILL.md file) for reusable procedures. Skills allow the agent to automate recurring task patterns. The skill is saved to <data_dir>/skills/<category>/<name>.md and will be available for future sessions."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name (lowercase, hyphens/underscores, max 64 chars)"
                        },
                        "description": {
                            "type": "string",
                            "description": "Brief description of what the skill does"
                        },
                        "content": {
                            "type": "string",
                            "description": "Full markdown body of the skill (steps, verification, etc.)"
                        },
                        "category": {
                            "type": "string",
                            "description": "Optional category for organizing (e.g., 'devops', 'data-science'). Default: 'general'"
                        }
                    },
                    "required": ["name", "description", "content"]
                }),
            },
            handler: create_skill_handler,
        },
        McpToolEntry {
            def: McpToolDef {
                name: "list_skills".to_string(),
                description:
                    "List all available skills with metadata: name, category, enabled/disabled status, file size, and line count. Reads skills from <data_dir>/skills/<category>/<skill-name>/SKILL.md and checks .usage.json for enabled state."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            handler: list_skills_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-skills".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}
