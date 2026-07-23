//! mcp-server-skills: standalone MCP server for creating reusable skill files.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_skill

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Tool: create_skill
// ---------------------------------------------------------------------------

fn handle_create_skill(args: Value) -> Result<(String, bool)> {
    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| {
        eprintln!("FATAL: OMNI_DIR must be set");
        std::process::exit(1);
    });

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
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let create_skill_handler: ToolHandler = Box::new(|args: Value, _meta: Option<McpMeta>| {
        Box::pin(async move { handle_create_skill(args) })
    });

    let tools = vec![McpToolEntry {
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
    }];

    let server_info = ServerInfo {
        name: "mcp-server-skills".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
