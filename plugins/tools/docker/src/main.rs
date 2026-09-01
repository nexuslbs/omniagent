//! mcp-server-compose - standalone MCP server for docker compose commands.
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
//! `workspace_dir` (default `/opt/workspace`) - the root under which the agent
//! may run compose projects.
//!
//! **Tool API**:
//! - `project_dir` (required): the compose project directory. Must be the
//!   workspace dir or a subdirectory of it.
//! - `compose_file` (optional, default `docker-compose.yml`): the compose file,
//!   relative to `project_dir` or an absolute path. Must stay inside the
//!   configured `workspace_dir`.
//! - `env_file` (optional): a `.env`-style file relative to `project_dir` or
//!   an absolute path. Must stay inside the configured `workspace_dir`. Passed
//!   to `docker compose --env-file`.
//!
//! **Project name**: the compose project name is derived from the tool's
//! arguments ONLY: `COMPOSE_PROJECT_NAME` from `env_file` (pinned with
//! `--project-name`), else the compose file's own `name:` (default) or the
//! project-dir basename. Ambient `COMPOSE_*` variables are stripped from the
//! child environment, so the tool can never inherit the agent's own project
//! name (e.g. the production stack) by accident.

use anyhow::Result;
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
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

/// Resolve a file (compose_file or env_file). The file may be:
/// - relative to project_dir (stays inside project_dir), or
/// - an absolute path ANYWHERE inside the configured workspace_dir
///   (e.g. `/opt/workspace/omni-deployer/omnidev.env` while the compose
///   project lives in `/opt/workspace/omni-stack`).
///
/// The workspace root is the sandbox boundary: files outside it are rejected.
fn resolve_project_file(
    file: &str,
    project_dir: &str,
    configured_workspace: &str,
    what: &str,
) -> Result<String> {
    if contains_forbidden_chars(file) {
        anyhow::bail!("Forbidden characters in {} argument", what);
    }
    let workspace = Path::new(configured_workspace)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(configured_workspace).to_path_buf());
    let project = Path::new(project_dir)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let candidate = Path::new(file);
    let resolved = if candidate.is_absolute() {
        // Absolute path: allowed anywhere inside the configured workspace.
        candidate
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Invalid {} '{}': {}", what, file, e))?
    } else {
        // Relative path: resolved against project_dir (backwards compatible).
        project
            .join(candidate)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Invalid {} '{}': {}", what, file, e))?
    };

    if !resolved.starts_with(&workspace) {
        anyhow::bail!(
            "{} must be inside workspace ({}), got: {}",
            what,
            configured_workspace,
            resolved.display()
        );
    }
    if !resolved.is_file() {
        anyhow::bail!("{} does not exist: {}", what, resolved.display());
    }
    Ok(resolved.display().to_string())
}


/// Read `COMPOSE_PROJECT_NAME` from an env-style file (the same variable docker
/// compose reads for the project name). Returns the value if present.
/// Understands `KEY=value` lines, an optional `export ` prefix, surrounding
/// quotes, and `#` comments.
fn env_file_project_name(env_file: &str) -> Option<String> {
    let content = std::fs::read_to_string(env_file).ok()?;
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim();
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "COMPOSE_PROJECT_NAME" {
                let v = value.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Build a tokio::process::Command for `docker compose`.
fn build_compose_command(
    command: &str,
    project_dir: &str,
    compose_files: &[String],
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

    // PIN the project directory explicitly: without it docker compose derives
    // it from the plugin process CWD / first compose file, so `.env` and
    // relative paths can resolve against the wrong place.
    cmd.arg("--project-directory");
    cmd.arg(project_dir);

    // Compose files: one -f per file, in order (base first, overrides after).
    // Each is already validated to stay inside the workspace by the caller.
    for compose_path in compose_files {
        cmd.arg("-f");
        cmd.arg(compose_path);
    }

    // Optional env file: --env-file must be given BEFORE the subcommand.
    // env_file arrives already resolved to an absolute path by the caller.
    if !env_file.is_empty() {
        cmd.arg("--env-file");
        cmd.arg(env_file);
    }

    // CRITICAL (2026-09-01, production-incident fix): the project name must be
    // determined by the TOOL'S ARGUMENTS, never by the ambient process
    // environment. omniagent loads the production .env (COMPOSE_PROJECT_NAME=
    // omni-stack) into spawned plugin environments, and docker compose's
    // interpolation precedence is "process env beats --env-file", so every
    // call used to resolve to the PRODUCTION project no matter what
    // project_dir / env_file were passed. Fix:
    //  - strip ambient COMPOSE_* vars (they must come from the env_file);
    //  - if the env_file sets COMPOSE_PROJECT_NAME, pin it explicitly with
    //    --project-name (highest precedence, argument-driven);
    //  - otherwise leave the name to the compose file (`name:` field default
    //    e.g. `name: ${COMPOSE_PROJECT_NAME:-omni}` -> "omni") or the
    //    project-dir basename - never the ambient environment.
    cmd.env_remove("COMPOSE_PROJECT_NAME");
    cmd.env_remove("COMPOSE_PROFILES");
    cmd.env_remove("COMPOSE_FILE");
    if !env_file.is_empty() {
        if let Some(name) = env_file_project_name(env_file) {
            cmd.arg("--project-name");
            cmd.arg(name);
        }
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

    // CRITICAL (G17b, Aug 2026): NEVER let the docker CLI child inherit this
    // server's stdin (fd 0 = the MCP JSON-RPC pipe). A tokio::process::Command
    // with no explicit .stdin() inherits the parent's fd 0. Under 50 concurrent
    // `docker compose exec` calls the CLI children hold the read end of the
    // SAME pipe the client writes requests to - they steal JSON-RPC lines and
    // the server's reader never sees them (intermittent request loss, sparse
    // dispatch tails). The exec-script path overrides stdin with a fresh piped
    // fd below; every other path gets /dev/null so the CLI can never consume
    // protocol bytes.
    cmd.stdin(std::process::Stdio::null());

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
        "3. Remember: services only need to reach each other INSIDE the docker network - \
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
// Kill-on-drop guard for foreground subprocesses
// ---------------------------------------------------------------------------

/// A spawned child process that is killed when the guard is dropped.
///
/// The docker CLI child (`docker compose …`) runs the agent's command. When
/// the thread that requested the tool call ends (interrupted / failed /
/// completed / stopped / `/stop-thread` / channel close), the omniagent client
/// sends an MCP `notifications/cancelled` for the in-flight request; the shared
/// server framework then DROPS this handler's future, which drops this guard,
/// which kills the child. Without this, a `docker compose exec … cargo build`
/// chain keeps burning CPU/RAM detached after its thread died (thread 73,
/// Aug 2026 - PIDs alive 6+ min after thread end).
///
/// The child is spawned in its own process group (`process_group(0)`), and the
/// kill also signals the whole group, so the `docker compose → docker` local
/// chain is reaped too - killing only the direct child would orphan the
/// grandchildren.
///
/// Explicit DETACHED operations (`up -d`, `run -d`, `start`, …) are the
/// exception: they must survive thread end (user rule). For those the guard is
/// created with `detach: true` and Drop is a no-op - the docker CLI child exits
/// on its own right after handing the containers to the daemon, and the
/// containers themselves are managed by the daemon, not by this process.
struct KillOnDrop {
    child: Option<tokio::process::Child>,
    detach: bool,
}

impl KillOnDrop {
    fn new(child: tokio::process::Child, detach: bool) -> Self {
        Self {
            child: Some(child),
            detach,
        }
    }

    /// Take the child's stdin, e.g. to pipe a script into the process.
    fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Wait for the child to exit and collect its output.
    ///
    /// The child stays owned by the guard for the whole wait, so if THIS future
    /// is dropped mid-wait (cancel notification, timeout, plugin shutdown), the
    /// Drop impl still kills the child - a bare `Child::wait_with_output` would
    /// lose the handle and leak the process.
    async fn wait_with_output(&mut self) -> std::io::Result<std::process::Output> {
        let child = self.child.as_mut().expect("wait_with_output called twice");
        let mut stdout_data = Vec::new();
        let mut stderr_data = Vec::new();
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let read_stdout = async {
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut stdout_data).await;
            }
        };
        let read_stderr = async {
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = tokio::io::AsyncReadExt::read_to_end(pipe, &mut stderr_data).await;
            }
        };
        tokio::join!(read_stdout, read_stderr);

        let status = child.wait().await?;
        Ok(std::process::Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Detached ops (`up -d`, `run -d`, `start`) survive thread end.
        if self.detach {
            return;
        }
        if let Some(mut child) = self.child.take() {
            // `Child::kill`/`Child::wait` are async - spawn a fire-and-forget
            // kill+reap task. If no runtime is active (process teardown on a
            // non-async thread), there is nothing safe we can do; the child is
            // orphaned and will be reaped by the OS eventually.
            #[cfg(unix)]
            {
                let pid = child.id();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        // Reap the whole process group (`docker compose → docker`
                        // chain); the child was spawned with process_group(0), so
                        // its pgid == its pid. Negative pid signals the group.
                        if let Some(pid) = pid {
                            let _ = tokio::process::Command::new("kill")
                                .args(["-KILL", "--", &format!("-{pid}")])
                                .status()
                                .await;
                        }
                    });
                }
            }
            #[cfg(not(unix))]
            {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                    });
                }
            }
        }
    }
}

/// A compose command is DETACHED (must survive thread-end) when it carries an
/// explicit detach flag (`-d` / `--detach`) or is `start`. Everything else is a
/// foreground/tracked command whose subprocess must be killed when the thread
/// that issued it ends.
fn is_detached_command(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let verb = words.next().unwrap_or("");
    let has_detach_flag = words.any(|w| w == "-d" || w == "--detach");
    verb == "start" || has_detach_flag
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
    let compose_files_arg: Vec<String> = match &args["compose_file"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => vec![],
    };
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

    // Resolve compose_file(s): each relative to project_dir or absolute inside
    // workspace. Default docker-compose.yml when omitted. Multiple entries become
    // repeated -f flags (base first, overrides after) - like
    // `docker compose -f base -f overlay`.
    let mut resolved_compose_files: Vec<String> = Vec::new();
    if compose_files_arg.is_empty() {
        resolved_compose_files.push(resolve_project_file(
            "docker-compose.yml",
            &project_dir,
            configured_workspace,
            "compose_file",
        )?);
    } else {
        for cf in &compose_files_arg {
            resolved_compose_files.push(resolve_project_file(
                cf,
                &project_dir,
                configured_workspace,
                "compose_file",
            )?);
        }
    }

    // Resolve env_file: relative to project_dir or absolute inside workspace, optional.
    let resolved_env_file = if env_file_arg.is_empty() {
        String::new()
    } else {
        resolve_project_file(
            &env_file_arg,
            &project_dir,
            configured_workspace,
            "env_file",
        )?
    };

    let verb = command.split_whitespace().next().unwrap_or("");
    // NO default timeout (Aug 2026 rule: fixed tool timeouts were removed -
    // background tasks + wait-task give the agent full tracking/cancel/log
    // control; a tool must never be killed by an invisible clock the agent
    // didn't set). Only an EXPLICIT `timeout` param bounds the command.
    let timeout_secs = timeout_override;

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
        &resolved_compose_files,
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

    // Whether this command is an explicit detached op that must survive
    // thread-end (`up -d`, `run -d`, `start`) - everything else is tracked and
    // its subprocess must be killed when the thread ends.
    let detach = is_detached_command(&command);

    // If script is provided, pipe it via stdin
    if verb == "exec" && !raw_script.is_empty() {
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::piped());
        // Own process group so kill-on-drop can reap the whole local chain.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = KillOnDrop::new(cmd.spawn()?, detach);

        // Write script to stdin
        if let Some(mut stdin) = child.take_stdin() {
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

    // Standard execution (no script piped via stdin). Spawn explicitly so the
    // child is wrapped in KillOnDrop: a plain `cmd.output()` would spawn
    // internally and give us no handle to kill when the request is cancelled
    // or the thread ends.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Own process group so kill-on-drop can reap the whole local chain.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = KillOnDrop::new(cmd.spawn()?, detach);
    let result = match timeout_secs {
        Some(secs) => {
            match tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()).await {
                Ok(Ok(output)) => Ok(Ok(output)),
                Ok(Err(e)) => Ok(Err(e)),
                Err(_elapsed) => Err(secs),
            }
        }
        None => match child.wait_with_output().await {
            Ok(output) => Ok(Ok(output)),
            Err(e) => Ok(Err(e)),
        },
    };

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
        Err(secs) => Ok((
            format!(
                "docker compose command timed out after {}s (explicit 'timeout' param was set)\n\nCommand:\n{}",
                secs, cmd_display,
            ),
            true,
        )),
    }
}

// ---------------------------------------------------------------------------
// Plugin config - received via MCP configure message, not from env vars
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
            let mut cfg = config.lock();
            if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.workspace_dir = dir.to_string();
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
                 'project_dir' (required) is the compose project directory - the workspace dir or a subdirectory of it. \
                 'compose_file' (optional) is the compose file(s): a STRING or ARRAY of strings - each a path RELATIVE \\
                 to project_dir (default docker-compose.yml when omitted), or an ABSOLUTE path anywhere inside the \\
                 workspace (e.g. a shared overlay). Multiple entries are passed as repeated -f flags in order \\
                 (e.g. [\"docker-compose.yml\", \"docker-compose.dev.yml\"] -> -f docker-compose.yml -f docker-compose.dev.yml), \\
                 so overrides merge exactly like `docker compose -f base -f overlay`. \\
                 'env_file' (optional) is a .env-style file: relative to project_dir, or an ABSOLUTE path anywhere inside \\
                 the workspace (e.g. /opt/workspace/<project>/<name>.env for a shared env file) - passed via --env-file. \
                 PROJECT NAME: pinned to COMPOSE_PROJECT_NAME from the env_file when present; otherwise the compose \
                 file's own name: (default) or the project-dir basename - ambient COMPOSE_* env vars are ignored, so \
                 the tool can NEVER inherit the agent's own (production) project name by accident. \
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
                 Optional 'timeout' parameter sets an explicit timeout for long-running commands; \
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
                        "type": ["string", "array"],
                        "items": {"type": "string"},
                        "description": "Compose file(s): a string or array of strings, each relative to project_dir (default docker-compose.yml) or an absolute path inside the workspace. Multiple entries are passed as repeated -f flags in order (base first, overrides after)."
                    },
                    "env_file": {
                        "type": "string",
                        "description": ".env-style file: relative to project_dir, or an absolute path anywhere inside the workspace. Passed via --env-file."
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
                        "description": "Optional - explicit timeout in seconds for this command. When omitted there is NO timeout: the command runs until it finishes, errors, or the agent cancels it (use the background task system: poll-task/wait-task/cancel-task)."
                    }
                },
                "required": ["project_dir", "command"]
            }),
        },
        handler: soft_error_async(move |args: Value, _meta: Option<McpMeta>| {
            let c = c1.clone();
            async move {
                let config = c.lock().clone();
                handle_compose(args, &config).await
            }
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

    #[test]
    fn detach_detection() {
        // Explicit detached ops survive thread end.
        assert!(is_detached_command("up -d"));
        assert!(is_detached_command("up --detach"));
        assert!(is_detached_command("run -d backend sleep 300"));
        assert!(is_detached_command("start"));
        // Foreground/tracked commands must be killed on thread end.
        assert!(!is_detached_command("up"));
        assert!(!is_detached_command("build"));
        assert!(!is_detached_command("exec -T backend cargo build"));
        assert!(!is_detached_command("logs --tail=50"));
        assert!(!is_detached_command("ps"));
        assert!(!is_detached_command("down"));
    }

    /// Is a PID still alive? (Linux: /proc/<pid> exists while the task lives.)
    fn is_process_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Poll up to `timeout` for the process to exit.
    async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if !is_process_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn kill_on_drop_kills_child() {
        // Tracked (foreground) child: dropping the guard must kill the process
        // tree so no stale `docker compose exec …` survives the thread.
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child pid");
        assert!(is_process_alive(pid), "child should be alive before drop");

        let guard = KillOnDrop::new(child, false);
        drop(guard);

        assert!(
            wait_for_exit(pid, Duration::from_secs(5)).await,
            "tracked child must be dead after guard drop (pid {pid} still alive)"
        );
    }

    #[tokio::test]
    async fn kill_on_drop_preserves_detached() {
        // Detached op (`up -d`, `run -d`, `start`): dropping the guard must NOT
        // kill the child - it is an explicit background operation.
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child pid");
        assert!(is_process_alive(pid), "child should be alive before drop");

        let guard = KillOnDrop::new(child, true);
        drop(guard);

        // Give a hypothetical kill task time to run; the detached child must
        // still be alive.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            is_process_alive(pid),
            "detached child must survive guard drop (pid {pid})"
        );

        // Cleanup so the test leaves no stray process.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    // -----------------------------------------------------------------------
    // resolve_project_file sandbox (R8): compose/env files may live anywhere
    // inside the configured workspace_dir, not only inside project_dir.
    // -----------------------------------------------------------------------

    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Throwaway layout: <tmp>/r8_<tag>_<pid>/ containing
    /// workspace/{proj/docker-compose.yml, proj/sub/compose.yml, proj.env},
    /// workspace/envs/omnidev.env, and an `outside/evil.env` sibling that
    /// escapes the workspace. Returns (guard, workspace_root, project_dir).
    fn make_layout(tag: &str) -> (TempDir, String, String) {
        let root = std::env::temp_dir().join(format!("r8_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let project = workspace.join("proj");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        std::fs::create_dir_all(workspace.join("envs")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(project.join("docker-compose.yml"), "services: {}\n").unwrap();
        std::fs::write(project.join("sub").join("compose.yml"), "services: {}\n").unwrap();
        std::fs::write(workspace.join("envs").join("omnidev.env"), "A=1\n").unwrap();
        std::fs::write(workspace.join("proj.env"), "A=1\n").unwrap();
        std::fs::write(root.join("outside").join("evil.env"), "A=1\n").unwrap();
        let ws = workspace
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let proj = project
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        (TempDir(root), ws, proj)
    }

    #[test]
    fn env_file_inside_workspace_but_outside_project_dir_is_accepted() {
        // Use case: project_dir=omni-stack, env_file=/opt/workspace/omni-deployer/omnidev.env
        let (_tmp, ws, proj) = make_layout("env_outside_proj");
        let env_file = format!("{ws}/envs/omnidev.env");
        let resolved = resolve_project_file(&env_file, &proj, &ws, "env_file").unwrap();
        assert_eq!(
            Path::new(&resolved),
            Path::new(&env_file).canonicalize().unwrap()
        );
    }

    #[test]
    fn absolute_path_inside_workspace_is_accepted() {
        let (_tmp, ws, proj) = make_layout("abs_inside");
        let compose = format!("{proj}/docker-compose.yml");
        let resolved = resolve_project_file(&compose, &proj, &ws, "compose_file").unwrap();
        assert_eq!(
            Path::new(&resolved),
            Path::new(&compose).canonicalize().unwrap()
        );
    }

    #[test]
    fn file_outside_workspace_is_rejected() {
        let (_tmp, ws, proj) = make_layout("outside_ws");
        let outside = Path::new(&ws)
            .parent()
            .unwrap()
            .join("outside")
            .join("evil.env");
        let err =
            resolve_project_file(outside.to_str().unwrap(), &proj, &ws, "env_file").unwrap_err();
        assert!(
            err.to_string().contains("workspace"),
            "expected workspace sandbox error, got: {err}"
        );
        // `..` traversal from inside the project must be rejected too.
        let err =
            resolve_project_file("../../outside/evil.env", &proj, &ws, "env_file").unwrap_err();
        assert!(
            err.to_string().contains("workspace"),
            "expected workspace sandbox error, got: {err}"
        );
    }

    #[test]
    fn relative_compose_file_inside_project_dir_still_works() {
        let (_tmp, ws, proj) = make_layout("rel_backcompat");
        let resolved =
            resolve_project_file("docker-compose.yml", &proj, &ws, "compose_file").unwrap();
        assert_eq!(
            Path::new(&resolved),
            Path::new(&proj)
                .join("docker-compose.yml")
                .canonicalize()
                .unwrap()
        );
        // Subdirectory-relative paths keep working.
        let resolved = resolve_project_file("sub/compose.yml", &proj, &ws, "compose_file").unwrap();
        assert_eq!(
            Path::new(&resolved),
            Path::new(&proj)
                .join("sub")
                .join("compose.yml")
                .canonicalize()
                .unwrap()
        );
    }

    #[test]
    fn build_compose_command_emits_one_dash_f_per_file() {
        // Multiple compose files -> repeated -f flags in order (base, overlay).
        let cmd = build_compose_command(
            "exec",
            "/tmp/proj",
            &[
                "/tmp/proj/docker-compose.yml".to_string(),
                "/tmp/proj/docker-compose.dev.yml".to_string(),
            ],
            "",
            "omniagent",
            "cargo build",
            "",
        )
        .unwrap();
        let repr = format!("{:?}", cmd);
        // Expect: docker compose -f /tmp/proj/docker-compose.yml -f /tmp/proj/docker-compose.dev.yml exec ...
        // (Debug format: "docker" "compose" "-f" "/tmp/proj/docker-compose.yml" "-f" "/tmp/proj/docker-compose.dev.yml" ...)
        let dash_f_count = repr.matches("-f").count();
        assert_eq!(dash_f_count, 2, "expected exactly 2 -f flags: {repr}");
        assert!(
            repr.contains("docker-compose.yml"),
            "base file missing: {repr}"
        );
        assert!(
            repr.contains("docker-compose.dev.yml"),
            "overlay file missing: {repr}"
        );
        // Base file must appear before the overlay file (order preserved).
        let base_pos = repr.find("docker-compose.yml").expect("base file position");
        let dev_pos = repr
            .find("docker-compose.dev.yml")
            .expect("overlay file position");
        assert!(base_pos < dev_pos, "base must precede overlay: {repr}");
    }

    #[test]
    fn build_compose_command_pins_project_directory() {
        let cmd =
            build_compose_command("ps", "/opt/workspace/omni-stack", &[], "", "", "", "").unwrap();
        let repr = format!("{:?}", cmd);
        assert!(
            repr.contains("--project-directory"),
            "missing --project-directory: {repr}"
        );
        assert!(repr.contains("/opt/workspace/omni-stack"));
    }

    #[test]
    fn env_file_project_name_parsing() {
        let dir = std::env::temp_dir().join(format!("compose-env-pn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join("dev.env");
        std::fs::write(&env, "# comment\nCOMPOSE_PROJECT_NAME=omnidev\nOTHER=1\n").unwrap();
        assert_eq!(
            env_file_project_name(env.to_str().unwrap()).as_deref(),
            Some("omnidev")
        );
        std::fs::write(&env, "export COMPOSE_PROJECT_NAME=\"omni-stack\"\n").unwrap();
        assert_eq!(
            env_file_project_name(env.to_str().unwrap()).as_deref(),
            Some("omni-stack")
        );
        std::fs::write(&env, "FOO=bar\n").unwrap();
        assert_eq!(env_file_project_name(env.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_compose_command_pins_project_name_from_env_file() {
        let dir = std::env::temp_dir().join(format!("compose-pn-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = dir.join("omnidev.env");
        std::fs::write(&env, "COMPOSE_PROJECT_NAME=omnidev\n").unwrap();
        let cmd = build_compose_command(
            "ps",
            "/tmp/proj",
            &[],
            env.to_str().unwrap(),
            "",
            "",
            "",
        )
        .unwrap();
        let repr = format!("{:?}", cmd);
        assert!(
            repr.contains("--project-name"),
            "missing --project-name: {repr}"
        );
        assert!(repr.contains("omnidev"));
        assert!(repr.contains("--env-file"));
        assert!(repr.contains("--project-directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_compose_command_without_env_file_has_no_project_name() {
        let cmd = build_compose_command("ps", "/tmp/proj", &[], "", "", "", "").unwrap();
        let repr = format!("{:?}", cmd);
        assert!(
            !repr.contains("--project-name"),
            "unexpected --project-name: {repr}"
        );
    }
}
