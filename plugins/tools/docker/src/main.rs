//! mcp-server-compose — standalone MCP server for docker compose commands.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: compose
//!
//! **Concurrency**: Each tool call runs in its own tokio task, so long-running
//! compose commands (up, exec, run) do not block other concurrent tool calls.
//!
//! **Config**: The plugin reads all configuration from the MCP `configure`
//! message (delivered by omniagent from the plugin config / config_schema).
//! It does NOT read environment variables. The single config key is
//! `workspace_dir` (default `/opt/workspace`) — the root under which the agent
//! may run compose projects.
//!
//! **Tool API**:
//! - `project_dir` (required): the compose project directory. Must be the
//!   workspace dir or a subdirectory of it.
//! - `compose_file` (optional, default `docker-compose.yml`): the compose file
//!   relative to `project_dir` (may include subdirectories). Must stay inside
//!   `project_dir`.
//! - `env_file` (optional): a `.env`-style file relative to `project_dir`
//!   (may include subdirectories). Must stay inside `project_dir`. Passed to
//!   `docker compose --env-file`.

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

/// Resolve the project directory against the configured workspace root.
///
/// `project_dir` is required and must be the workspace dir or a subdirectory
/// of it. Returns the canonicalized absolute project directory.
fn resolve_project_dir(project_dir: &str, configured_workspace: &str) -> Result<String> {
    if project_dir.is_empty() {
        anyhow::bail!(
            "Missing 'project_dir' argument: must be the workspace dir or a subdirectory"
        );
    }
    let configured_path = Path::new(configured_workspace)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(configured_workspace).to_path_buf());

    let resolved = Path::new(project_dir)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Invalid project directory '{}': {}", project_dir, e))?;
    if !resolved.starts_with(&configured_path) {
        anyhow::bail!(
            "Project directory must be inside workspace ({}), got: {}",
            configured_workspace,
            project_dir
        );
    }
    if !resolved.is_dir() {
        anyhow::bail!("Project directory does not exist: {}", resolved.display());
    }
    Ok(resolved.display().to_string())
}

/// Resolve a relative file (compose_file or env_file) against the project
/// directory. The file must stay inside the project directory (or a
/// subdirectory of it).
fn resolve_project_file(file: &str, project_dir: &str, what: &str) -> Result<String> {
    if contains_forbidden_chars(file) {
        anyhow::bail!("Forbidden characters in {} argument", what);
    }
    let project = Path::new(project_dir)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    // Files are relative to project_dir by design. Reject absolute paths —
    // the whole point is that the file lives inside the project directory.
    let candidate = Path::new(file);
    if candidate.is_absolute() {
        anyhow::bail!(
            "{} must be relative to project_dir, got absolute path: {}",
            what,
            file
        );
    }

    let resolved = project
        .join(candidate)
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Invalid {} '{}': {}", what, file, e))?;
    if !resolved.starts_with(&project) {
        anyhow::bail!(
            "{} must be inside project_dir ({}), got: {}",
            what,
            project_dir,
            file
        );
    }
    if !resolved.is_file() {
        anyhow::bail!("{} does not exist: {}", what, resolved.display());
    }
    Ok(resolved.display().to_string())
}

/// Build a tokio::process::Command for `docker compose`.
fn build_compose_command(
    command: &str,
    project_dir: &str,
    compose_file: &str,
    env_file: &str,
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

    // Compose file: resolve against project_dir (default docker-compose.yml).
    // It's already validated to stay inside project_dir by the caller.
    let compose_path = Path::new(project_dir).join(compose_file);
    cmd.arg("-f");
    cmd.arg(&compose_path);

    // Optional env file: --env-file must be given BEFORE the subcommand.
    if !env_file.is_empty() {
        let env_path = Path::new(project_dir).join(env_file);
        cmd.arg("--env-file");
        cmd.arg(&env_path);
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
            anyhow::bail!(
                "'service' is required for '{}' command. Add it to the tool call, e.g. \
                 {{\"project_dir\": \"/opt/workspace/my-project\", \"command\": \"exec\", \
                 \"service\": \"backend\", \"args\": \"ls -la\"}}",
                verb
            );
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
// Error enrichment helpers
// ---------------------------------------------------------------------------

/// Detect a host port allocation conflict in docker compose stderr and build a
/// helpful message naming the port and suggesting a fix.
///
/// Docker-on-Docker (this Hermes container) has NO host port mapping, so agent
/// projects commonly collide with other containers that DO publish ports
/// (movie-db backend hit "Bind for 0.0.0.0:8080 failed: port is already
/// allocated"). When the error contains the classic Docker message, extract the
/// offending port and tell the agent what to do instead of dumping raw stderr.
fn enrich_port_conflict(stderr: &str) -> Option<String> {
    // Match: "Bind for 0.0.0.0:8080 failed: port is already allocated"
    // or:     "Error response from daemon: driver failed programming external
    //          connectivity on endpoint X (...): Bind for 0.0.0.0:8080 failed:
    //          port is already allocated"
    // Extract the port manually (no regex dependency).
    let marker = "port is already allocated";
    if !stderr.contains(marker) {
        return None;
    }
    let bind_marker = "Bind for ";
    let idx = stderr.find(bind_marker)?;
    let rest = &stderr[idx + bind_marker.len()..];
    let after_addr = rest.find(':')?;
    let after_addr = &rest[after_addr + 1..];
    let port: String = after_addr
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if port.is_empty() {
        return None;
    }

    let mut msg = format!(
        "HOST PORT CONFLICT: port {} is already allocated by another container on this host.\n\n",
        port
    );
    msg.push_str(
        "This usually means another service (e.g. the omni stack itself) already publishes that port, \
         or a previous container of this project is still running.\n\n",
    );
    msg.push_str("Fixes:\n");
    msg.push_str(&format!(
        "1. Pick a different host port for this service (e.g. map 8081:8080 instead of {}:8080) \
         and update the ports: mapping in your compose file.\n",
        port
    ));
    msg.push_str(
        "2. If the conflict is with a leftover container from a previous run of THIS project, \
         stop it first: docker compose -f <compose-file> down  (or 'docker rm -f <container-name>').\n",
    );
    msg.push_str(
        "3. Remember: services only need to reach each other INSIDE the docker network — \
         prefer omitting host 'ports:' entirely and use the service name as the hostname \
         (e.g. http://backend:8080 from another container).\n",
    );
    Some(msg)
}

/// Build a "command failed" error message, enriching known failure modes
/// (port conflicts) with actionable guidance.
fn build_failure_message(rc: i32, stdout: &str, stderr: &str, cmd_display: &str) -> String {
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
    // Enrich known failure modes
    if let Some(port_msg) = enrich_port_conflict(stderr) {
        msg.push('\n');
        msg.push_str(&port_msg);
    }
    msg.push_str(&format!("\nCommand:\n{}", cmd_display));
    msg
}

// ---------------------------------------------------------------------------
// Tool: compose (async handler)
// ---------------------------------------------------------------------------

async fn handle_compose(args: Value, config: &Config) -> Result<(String, bool)> {
    let configured_workspace = &config.workspace_dir;

    let command = args["command"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!(
            "Missing 'command' argument. Required fields: project_dir (string, required) - the compose project directory (workspace dir or a subdirectory), \
            command (string, required) - compose verb + flags (e.g. 'up -d', 'build', 'ps', 'logs --tail=50'). \
            For exec/run you ALSO need: service (string) - container/service name, args (string) - command to run inside the container. \
            Example: {{\"project_dir\": \"/opt/workspace/my-project\", \"command\": \"exec\", \"service\": \"backend\", \"args\": \"npm run build\"}}"
        ))?
        .to_string();

    let project_dir_arg = args["project_dir"].as_str().unwrap_or("").to_string();
    let compose_file_arg = args["compose_file"].as_str().unwrap_or("").to_string();
    let env_file_arg = args["env_file"].as_str().unwrap_or("").to_string();
    let service_name = args["service"].as_str().unwrap_or("");
    let exec_args = args["args"].as_str().unwrap_or("");
    let raw_script = args["script"].as_str().unwrap_or("");

    // Optional per-command timeout override (seconds).
    let timeout_override = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()));

    // Resolve project_dir: required, must be inside the configured workspace.
    if contains_forbidden_chars(&project_dir_arg) {
        anyhow::bail!("Forbidden characters in project_dir argument");
    }
    let project_dir = resolve_project_dir(&project_dir_arg, configured_workspace)?;

    // Resolve compose_file: relative to project_dir, default docker-compose.yml.
    let compose_file = if compose_file_arg.is_empty() {
        "docker-compose.yml".to_string()
    } else {
        compose_file_arg.clone()
    };
    let resolved_compose_file = resolve_project_file(&compose_file, &project_dir, "compose_file")?;

    // Resolve env_file: relative to project_dir, optional.
    let resolved_env_file = if env_file_arg.is_empty() {
        String::new()
    } else {
        resolve_project_file(&env_file_arg, &project_dir, "env_file")?
    };

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

    let mut cmd = build_compose_command(
        &command,
        &project_dir,
        &resolved_compose_file,
        &resolved_env_file,
        service_name,
        exec_args,
        raw_script,
    )?;
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
            let msg = build_failure_message(rc, &stdout, &stderr, &cmd_display);
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
                let msg = build_failure_message(rc, &stdout, &stderr, &cmd_display);
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
                "Run docker compose commands on a compose project inside the workspace. \
                 'project_dir' (required) is the compose project directory — the workspace dir or a subdirectory of it. \
                 'compose_file' (optional) is the compose file relative to project_dir (default docker-compose.yml, \
                 may include subdirectories, must stay inside project_dir). \
                 'env_file' (optional) is a .env-style file relative to project_dir, passed via --env-file \
                 (must stay inside project_dir). \
                 Use 'command' for the compose verb + flags (e.g. 'up -d', 'ps', 'build', 'logs --tail=50'). \
                 For exec/run: use 'service' (container name) and 'args' (command to run inside container). \
                 USAGE EXAMPLES: \
                 - start a project: {{\"project_dir\": \"/opt/workspace/my-project\", \"command\": \"up -d\"}} \
                 - run a command inside a container: {{\"project_dir\": \"/opt/workspace/my-project\", \"command\": \"exec\", \"service\": \"backend\", \"args\": \"npm run build\"}} \
                 - view logs: {{\"project_dir\": \"/opt/workspace/my-project\", \"command\": \"logs --tail=50\"}} \
                 'args' have NO character restrictions -- automatically wrapped in sh -c, \
                 so use shell operators (&&, ||, |, quotes) exactly as on a host terminal. \
                 For exec with 'script': pass Python code as the 'script' parameter and it will be piped \
                 to python3 inside the container via stdin (no character restrictions -- ideal for complex scripts). \
                 Optional 'timeout' parameter overrides the default timeout for long-running commands. \
                 NOTE: if a command fails with a host port conflict, pick a different host port or omit \
                 the host 'ports:' mapping (services reach each other by service name inside the docker network)."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_dir": {
                        "type": "string",
                        "description": "The compose project directory: the workspace dir or a subdirectory of it."
                    },
                    "compose_file": {
                        "type": "string",
                        "description": "Compose file relative to project_dir (default docker-compose.yml). May include subdirectories; must stay inside project_dir."
                    },
                    "env_file": {
                        "type": "string",
                        "description": ".env-style file relative to project_dir, passed via --env-file. Must stay inside project_dir."
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_conflict_detected_plain() {
        let stderr = "Error response from daemon: driver failed programming external connectivity on endpoint movie-db-backend (abc123): Bind for 0.0.0.0:8080 failed: port is already allocated";
        let msg = enrich_port_conflict(stderr).expect("should detect port conflict");
        assert!(
            msg.contains("8080"),
            "should name the offending port: {}",
            msg
        );
        assert!(msg.contains("HOST PORT CONFLICT"));
        assert!(
            msg.contains("8081:8080"),
            "should suggest an alternative port"
        );
    }

    #[test]
    fn port_conflict_detected_simple() {
        let stderr = "Bind for 0.0.0.0:5173 failed: port is already allocated";
        let msg = enrich_port_conflict(stderr).expect("should detect port conflict");
        assert!(msg.contains("5173"));
    }

    #[test]
    fn no_port_conflict_returns_none() {
        assert!(enrich_port_conflict("some other error: permission denied").is_none());
        assert!(enrich_port_conflict("").is_none());
        assert!(enrich_port_conflict("Bind for 0.0.0.0:8080 failed").is_none());
    }

    #[test]
    fn failure_message_includes_guidance_on_port_conflict() {
        let stderr = "Bind for 0.0.0.0:8080 failed: port is already allocated";
        let msg = build_failure_message(1, "", stderr, "docker compose up -d");
        assert!(msg.contains("HOST PORT CONFLICT"));
        assert!(msg.contains("8080"));
        assert!(msg.contains("Command:"));
    }

    #[test]
    fn failure_message_plain_error_no_guidance() {
        let stderr = "service \"db\" is undefined";
        let msg = build_failure_message(1, "", stderr, "docker compose up -d");
        assert!(!msg.contains("HOST PORT CONFLICT"));
        assert!(msg.contains("service \"db\" is undefined"));
    }
}
