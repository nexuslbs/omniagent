//! mcp-server-compose — standalone MCP server for docker compose commands.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: compose
//!
//! **Concurrency**: Each tool call runs in its own tokio task, so long-running
//! compose commands (up, exec, run) do not block other concurrent tool calls.

use anyhow::Result;
use mcp_server_util::*;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Allowed compose subcommands. Everything else will be rejected.
const ALLOWED_VERBS: &[&str] = &[
    "up", "down", "ps", "logs", "build", "restart", "stop", "exec", "run", "pull",
];

/// Characters forbidden in non-exec/run arguments (compose verb, service name, flags).
const FORBIDDEN_CHARS: &[char] = &[
    '|', ';', '&', '`', '$', '>', '<', '?', '[', ']', '{', '}', '!', '~',
];

/// Default timeouts per command verb (seconds).
fn default_timeout(verb: &str) -> u64 {
    match verb {
        "build" | "pull" => 600,
        "up" | "restart" => 300,
        "exec" | "run" => 600,
        _ => 300, // ps, logs, down, stop
    }
}

/// Validate that a string contains no forbidden shell-metacharacters.
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

/// Build a tokio::process::Command for `docker compose`.
fn build_compose_command(
    command: &str,
    project_dir: &str,
    service_name: &str,
    exec_args: &str,
    raw_script: &str,
) -> Result<Command> {
    let verb = command.split_whitespace().next().unwrap_or("");
    if verb.is_empty() || !ALLOWED_VERBS.contains(&verb) {
        anyhow::bail!(
            "Unrecognized compose command '{}'. Allowed: {}",
            verb,
            ALLOWED_VERBS.join(", ")
        );
    }

    let mut cmd = Command::new("docker");
    cmd.arg("compose");

    if !project_dir.is_empty() {
        cmd.arg("--project-directory");
        cmd.arg(&project_dir);
    }

    let parts: Vec<&str> = command.split_whitespace().collect();
    cmd.arg(verb);

    // For exec/run: only pass through flag-like parts (starting with '-').
    // The service name and command args have their own dedicated parameters
    // (service_name, exec_args). Any non-flag words in parts[1..] are leaked
    // service names or shell commands that the LLM accidentally put in the
    // command field -- they would cause a double-service-name error or
    // forbidden-char rejections if passed through.  Strip them out.
    let is_exec_or_run = verb == "exec" || verb == "run";
    let extra_parts: Vec<&str> = if is_exec_or_run {
        parts[1..]
            .iter()
            .filter(|p| p.starts_with('-'))
            .copied()
            .collect()
    } else {
        parts[1..].to_vec()
    };

    for part in &extra_parts {
        if contains_forbidden_chars(part) {
            anyhow::bail!("Forbidden characters in command argument: '{}'", part);
        }
        cmd.arg(part);
    }

    if verb == "exec" || verb == "run" {
        if service_name.is_empty() {
            anyhow::bail!("'service' is required for '{}' command", verb);
        }
        if verb == "exec" && !raw_script.is_empty() {
            // Pipe script via stdin to avoid forbidden chars in command args.
            cmd.arg("-T"); // no TTY -- required for stdin piping
            cmd.arg(service_name);
            cmd.arg("python3");
        } else {
            cmd.arg("-T"); // no TTY -- prevents hangs on output-producing commands
            cmd.arg(service_name);
            if !exec_args.is_empty() {
                // Pass the full command string as a single argument to sh -c
                // so shell operators (&&, ||, |, quotes) work inside the container.
                // Docker compose exec passes args directly through execve --
                // no host shell stripping.  Wrapping in sh -c gives the
                // container-side shell full interpretation of the command.
                cmd.arg("sh");
                cmd.arg("-c");
                cmd.arg(exec_args);
            }
        }
    }

    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Tool: compose (async handler)
// ---------------------------------------------------------------------------

async fn handle_compose(args: Value, config: &Config) -> Result<(String, bool)> {
    let workspace_dir = &config.workspace_dir;

    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!(
            "Missing 'command' argument. Valid parameters: project_dir (string) - directory with docker-compose.yml, \
            command (string, required) - compose verb + flags (e.g. 'up -d', 'build', 'ps', 'logs --tail=50'), \
            service (string) - container name (required for exec/run), \
            args (string) - command to run inside container (for exec/run, no char restrictions), \
            script (string) - Python code piped via stdin to python3 inside container (for exec only), \
            timeout (number) - override default timeout in seconds"
        ))?
        .to_string();

    let project_dir = args["project_dir"].as_str().unwrap_or("").to_string();
    let service_name = args["service"].as_str().unwrap_or("");
    let exec_args = args["args"].as_str().unwrap_or("");
    let raw_script = args["script"].as_str().unwrap_or("");

    // Optional per-command timeout override (seconds).
    let timeout_override = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()));

    // Validate project_dir
    if contains_forbidden_chars(&project_dir) {
        anyhow::bail!("Forbidden characters in project_dir argument");
    }
    if !project_dir.is_empty() {
        validate_workspace_path(&project_dir, &workspace_dir)?;
    }

    let verb = command.split_whitespace().next().unwrap_or("");
    let timeout_secs = timeout_override.unwrap_or_else(|| default_timeout(verb));

    // Validate the verb is allowed (build_compose_command will also check)
    if verb.is_empty() || !ALLOWED_VERBS.contains(&verb) {
        anyhow::bail!(
            "Unrecognized compose command '{}'. Allowed: {}",
            verb,
            ALLOWED_VERBS.join(", ")
        );
    }

    let mut cmd =
        build_compose_command(&command, &project_dir, service_name, exec_args, raw_script)?;
    let cmd_repr = format!("{:?}", cmd);

    // Truncated command display for error messages (max 1000 chars to avoid
    // bloating LLM context with huge inline scripts in exec_args).
    const CMD_DISPLAY_MAX: usize = 1000;
    let cmd_display = if cmd_repr.len() > CMD_DISPLAY_MAX {
        format!(
            "{}... [truncated from {} chars]",
            &cmd_repr[..CMD_DISPLAY_MAX],
            cmd_repr.len()
        )
    } else {
        cmd_repr.clone()
    };

    // If script is provided, pipe it via stdin
    if verb == "exec" && !raw_script.is_empty() {
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;

        // Write script to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(raw_script.as_bytes()).await?;
            // Close stdin so the remote python process knows to stop reading
            drop(stdin);
        }

        let output = child.wait_with_output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let rc = output.status.code().unwrap_or(-1);

        if rc != 0 {
            let mut msg = format!("docker compose command failed (exit {}):\n\n", rc);
            if !stdout.is_empty() {
                msg.push_str(&format!(
                    "--- stdout ({} chars) ---\n{}\n",
                    stdout.len(),
                    stdout
                ));
            }
            if !stderr.is_empty() {
                msg.push_str(&format!(
                    "--- stderr ({} chars) ---\n{}\n",
                    stderr.len(),
                    stderr
                ));
            }
            if stdout.is_empty() && stderr.is_empty() {
                msg.push_str("(no output)\n");
            }
            msg.push_str(&format!("\nCommand:\n{}", cmd_display));
            return Ok((msg, true));
        }

        let content = if stdout.is_empty() {
            format!(
                "docker compose {}: ok ({} bytes script piped via stdin)",
                command,
                raw_script.len()
            )
        } else {
            let max_chars: usize = 50_000;
            if stdout.len() > max_chars {
                format!(
                    "```\n{}\n```\n\n[... truncated from {} to ~{} chars]",
                    &stdout[..max_chars],
                    stdout.len(),
                    max_chars
                )
            } else {
                format!("```\n{}\n```", stdout)
            }
        };
        return Ok((content, false));
    }

    // Standard execution (no script piped via stdin)
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let rc = output.status.code().unwrap_or(-1);

            if rc != 0 {
                let mut msg = format!("docker compose command failed (exit {}):\n\n", rc);
                if !stdout.is_empty() {
                    msg.push_str(&format!("--- stdout ({} chars) ---\n{}\n", stdout.len(), stdout));
                }
                if !stderr.is_empty() {
                    msg.push_str(&format!("--- stderr ({} chars) ---\n{}\n", stderr.len(), stderr));
                }
                if stdout.is_empty() && stderr.is_empty() {
                    msg.push_str("(no output)\n");
                }
                msg.push_str(&format!("\nCommand:\n{}", cmd_display));
                return Ok((msg, true));
            }

            let content = if stdout.is_empty() {
                format!("docker compose {}: ok", command)
            } else {
                let max_chars: usize = 50_000;
                if stdout.len() > max_chars {
                    format!(
                        "```\n{}\n```\n\n[... truncated from {} to ~{} chars]",
                        &stdout[..max_chars],
                        stdout.len(),
                        max_chars
                    )
                } else {
                    format!("```\n{}\n```", stdout)
                }
            };

            Ok((content, false))
        }
        Ok(Err(e)) => Ok((format!("docker command failed: {}\n\nCommand:\n{}", e, cmd_display), true)),
        Err(_elapsed) => Ok((
            format!(
                "docker compose command timed out after {}s (use 'timeout' param to override)\n\nCommand:\n{}",
                timeout_secs, cmd_display,
            ),
            true,
        )),
    }
}

// ---------------------------------------------------------------------------
// Plugin config — received via MCP configure message, not from env vars
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Config {
    workspace_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace_dir: "/opt/workspace".to_string(),
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
                if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                    if !dir.is_empty() {
                        cfg.workspace_dir = dir.to_string();
                    }
                }
            }
        })
    };

    let c1 = config.clone();
    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "compose".to_string(),
            description:
                "Run docker compose commands. \
                 Use 'project_dir' for the directory with docker-compose.yml. \
                 Use 'command' for the compose verb + flags (e.g. 'up -d', 'ps', 'build', 'logs --tail=50'). \
                 For exec/run: use 'service' (container name) and 'args' (command to run inside container). \
                 'args' have NO character restrictions -- automatically wrapped in sh -c, \
                 so use shell operators (&&, ||, |, quotes) exactly as on a host terminal. \
                 For exec with 'script': pass Python code as the 'script' parameter and it will be piped \
                 to python3 inside the container via stdin (no character restrictions -- ideal for complex scripts). \
                 Optional 'timeout' parameter overrides the default timeout for long-running commands."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_dir": {
                        "type": "string",
                        "description": "Directory containing docker-compose.yml"
                    },
                    "command": {
                        "type": "string",
                        "description": "Compose subcommand and flags (e.g. 'up -d', 'ps', 'build', 'exec', 'logs --tail=50')"
                    },
                    "service": {
                        "type": "string",
                        "description": "Service/container name (required for exec and run commands)"
                    },
                    "args": {
                        "type": "string",
                        "description": "Command to run inside the container (for exec/run). No character restrictions. Automatically wrapped in sh -c, so write commands exactly as on a host terminal. Examples: 'cd /app && npm run build', 'ls -la && cat config.json'"
                    },
                    "script": {
                        "type": "string",
                        "description": "Python script to pipe via stdin into python3 inside the container (for exec only). No character restrictions. Use this for complex multi-line scripts instead of 'args'."
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Optional -- override default timeout in seconds. Defaults: build/pull=600, up/restart=300, exec/run=600, ps/logs/down/stop=300"
                    }
                },
                "required": ["project_dir", "command"]
            }),
        },
        handler: Box::new(move |args: Value, _meta: Option<McpMeta>| {
            let c = c1.clone();
            Box::pin(async move {
                let config = c.lock().unwrap().clone();
                handle_compose(args, &config).await
            })
        }),
    }];

    let server_info = ServerInfo {
        name: "mcp-server-compose".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(server_info, tools, on_configure).await
}
