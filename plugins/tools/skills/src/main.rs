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
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Tool: create_skill
// ---------------------------------------------------------------------------

fn handle_create_skill(args: Value, config: &Config, profile_name: &str) -> Result<(String, bool)> {
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
    let tags = args["tags"].as_str().unwrap_or("");
    let related_skills = args["related_skills"].as_str().unwrap_or("");

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
    if description.chars().count() > 1024 {
        anyhow::bail!(
            "Skill description must be 1024 characters or less (got {} chars). Use the 'Use when <trigger>' convention to keep descriptions actionable.",
            description.chars().count()
        );
    }
    if content.is_empty() {
        anyhow::bail!("Skill content must not be empty");
    }

    // Normalize name
    let normalized = name.to_lowercase().replace(' ', "-");

    // Build file path: <data_dir>/skills/<category>/<name>.md for the global
    // root, or <data_dir>/profiles/<profile>/skills/<category>/<name>.md when
    // a profile is active. The PROFILE root is what the prompt plugin lists
    // as "Available skills" in every system prompt - writing there makes the
    // new skill immediately visible to the agent. (A global-root write would
    // be findable via list_skills/view_skill but would NOT show up in the
    // agent's own prompt, so the agent would never know it exists.)
    let profile = profile_name.trim().to_string();
    let skills_base = if profile.is_empty() {
        Path::new(&data_dir).join("skills")
    } else {
        Path::new(&data_dir)
            .join("profiles")
            .join(&profile)
            .join("skills")
    };
    let skill_dir = skills_base.join(category).join(&normalized);
    let skill_path = skill_dir.join("SKILL.md");

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

    // Write the file - Hermes-compatible SKILL.md with rich frontmatter:
    // description follows the "Use when <trigger>..." convention (prepended
    // when missing) so the prompt block renders an actionable trigger; license
    // MIT; optional metadata.hermes tags / related_skills from args.
    let use_when = if description.trim().to_lowercase().starts_with("use when") {
        description.trim().to_string()
    } else {
        format!("Use when {}", description.trim())
    };
    let escaped_desc = use_when.replace('"', "\\\"");
    let mut frontmatter = format!(
        "---\nname: {}\ndescription: \"{}\"\nversion: 0.1.0\nauthor: omniagent\nlicense: MIT\n",
        normalized, escaped_desc
    );
    let tag_list: Vec<String> = tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let related_list: Vec<String> = related_skills
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if !tag_list.is_empty() || !related_list.is_empty() {
        frontmatter.push_str("metadata:\n  hermes:\n");
        if !tag_list.is_empty() {
            frontmatter.push_str("    tags:\n");
            for tag in &tag_list {
                frontmatter.push_str(&format!("      - {}\n", tag));
            }
        }
        if !related_list.is_empty() {
            frontmatter.push_str("    related_skills:\n");
            for rs in &related_list {
                frontmatter.push_str(&format!("      - {}\n", rs));
            }
        }
    }
    frontmatter.push_str("---\n\n");

    let file_content = format!("{}{}", frontmatter, content);

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
        // category subdirectory - the profile-scoped layout). Handle them
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
/// their content - skills were write-only (create) and metadata-only (list).
/// This closes the loop: the agent can load the procedure and follow it.
///
/// Accepts the skill name (case-insensitive, hyphens/underscores normalized),
/// optionally scoped by category. Every layout list_skills can enumerate is
/// supported:
///   - `<skills>/<name>/SKILL.md`            (root-level skill directory)
///   - `<skills>/<name>.md`                  (root-level flat file)
///   - `<skills>/<category>/<name>/SKILL.md` (directory layout)
///   - `<skills>/<category>/<name>.md`       (flat file layout)
///
/// Skills may live under either the GLOBAL skills root (`{omni_dir}/skills`)
/// or the PROFILE-scoped root (`{omni_dir}/profiles/{profile}/skills` - where
/// the prompt plugin lists them from). Both are searched, and a candidate
/// matches by its frontmatter `name` (what list_skills reports) or by its
/// directory/file name.
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

    // Normalize the requested name: lowercase, spaces to hyphens, strip a
    // trailing .md so both "docker-compose-usage" and "docker-compose-usage.md"
    // resolve.
    let requested = normalize_skill_name(name_arg);

    let mut found: Option<(String, String)> = None; // (path, content)
    for skills_root in &skills_roots {
        if !skills_root.exists() {
            continue;
        }
        for (skill_file, skill_category, skill_name, path_name) in
            enumerate_root_skills(skills_root)?
        {
            // Optional category scoping: when the caller passes the category
            // list_skills reported, honour it (list reports the parent dir as
            // category for root-level skill dirs too).
            if !category.is_empty()
                && normalize_skill_name(&skill_category) != normalize_skill_name(category)
            {
                continue;
            }
            let requested_matches = normalize_skill_name(&skill_name) == requested
                || normalize_skill_name(&path_name) == requested;
            if !requested_matches {
                continue;
            }
            let content = fs::read_to_string(&skill_file).map_err(|e| {
                anyhow::anyhow!("Failed to read skill '{}': {}", skill_file.display(), e)
            })?;
            found = Some((skill_file.display().to_string(), content));
            break;
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

/// Normalize a skill name for comparison: lowercase, spaces to hyphens, and a
/// trailing ".md" removed (so "name", "Name" and "name.md" all resolve alike).
fn normalize_skill_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .replace(' ', "-")
        .trim_end_matches(".md")
        .to_string()
}

/// Return the name a skill file is known by: the frontmatter `name:` field
/// when present (what list_skills reports), otherwise the parent directory
/// name for a `<name>/SKILL.md` layout or the file stem for a `<name>.md`
/// layout.
fn skill_display_name(skill_file: &Path) -> String {
    if let Ok(content) = fs::read_to_string(skill_file) {
        if let Some(front) = extract_frontmatter_field(&content, "name") {
            if !front.trim().is_empty() {
                return front;
            }
        }
    }
    path_skill_name(skill_file)
}

/// Directory/file based name of a skill file, ignoring frontmatter.
fn path_skill_name(skill_file: &Path) -> String {
    let file_name = skill_file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    if file_name.eq_ignore_ascii_case("SKILL.md") {
        skill_file
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string()
    } else {
        file_name
            .strip_suffix(".md")
            .unwrap_or(&file_name)
            .to_string()
    }
}

/// Enumerate every skill file under one skills root, mirroring exactly the
/// layouts list_skills discovers, INCLUDING the root-level skill-directory
/// layout `<root>/<name>/SKILL.md` that the profile skills roots use (e.g.
/// `{omni_dir}/profiles/omni/skills/remote-development/SKILL.md`), which the
/// old view_skill path guessing never searched.
///
/// Returns (skill_file_path, category, display_name, path_name). For a
/// root-level `<name>/SKILL.md` directory, `category` is the directory name
/// (what list_skills reports for it) and `path_name` is the directory name.
fn enumerate_root_skills(skills_root: &Path) -> Result<Vec<(PathBuf, String, String, String)>> {
    let mut out: Vec<(PathBuf, String, String, String)> = Vec::new();
    if !skills_root.exists() {
        return Ok(out);
    }
    let mut root_entries: Vec<PathBuf> = fs::read_dir(skills_root)
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to read skills directory '{}': {}",
                skills_root.display(),
                e
            )
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    root_entries.sort();

    for root_entry in root_entries {
        let entry_name = root_entry
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if entry_name.starts_with('.') {
            continue;
        }
        if root_entry.is_file() && entry_name.ends_with(".md") {
            // Root-level flat file: <root>/<name>.md
            out.push((
                root_entry.clone(),
                String::new(),
                skill_display_name(&root_entry),
                path_skill_name(&root_entry),
            ));
            continue;
        }
        if !root_entry.is_dir() {
            continue;
        }
        let direct_skill = root_entry.join("SKILL.md");
        if direct_skill.is_file() {
            // Root-level skill directory: <root>/<name>/SKILL.md (the layout
            // profile skills roots use; list_skills reports the dir name as
            // the category).
            let display = skill_display_name(&direct_skill);
            out.push((direct_skill, entry_name.clone(), display, entry_name));
            continue;
        }
        // Category directory: scan its children (Pattern 1 + Pattern 2).
        let category_name = entry_name;
        let mut children: Vec<PathBuf> = fs::read_dir(&root_entry)
            .map_err(|e| anyhow::anyhow!("Failed to read category '{}': {}", category_name, e))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        children.sort();
        for child in children {
            let child_name = child
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if child_name.starts_with('.') {
                continue;
            }
            if child.is_dir() {
                // Pattern 1: <category>/<name>/SKILL.md
                let skill_file = child.join("SKILL.md");
                if skill_file.exists() {
                    let display = skill_display_name(&skill_file);
                    out.push((
                        skill_file,
                        category_name.clone(),
                        display,
                        child_name.clone(),
                    ));
                }
            } else if child.is_file() && child_name.ends_with(".md") {
                // Pattern 2: <category>/<name>.md flat file
                let stem = child_name.strip_suffix(".md").unwrap();
                if !stem.is_empty() {
                    out.push((
                        child.clone(),
                        category_name.clone(),
                        skill_display_name(&child),
                        stem.to_string(),
                    ));
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Plugin config - received via MCP configure message, not from env vars
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
    let create_skill_handler: ToolHandler = Box::new(move |args: Value, meta: Option<McpMeta>| {
        let c = c1.clone();
        let profile = meta
            .as_ref()
            .and_then(|m| m.profile_name.clone())
            .unwrap_or_default();
        Box::pin(async move {
            let config = c.lock().clone();
            handle_create_skill(args, &config, &profile)
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
                    "Create a new skill (SKILL.md file) for reusable procedures. Skills allow the agent to automate recurring task patterns. The skill is saved to the ACTIVE PROFILE's skills dir in the Hermes layout (<data_dir>/profiles/<profile>/skills/<category>/<name>/SKILL.md) so it shows up in the agent's own 'Available skills' prompt; with no active profile it falls back to <data_dir>/skills/<category>/<name>/SKILL.md. Frontmatter follows Hermes conventions: description (max 1024 chars) is prefixed with 'Use when ' when missing, license is MIT, and optional comma-separated tags / related_skills land under metadata.hermes. It will be available for future sessions."
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
                        },
                        "tags": {
                            "type": "string",
                            "description": "Optional comma-separated tags for the skill (stored under metadata.hermes.tags)"
                        },
                        "related_skills": {
                            "type": "string",
                            "description": "Optional comma-separated names of related skills (stored under metadata.hermes.related_skills)"
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
                    "READ the full content of a skill by name. Use this when a skill is listed as available and you need its actual procedure/steps - e.g. before working in a workspace repo, running docker compose, or following a workflow. Accepts the skill name (case-insensitive; optionally scoped by category). Returns the complete SKILL.md content."
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
        // Skills live under profiles/<profile>/skills - view_skill must find
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
    fn view_skill_root_level_skill_directory_profile() {
        // Profile skills roots hold skills as <skills>/<name>/SKILL.md directly
        // (no category level), e.g. remote-development. view_skill must resolve
        // them (previously reported 'not found').
        let (cfg, dir) = test_config();
        let root_level = dir
            .join("profiles")
            .join("omni")
            .join("skills")
            .join("remote-development")
            .join("SKILL.md");
        fs::create_dir_all(root_level.parent().unwrap()).unwrap();
        fs::write(
            &root_level,
            "---\nname: remote-development\ndescription: \"Remote work\"\n---\n\n# Remote Development\n\nUse the persistent ssh config.\n",
        )
        .unwrap();
        let (msg, is_error) = handle_view_skill(
            serde_json::json!({"name": "remote-development"}),
            &cfg,
            "omni",
        )
        .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Remote Development"));
        assert!(msg.contains("Use the persistent ssh config"));
        // Category scoping with the category list_skills reports also works.
        let (msg2, is_error2) = handle_view_skill(
            serde_json::json!({"name": "remote-development", "category": "remote-development"}),
            &cfg,
            "omni",
        )
        .expect("view_skill with category should succeed");
        assert!(!is_error2, "msg: {}", msg2);
        assert!(msg2.contains("# Remote Development"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_root_level_skill_directory_global() {
        // Same root-level directory layout under the GLOBAL skills root.
        let (cfg, dir) = test_config();
        let root_level = dir
            .join("skills")
            .join("remote-development")
            .join("SKILL.md");
        fs::create_dir_all(root_level.parent().unwrap()).unwrap();
        fs::write(
            &root_level,
            "---\nname: remote-development\ndescription: \"Remote work\"\n---\n\n# Remote Development\n\nGlobal root dir layout.\n",
        )
        .unwrap();
        let (msg, is_error) =
            handle_view_skill(serde_json::json!({"name": "remote-development"}), &cfg, "")
                .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Remote Development"));
        assert!(msg.contains("Global root dir layout."));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_skill_matches_by_frontmatter_name() {
        // A skill whose frontmatter name differs from its directory name is
        // listed by its frontmatter name; view_skill must resolve that name.
        let (cfg, dir) = test_config();
        let skill_file = dir
            .join("skills")
            .join("general")
            .join("dir-name-differs")
            .join("SKILL.md");
        fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
        fs::write(
            &skill_file,
            "---\nname: listed-name\ndescription: \"FM name\"\n---\n\n# Listed Name\n\nFrontmatter name resolves.\n",
        )
        .unwrap();
        let (msg, is_error) =
            handle_view_skill(serde_json::json!({"name": "listed-name"}), &cfg, "")
                .expect("view_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        assert!(msg.contains("# Listed Name"));
        assert!(msg.contains("Frontmatter name resolves."));
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

    #[test]
    fn create_skill_writes_hermes_dir_layout_with_frontmatter() {
        let (cfg, dir) = test_config();
        let (msg, is_error) = handle_create_skill(
            serde_json::json!({
                "name": "my-skill",
                "description": "run the release build pipeline",
                "content": "# My Skill\n\n1. Do the thing.\n",
                "category": "devops",
                "tags": "build, release",
                "related_skills": "git-workflow",
            }),
            &cfg,
            "omni",
        )
        .expect("create_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        let skill_file = dir
            .join("profiles")
            .join("omni")
            .join("skills")
            .join("devops")
            .join("my-skill")
            .join("SKILL.md");
        assert!(skill_file.exists(), "SKILL.md missing: {msg}");
        let content = fs::read_to_string(&skill_file).unwrap();
        assert!(content.contains("description: \"Use when run the release build pipeline\""));
        assert!(content.contains("license: MIT"));
        assert!(content.contains("metadata:"));
        assert!(content.contains("tags:"));
        assert!(content.contains("- build"));
        assert!(content.contains("- release"));
        assert!(content.contains("related_skills:"));
        assert!(content.contains("- git-workflow"));
        assert!(content.contains("name: my-skill"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_skill_keeps_existing_use_when_prefix() {
        let (cfg, dir) = test_config();
        let (msg, is_error) = handle_create_skill(
            serde_json::json!({
                "name": "with-trigger",
                "description": "Use when you need to deploy the stack",
                "content": "body",
            }),
            &cfg,
            "omni",
        )
        .expect("create_skill should succeed");
        assert!(!is_error, "msg: {}", msg);
        let skill_file = dir
            .join("profiles")
            .join("omni")
            .join("skills")
            .join("general")
            .join("with-trigger")
            .join("SKILL.md");
        let content = fs::read_to_string(&skill_file).unwrap();
        assert!(content.contains("description: \"Use when you need to deploy the stack\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_skill_rejects_description_over_1024_chars() {
        let (cfg, dir) = test_config();
        let long_desc = "x".repeat(1025);
        let err = handle_create_skill(
            serde_json::json!({"name": "too-long", "description": long_desc, "content": "body"}),
            &cfg,
            "omni",
        )
        .expect_err(">1024 char description must be rejected");
        assert!(err.to_string().contains("1024"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_skill_rejects_duplicate_in_dir_layout() {
        let (cfg, dir) = test_config();
        handle_create_skill(
            serde_json::json!({"name": "dup", "description": "first", "content": "a"}),
            &cfg,
            "omni",
        )
        .expect("first create should succeed");
        let err = handle_create_skill(
            serde_json::json!({"name": "dup", "description": "second", "content": "b"}),
            &cfg,
            "omni",
        )
        .expect_err("duplicate must be rejected");
        assert!(err.to_string().contains("already exists"));
        let _ = fs::remove_dir_all(&dir);
    }
}
