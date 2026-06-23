//! Native Docker Compose MCP tool.
//!
//! Runs `docker compose` commands, restricted to project directories
//! under the configured workspace directory.
//!
//! Tools:
//!   - `compose`: Run docker compose commands (ps, up, down, logs, build, exec, stop, restart, pull)

use crate::mcp::{truncate_content, AppContext, McpTool, McpToolResult, DEFAULT_MAX_TOOL_OUTPUT_CHARS};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// Characters that are forbidden in arguments to prevent shell injection.
const FORBIDDEN_CHARS: &[char] = &['|', ';', '&', '`', '$', '>', '<', '*', '?', '[', ']', '{', '}', '!', '~'];

/// Validate that a string contains no shell-metacharacters.
fn contains_forbidden_chars(s: &str) -> bool {
    s.chars().any(|c| FORBIDDEN_CHARS.contains(&c))
}

/// Validate that a project directory is under the allowed workspace.
fn validate_workspace_path(project_dir: &str, workspace_dir: &str) -> Result<()> {
    if project_dir.is_empty() {
        return Ok(());
    }
    let resolved = Path::new(project_dir)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Invalid project directory '{}': {}", project_dir, e))?;
    let workspace = Path::new(workspace_dir)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(workspace_dir).to_path_buf());
    if !resolved.starts_with(&workspace) {
        anyhow::bail!(
            "Project directory must be under {}, got: {}",
            workspace_dir,
            project_dir
        );
    }
    if !resolved.is_dir() {
        anyhow::bail!("Project directory does not exist: {}", resolved.display());
    }
    Ok(())
}

/// Run a docker command with timeout and return (stdout, stderr, exit_code).
fn run_docker(args: &[&str], dir: Option<&str>, timeout_secs: u64) -> (String, String, i32) {
    let mut cmd = Command::new("docker");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }

    // Use a channel to implement timeout
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = cmd.output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(output)) => (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code().unwrap_or(-1),
        ),
        Ok(Err(e)) => (String::new(), format!("docker command failed: {}", e), -1),
        Err(_) => (
            String::new(),
            format!("docker command timed out after {}s", timeout_secs),
            -1,
        ),
    }
}

/// Create the `compose` MCP tool.
pub fn compose_tool() -> McpTool {
    McpTool {
        name: "compose".to_string(),
        description: "RUN DOCKER COMPOSE commands (ps, up, down, logs, build, exec, stop, restart, pull). \
            Only operates on projects under the configured workspace directory. \
            Use 'dir' to set the project directory containing docker-compose.yml. \
            Use 'service' to target a specific service (e.g. 'logs web' or 'stop db'). \
            Use 'args' for extra flags like '-d' for 'up -d' or '--tail=50' for 'logs --tail=50'."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "dir": {
                    "type": "string",
                    "description": "Absolute path to the project directory containing docker-compose.yml"
                },
                "command": {
                    "type": "string",
                    "description": "Compose subcommand: ps, up, down, logs, build, exec, stop, restart, pull"
                },
                "service": {
                    "type": "string",
                    "description": "Service name (optional, for targeted commands)"
                },
                "args": {
                    "type": "string",
                    "description": "Extra arguments e.g. '-d' for 'up -d', '--tail=50' for 'logs --tail=50'"
                }
            },
            "required": ["command"]
        }),
        handler: Arc::new(|args: Value, ctx: AppContext| -> Result<McpToolResult> {
            let command = args["command"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?
                .to_string();

            if contains_forbidden_chars(&command) {
                anyhow::bail!("Forbidden characters in command argument");
            }

            let project_dir = args["dir"].as_str().unwrap_or("").to_string();
            if contains_forbidden_chars(&project_dir) {
                anyhow::bail!("Forbidden characters in dir argument");
            }

            if !project_dir.is_empty() {
                validate_workspace_path(&project_dir, &ctx.workspace_dir)?;
            }

            let service = args["service"].as_str().unwrap_or("").to_string();
            if contains_forbidden_chars(&service) {
                anyhow::bail!("Forbidden characters in service argument");
            }

            let extra_args = args["args"].as_str().unwrap_or("").to_string();
            if contains_forbidden_chars(&extra_args) {
                anyhow::bail!("Forbidden characters in args argument");
            }

            // Build arguments vector
            let mut arg_parts: Vec<String> = Vec::new();

            // Prepend "compose" — the args vector is passed to `docker` CLI,
            // so the full command is `docker compose <subcommand> ...`
            arg_parts.push("compose".to_string());

            // Note: current_dir() is already set in run_docker(), so CWD
            // is sufficient for compose to find docker-compose.yml. No
            // need for --project-directory.

            arg_parts.push(command.clone());

            if !service.is_empty() {
                arg_parts.push(service.clone());
            }

            for arg in extra_args.split_whitespace() {
                if !arg.is_empty() {
                    arg_parts.push(arg.to_string());
                }
            }

            // Rebuild as Vec<&str> for the helper
            let arg_refs: Vec<&str> = arg_parts.iter().map(|s| s.as_str()).collect();
            let dir_opt = if project_dir.is_empty() { None } else { Some(project_dir.as_str()) };

            let timeout = if command == "build" { 600u64 } else { 300u64 };
            let (stdout, stderr, rc) = run_docker(&arg_refs, dir_opt, timeout);

            if rc != 0 {
                return Ok(McpToolResult {
                    call_id: String::new(),
                    content: format!("docker compose {} failed (exit {}):\n{}", command, rc, stderr),
                    is_error: true,
                });
            }

            let content = if stdout.is_empty() {
                format!("docker compose {}: ok", command)
            } else {
                format!(
                    "```\n{}\n```",
                    truncate_content(&stdout, DEFAULT_MAX_TOOL_OUTPUT_CHARS)
                )
            };

            Ok(McpToolResult {
                call_id: String::new(),
                content,
                is_error: false,
            })
        }),
    }
}
