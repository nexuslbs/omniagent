//! mcp-server-skills: standalone MCP server for creating, listing, and reading reusable skill files.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_skill, list_skills, view_skill

use anyhow::Result;
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

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

fn handle_skills_list(_args: Value, config: &Config, profile_name: &str) -> Result<(String, bool)> {
    let data_dir = &config.omni_dir;

    // Search both the global root and the profile-scoped root (the prompt
    // plugin lists skills from the profile root, so list must find them there).
    let mut skills_roots: Vec<std::path::PathBuf> = vec![Path::new(&data_dir).join("skills")];
    if !profile_name.is_empty() {
        let profile_root = Path::new(&data_dir)
            .join("profiles")
            .join(profile_name)
            .join("skills");
        if !skills_roots.contains(&profile_root) {
            skills_roots.push(profile_root);
        }
    }

    // Load usage data from the first root that has a .usage.json
    let mut usage_data: HashMap<String, Value> = HashMap::new();
    for root in &skills_roots {
        let usage_path = root.join(".usage.json");
        if let Ok(content) = fs::read_to_string(&usage_path) {
            if let Ok(data) = serde_json::from_str(&content) {
                usage_data = data;
                break;
            }
        }
    }

    let mut results: Vec<Value> = Vec::new();

    for skills_root in &skills_roots {
        if !skills_root.exists() {
            continue;
        }

        let mut root_entries: Vec<_> = fs::read_dir(skills_root)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to read skills directory '{}': {}",
                    skills_root.display(),
                    e
                )
            })?
            .filter_map(|e| e.ok())
            .collect();
        root_entries.sort_by_key(|e| e.file_name());

        // Pattern 3: flat <name>.md files directly in the skills root (no
        // category subdirectory — the profile-scoped layout). Handle them
        // first so they're listed even when no category dirs exist.
        for root_entry in &root_entries {
            let path = root_entry.path();
            let entry_name = root_entry.file_name().to_string_lossy().to_string();
            if entry_name.starts_with('.') {
                continue;
            }
            if path.is_file() && entry_name.ends_with(".md") {
                let stem = entry_name.strip_suffix(".md").unwrap();
                if !stem.is_empty() {
                    collect_skill_info(&path, stem, "", &usage_data, &mut results)?;
                }
            }
        }

        for category_entry in root_entries {
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
// Tool: view_skill
// ---------------------------------------------------------------------------

/// `view_skill`: read the full content of a skill by name.
///
/// The agent is told which skills are available (via the prompt plugin's
/// "Available skills" block), but without this tool it cannot actually READ
/// their content — skills were write-only (create) and metadata-only (list).
/// This closes the loop: the agent can load the procedure and follow it.
///
/// Accepts the skill name (case-insensitive, hyphens/underscores normalized),
/// optionally scoped by category. Both storage layouts are supported:
///   - `<skills>/<category>/<name>/SKILL.md` (directory layout)
///   - `<skills>/<category>/<name>.md` (flat file layout)
///
/// Skills may live under either the GLOBAL skills root (`{omni_dir}/skills`)
/// or the PROFILE-scoped root (`{omni_dir}/profiles/{profile}/skills` — where
/// the prompt plugin lists them from). Both are searched.
fn handle_view_skill(args: Value, config: &Config, profile_name: &str) -> Result<(String, bool)> {
    let data_dir = &config.omni_dir;

    let name_arg = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'name'"))?;
    if name_arg.trim().is_empty() {
        anyhow::bail!("Skill name must not be empty");
    }
    let category = args["category"].as_str().unwrap_or("");

    // Search both the global root and the profile-scoped root.
    let mut skills_roots: Vec<std::path::PathBuf> = vec![Path::new(&data_dir).join("skills")];
    if !profile_name.is_empty() {
        skills_roots.push(
            Path::new(&data_dir)
                .join("profiles")
                .join(profile_name)
                .join("skills"),
        );
    }

    // Normalize the requested name: lowercase, spaces → hyphens, strip a
    // trailing .md so both "docker-compose-usage" and "docker-compose-usage.md"
    // resolve.
    let requested = name_arg
        .trim()
        .to_lowercase()
        .replace(' ', "-")
        .trim_end_matches(".md")
        .to_string();

    let mut found: Option<(String, String)> = None; // (path, content)
    for skills_root in &skills_roots {
        if !skills_root.exists() {
            continue;
        }
        // Pattern 3: <root>/<name>.md — flat file directly in the skills
        // root (no category subdirectory; the profile-scoped layout used by
        // the prompt plugin).
        let root_flat = skills_root.join(format!("{}.md", requested));
        if root_flat.exists() {
            let content = fs::read_to_string(&root_flat).map_err(|e| {
                anyhow::anyhow!("Failed to read skill '{}': {}", root_flat.display(), e)
            })?;
            found = Some((root_flat.display().to_string(), content));
            break;
        }
        // If a category was given, only look there; otherwise scan all
        // category directories in this root.
        let category_dirs: Vec<String> = if !category.is_empty() {
            vec![category.to_string()]
        } else {
            let mut dirs: Vec<String> = fs::read_dir(skills_root)
                .map_err(|e| anyhow::anyhow!("Failed to read skills directory: {}", e))?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            dirs.sort();
            dirs
        };

        for cat in &category_dirs {
            if cat.starts_with('.') {
                continue;
            }
            let cat_path = skills_root.join(cat);
            // Pattern 1: <category>/<name>/SKILL.md
            let dir_candidate = cat_path.join(&requested).join("SKILL.md");
            if dir_candidate.exists() {
                let content = fs::read_to_string(&dir_candidate).map_err(|e| {
                    anyhow::anyhow!("Failed to read skill '{}': {}", dir_candidate.display(), e)
                })?;
                found = Some((dir_candidate.display().to_string(), content));
                break;
            }
            // Pattern 2: <category>/<name>.md
            let flat_candidate = cat_path.join(format!("{}.md", requested));
            if flat_candidate.exists() {
                let content = fs::read_to_string(&flat_candidate).map_err(|e| {
                    anyhow::anyhow!("Failed to read skill '{}': {}", flat_candidate.display(), e)
                })?;
                found = Some((flat_candidate.display().to_string(), content));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }

    let (path, content) = match found {
        Some(f) => f,
        None => {
            anyhow::bail!(
                "Skill '{}' not found under {} (searched global + profile '{}'). Use list_skills to see available skills.",
                name_arg,
                skills_roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                if profile_name.is_empty() {
                    "(none)".to_string()
                } else {
                    profile_name.to_string()
                }
            );
        }
    };

    // Truncate very large skills to keep the tool result manageable.
    const MAX_SKILL_CHARS: usize = 20_000;
    let display = if content.len() > MAX_SKILL_CHARS {
        format!(
            "{}\n\n[... truncated from {} to ~{} chars]",
            &content[..MAX_SKILL_CHARS],
            content.len(),
            MAX_SKILL_CHARS
        )
    } else {
        content
    };

    Ok((
        format!("--- Skill: {} (from {})\n\n{}", requested, path, display),
        false,
    ))
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
            let mut cfg = config.lock();
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
        })
    };

    let c1 = config.clone();
    let create_skill_handler: ToolHandler = Box::new(move |args: Value, _meta: Option<McpMeta>| {
        let c = c1.clone();
        Box::pin(async move {
            let config = c.lock().clone();
            handle_create_skill(args, &config)
        })
    });

    let c2 = config.clone();
    let list_skills_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c2.clone();
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let config = c.lock().clone();
            handle_skills_list(args, &config, &profile)
        })
    });

    let c3 = config.clone();
    let view_skill_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c3.clone();
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let config = c.lock().clone();
            handle_view_skill(args, &config, &profile)
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
        McpToolEntry {
            def: McpToolDef {
                name: "view_skill".to_string(),
                description:
                    "READ the full content of a skill by name. Use this when a skill is listed as available and you need its actual procedure/steps — e.g. before working in a workspace repo, running docker compose, or following a workflow. Accepts the skill name (case-insensitive; optionally scoped by category). Returns the complete SKILL.md content."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Skill name (e.g. 'docker-compose-usage', 'git-workflow')"
                        },
                        "category": {
                            "type": "string",
                            "description": "Optional category to scope the search (e.g. 'devops', 'software-development'). If omitted, all categories are searched."
                        }
                    },
                    "required": ["name"]
                }),
            },
            handler: view_skill_handler,
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-skills".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Config pointing at a temp omni dir with a flat-file skill.
    /// Each call gets a UNIQUE directory (tests run in parallel; sharing one
    /// dir would let one test's cleanup delete another test's fixtures).
    fn test_config() -> (Config, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "skills-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&dir);
        let skills_dir = dir.join("skills").join("software-development");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("docker-compose-usage.md"),
            "---\nname: docker-compose-usage\ndescription: \"Docker Compose Usage\"\n---\n\n# Docker Compose Usage\n\n1. Use `compose` tool with project_dir inside /opt/workspace.\n2. Never install packages inside running containers.\n",
        )
        .unwrap();
        // Directory layout skill (Pattern 1).
        let dir_skill = dir
            .join("skills")
            .join("devops")
            .join("git-workflow")
            .join("SKILL.md");
        fs::create_dir_all(dir_skill.parent().unwrap()).unwrap();
        fs::write(
            dir_skill,
            "---\nname: git-workflow\ndescription: \"Git Tool Usage\"\n---\n\n# Git Workflow\n\nPush to main only.\n",
        )
        .unwrap();
        // Profile-scoped skill (the layout the prompt plugin actually lists).
        let profile_skill = dir
            .join("profiles")
            .join("omni")
            .join("skills")
            .join("workspace-development.md");
        fs::create_dir_all(profile_skill.parent().unwrap()).unwrap();
        fs::write(
            &profile_skill,
            "---\nname: workspace-development\ndescription: \"Workspace Development with Docker\"\n---\n\n# Workspace Development\n\nNever install packages inside running containers.\n",
        )
        .unwrap();
        (
            Config {
                omni_dir: dir.to_string_lossy().to_string(),
            },
            dir,
        )
    }

    #[test]
    fn view_skill_flat_file_by_name() {
        let (cfg, dir) = test_config();
        let (msg, is_error) = handle_view_skill(
            serde_json::json!({"name": "docker-compose-usage"}),
            &cfg,
            "omni",
        )
        .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Docker Compose Usage"));
        assert!(msg.contains("Never install packages inside running containers"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_directory_layout() {
        let (cfg, dir) = test_config();
        let (msg, is_error) =
            handle_view_skill(serde_json::json!({"name": "git-workflow"}), &cfg, "omni")
                .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Git Workflow"));
        assert!(msg.contains("Push to main only"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_normalizes_name_and_category() {
        let (cfg, dir) = test_config();
        // Case + trailing .md normalization; category scoping works.
        let (msg, is_error) = handle_view_skill(
            serde_json::json!({"name": "Docker-Compose-Usage.md", "category": "software-development"}),
            &cfg,
            "omni",
        )
        .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("Docker Compose Usage"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_profile_scoped() {
        // Skills live under profiles/<profile>/skills — view_skill must find
        // them there (this is where the prompt plugin lists them from).
        let (cfg, dir) = test_config();
        let (msg, is_error) = handle_view_skill(
            serde_json::json!({"name": "workspace-development"}),
            &cfg,
            "omni",
        )
        .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Workspace Development"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_not_found() {
        let (cfg, dir) = test_config();
        let err = handle_view_skill(serde_json::json!({"name": "no-such-skill"}), &cfg, "omni")
            .expect_err("missing skill must error");
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains("list_skills"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_skills_finds_profile_scoped() {
        let (cfg, dir) = test_config();
        let (msg, is_error) = handle_skills_list(serde_json::json!({}), &cfg, "omni")
            .expect("list_skills should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("docker-compose-usage"));
        assert!(msg.contains("git-workflow"));
        assert!(msg.contains("workspace-development"));
        let _ = fs::remove_dir_all(&dir);
    }
}
