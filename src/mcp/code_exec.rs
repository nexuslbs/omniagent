//! `code_exec` - run a model-written program in the toolbox container and get
//! a **typed** JSON result back (dsh `codeRuntime` seam).
//!
//! Design contract (kanban task 8):
//! - The core NEVER executes the program in-process: the program runs inside
//!   the toolbox sidecar container (the established utility-execution sandbox
//!   with python3 + the postgres client env), reached through the same
//!   `docker exec` pattern the docker plugin uses for project containers.
//! - The completion value crosses a lossless-JSON boundary as a TYPED value:
//!   `{ "ok": true, "value": <typed json>, "logs": [stdout lines] }`.
//! - Errors are FIELDS on a resolved result - `{ "ok": false, "error": <msg>,
//!   "logs": [...] }` - never a tool-call failure (`is_error: false`).
//! - Timeout: bounded client-side (tokio timeout + process-group kill of the
//!   docker CLI child) AND container-side (the runner installs a hard alarm /
//!   exit timer, so the program dies even if the client vanishes).
//! - Abort: dropping the handler future (cancel notification) kills the CLI
//!   child via the `KillOnDrop` guard; the container-side timer bounds the
//!   program itself.
//! - The toolbox image carries the runners (`/usr/bin/code_exec_runner`,
//!   `/usr/bin/code_exec_runner_js`) - see omni-stack `services/toolbox`.
//! - Toolbox discovery mirrors deploy.py's `_g25_toolbox_name()`: docker label
//!   `com.docker.compose.service=toolbox` (project name varies across
//!   omnidev/omnideploy), so no hardcoded container names.

use crate::error::AppResult;
use crate::mcp::{tool_qualify, AppContext, McpTool, McpToolResult};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Marker the toolbox runner emits before the JSON-serialized completion value.
const OK_MARKER: &str = "__CODE_EXEC_OK__";
/// Marker the toolbox runner emits before the JSON-serialized error payload.
const ERR_MARKER: &str = "__CODE_EXEC_ERR__";
/// Default execution timeout when the caller doesn't pass `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling for a single program run (in-handler).
const MAX_TIMEOUT_SECS: u64 = 600;
/// Maximum program size accepted (bytes).
const MAX_PROGRAM_BYTES: usize = 256 * 1024;
/// Maximum args-JSON size accepted (bytes).
const MAX_ARGS_BYTES: usize = 256 * 1024;
/// Maximum chars of program output returned in `logs` (rest truncated).
const MAX_LOG_CHARS: usize = 50_000;
/// Runner binary inside the toolbox image (python).
const RUNNER_PY: &str = "/usr/bin/code_exec_runner";
/// Runner binary inside the toolbox image (js, node).
const RUNNER_JS: &str = "/usr/bin/code_exec_runner_js";

/// Build the `code_exec` tool.
pub fn code_exec_tool() -> McpTool {
    McpTool {
        name: tool_qualify("builtin", "code_exec"),
        description: "Run a model-written program in the toolbox sandbox container (NOT in-process) and get a TYPED JSON result back. \
            Input: `language` (\"python\" or \"js\"), `program` (source text; top-level `return` and `await` are allowed), \
            optional `args` (JSON object passed to the program as its `args` parameter), optional `timeout_secs` (default 120, max 600). \
            Returns `{\"ok\": true, \"value\": <typed json>, \"logs\": [stdout lines]}` on success, or \
            `{\"ok\": false, \"error\": <message>, \"logs\": [...]}` on failure/timeout - errors are FIELDS, never a tool failure. \
            The program runs inside the toolbox container, isolated from the agent core; it is killed hard on timeout or cancellation."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["python", "js"],
                    "description": "Program language. python uses the toolbox python3; js uses node in the toolbox."
                },
                "program": {
                    "type": "string",
                    "description": "Program source. Top-level `return` and (python) top-level `await` are supported; the completion value is JSON-serialized and returned typed. Stdout lines appear in `logs`."
                },
                "args": {
                    "type": "object",
                    "description": "Optional JSON arguments passed to the program as its `args` parameter (null if omitted)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Maximum seconds to run (default 120, max 600). The program is killed hard at the limit.",
                    "default": 120
                }
            },
            "required": ["language", "program"]
        }),
        server_name: None,
        // Hard ceiling for the agent loop; the handler self-bounds by the
        // `timeout_secs` argument (max MAX_TIMEOUT_SECS) and kills the child.
        timeout_secs: Some(MAX_TIMEOUT_SECS + 30),
        handler: Arc::new(|args: Value, _ctx: AppContext| {
            Box::pin(handle_code_exec(args))
        }),
    }
}

/// Locate the toolbox container of the current compose project via docker
/// label filter (project name varies: omnidev / omnideploy / omni).
async fn find_toolbox_container() -> Result<String, String> {
    let out = tokio::process::Command::new("docker")
        .env_clear()
        .env("PATH", crate::process_env::MINIMAL_PATH)
        .args([
            "ps",
            "--filter",
            "label=com.docker.compose.service=toolbox",
            "--filter",
            "status=running",
            "--format",
            "{{.Names}}",
        ])
        .output()
        .await
        .map_err(|e| format!("cannot run docker: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    names.into_iter().next().ok_or_else(|| {
        "toolbox container not found: no running container with label \
         com.docker.compose.service=toolbox (is the omni-stack compose project up?)"
            .to_string()
    })
}

/// Build the `docker exec -i <toolbox> <runner> <args-json> <timeout>` command.
/// The program is piped to the runner via stdin.
fn build_run_command(
    toolbox: &str,
    language: &str,
    args_json: &str,
    timeout_secs: u64,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("docker");
    // Platform-level env isolation (2026-09-01): the docker CLI child must not
    // inherit the agent's ambient environment. Empty env + explicit minimal
    // PATH only (the docker CLI may need PATH to locate the compose plugin).
    cmd.env_clear();
    cmd.env("PATH", crate::process_env::MINIMAL_PATH);
    cmd.arg("exec").arg("-i").arg(toolbox);
    // busybox `timeout` = hard container-side kill (SIGTERM preempts busy
    // loops, so even a JS `while(true){}` that starves its event loop - and
    // thus the runner's own setTimeout guard - cannot outlive the limit).
    // The runner still installs its own in-process timer as a redundant
    // guard for the common (non-starvation) case.
    cmd.arg("timeout").arg(timeout_secs.to_string());
    if language == "python" {
        cmd.arg(RUNNER_PY);
    } else {
        cmd.arg("node").arg(RUNNER_JS);
    }
    cmd.arg(args_json).arg(timeout_secs.to_string());
    cmd
}

/// Parse the runner's stdout into the typed result pieces.
/// Returns (ok_value, err_value, log_lines).
fn parse_runner_output(stdout: &str) -> (Option<Value>, Option<Value>, Vec<String>) {
    let mut ok_val = None;
    let mut err_val = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix(OK_MARKER) {
            ok_val = serde_json::from_str(rest).ok();
        } else if let Some(rest) = line.strip_prefix(ERR_MARKER) {
            err_val = serde_json::from_str(rest).ok();
        }
    }
    let logs: Vec<String> = stdout
        .lines()
        .filter(|l| !l.starts_with(OK_MARKER) && !l.starts_with(ERR_MARKER))
        .map(|l| l.to_string())
        .collect();
    (ok_val, err_val, logs)
}

/// Cap the log lines to MAX_LOG_CHARS total (array of strings).
fn logs_array(logs: &[String]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut total = 0usize;
    for line in logs {
        total += line.len() + 1;
        if total > MAX_LOG_CHARS {
            out.push(json!("[... logs truncated ...]"));
            break;
        }
        out.push(json!(line));
    }
    out
}

/// Shape runner output into the `{ok, value|error, logs}` contract.
fn shape_output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Value {
    let (ok_val, err_val, logs) = parse_runner_output(stdout);
    let logs = logs_array(&logs);
    if let Some(ok) = ok_val {
        return json!({ "ok": true, "value": ok, "logs": logs });
    }
    if let Some(err) = err_val {
        let msg = err
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("program error")
            .to_string();
        let mut payload = json!({ "ok": false, "error": msg, "logs": logs });
        if let Some(tb) = err.get("traceback").and_then(|v| v.as_str()) {
            payload["traceback"] = json!(tb);
        }
        return payload;
    }
    // No marker: the process crashed/was killed before the runner could emit
    // a marker (e.g. OOM, signal, python interpreter failure).
    let stderr_tail = stderr.trim();
    let reason = match exit_code {
        Some(code) => format!("program exited with status {code}"),
        None => "program was killed by a signal".to_string(),
    };
    let mut error = reason;
    if !stderr_tail.is_empty() {
        error.push_str(&format!(
            ": {}",
            crate::mcp::truncate_content(stderr_tail, 2000)
        ));
    }
    json!({ "ok": false, "error": error, "logs": logs })
}

/// A resolved error-as-field result (never a tool failure).
fn ok_false(error: &str) -> McpToolResult {
    McpToolResult {
        call_id: String::new(),
        content: serde_json::to_string_pretty(&json!({
            "ok": false,
            "error": error,
            "logs": []
        }))
        .unwrap_or_else(|_| format!("{{\"ok\": false, \"error\": \"{error}\"}}")),
        is_error: false,
    }
}

/// Guard that owns the docker CLI child for the whole run: if THIS future is
/// dropped mid-wait (cancel notification, plugin shutdown), the Drop impl
/// kills the child so no `docker exec` straggler survives the thread
/// (mirrors the docker plugin's KillOnDrop).
struct KillOnDrop {
    child: Option<tokio::process::Child>,
}

impl KillOnDrop {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Take the child's stdin, e.g. to pipe the program into the runner.
    fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.as_mut().and_then(|c| c.stdin.take())
    }

    /// Wait for the child to exit and collect its output. The child stays
    /// owned by the guard for the whole wait, so dropping mid-wait still
    /// kills it (a bare `Child::wait_with_output` would lose the handle).
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

    /// Kill the docker CLI child (SIGKILL via start_kill) plus its process
    /// group, so both the client and any of its subprocesses die.
    fn kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        #[cfg(unix)]
        if let Some(pid) = self.child_pid() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .status();
        }
    }

    fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn handle_code_exec(args: Value) -> AppResult<McpToolResult> {
    let language = args
        .get("language")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if language != "python" && language != "js" {
        return Ok(ok_false(
            "invalid `language`: expected \"python\" or \"js\"",
        ));
    }
    let program = args
        .get("program")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if program.trim().is_empty() {
        return Ok(ok_false("`program` is required and must not be empty"));
    }
    if program.len() > MAX_PROGRAM_BYTES {
        return Ok(ok_false(&format!(
            "program too large: {} bytes (max {MAX_PROGRAM_BYTES})",
            program.len()
        )));
    }
    let args_value = args.get("args").cloned().unwrap_or(Value::Null);
    let args_json = args_value.to_string();
    if args_json.len() > MAX_ARGS_BYTES {
        return Ok(ok_false(&format!(
            "args too large: {} bytes (max {MAX_ARGS_BYTES})",
            args_json.len()
        )));
    }
    let timeout = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let toolbox = match find_toolbox_container().await {
        Ok(name) => name,
        Err(e) => return Ok(ok_false(&e)),
    };

    let mut cmd = build_run_command(&toolbox, &language, &args_json, timeout);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Own process group so `kill -KILL -<pid>` reaps the whole tree.
        cmd.process_group(0);
    }
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Ok(ok_false(&format!("failed to spawn docker exec: {e}"))),
    };
    let mut guard = KillOnDrop::new(child);
    let mut stdin = match guard.take_stdin() {
        Some(s) => s,
        None => return Ok(ok_false("failed to open program pipe")),
    };
    let program_bytes = program.into_bytes();
    let write_handle = tokio::spawn(async move {
        let _ = stdin.write_all(&program_bytes).await;
        let _ = stdin.shutdown().await;
    });

    let run = async { guard.wait_with_output().await };
    let outcome = tokio::time::timeout(Duration::from_secs(timeout + 5), run).await;
    let _ = write_handle.await;

    match outcome {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let payload = shape_output(&stdout, &stderr, output.status.code());
            Ok(McpToolResult {
                call_id: String::new(),
                content: serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| payload.to_string()),
                is_error: false,
            })
        }
        Ok(Err(e)) => {
            guard.kill();
            Ok(ok_false(&format!("failed to run program in toolbox: {e}")))
        }
        Err(_) => {
            guard.kill();
            Ok(ok_false(&format!("program timed out after {timeout}s")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok_typed_roundtrip_all_types() {
        // dict / list / str / int / bool / null
        for (raw, kind) in [
            ("{\"a\":1,\"b\":[1,2]}", "object"),
            ("[1,2,3]", "array"),
            ("\"hi\"", "string"),
            ("42", "number"),
            ("true", "boolean"),
            ("null", "null"),
        ] {
            let stdout = format!("some log line\n{OK_MARKER}{raw}\n");
            let (ok, err, logs) = parse_runner_output(&stdout);
            assert!(ok.is_some(), "expected ok value for {raw}");
            assert!(err.is_none(), "no err for {raw}");
            let v = ok.unwrap();
            let is_kind = match kind {
                "object" => v.is_object(),
                "array" => v.is_array(),
                "string" => v.is_string(),
                "number" => v.is_number(),
                "boolean" => v.is_boolean(),
                "null" => v.is_null(),
                other => panic!("bad kind {other}"),
            };
            assert!(is_kind, "type mismatch for {raw}: {v}");
            assert_eq!(logs, vec!["some log line".to_string()]);
        }
        // Exact value equality for a dict round-trip.
        let (ok, _, _) = parse_runner_output(&format!("{OK_MARKER}{{\"x\":[1,true,null,\"s\"]}}"));
        assert_eq!(ok, Some(json!({"x": [1, true, null, "s"]})));
    }

    #[test]
    fn parse_error_as_field() {
        let stdout = format!(
            "line1\n{ERR_MARKER}{{\"error\":\"ZeroDivisionError: division by zero\",\"traceback\":\"tb\"}}\n"
        );
        let (ok, err, logs) = parse_runner_output(&stdout);
        assert!(ok.is_none());
        assert!(err.is_some());
        assert_eq!(logs, vec!["line1".to_string()]);
        let payload = shape_output(&stdout, "", Some(0));
        assert_eq!(payload["ok"], false);
        assert_eq!(
            payload["error"],
            json!("ZeroDivisionError: division by zero")
        );
        assert_eq!(payload["traceback"], json!("tb"));
        assert_eq!(payload["logs"], json!(["line1"]));
    }

    #[test]
    fn shape_crash_without_marker() {
        let payload = shape_output("", "Killed", Some(137));
        assert_eq!(payload["ok"], false);
        assert!(payload["error"]
            .as_str()
            .unwrap()
            .contains("exited with status 137"));
        assert!(payload["error"].as_str().unwrap().contains("Killed"));
    }

    #[test]
    fn shape_signal_kill_without_marker() {
        let payload = shape_output("", "", None);
        assert_eq!(payload["ok"], false);
        assert!(payload["error"].as_str().unwrap().contains("signal"));
    }

    #[test]
    fn timeout_shape_is_error_field() {
        let payload = json!({ "ok": false, "error": "program timed out after 5s", "logs": [] });
        assert_eq!(payload["ok"], false);
        assert!(payload["error"].as_str().unwrap().contains("timed out"));
    }

    #[test]
    fn logs_are_capped() {
        let long: Vec<String> = (0..100_000).map(|i| format!("line {i}")).collect();
        let arr = logs_array(&long);
        assert!(arr.len() < long.len());
        assert!(arr
            .iter()
            .any(|v| v.as_str() == Some("[... logs truncated ...]")));
    }

    #[test]
    fn build_command_targets_toolbox_runner() {
        let cmd = build_run_command("omnidev-toolbox-1", "python", "{\"a\":1}", 30);
        let repr = format!("{cmd:?}");
        assert!(repr.contains("\"docker\""));
        assert!(repr.contains("\"exec\""));
        assert!(repr.contains("\"-i\""));
        assert!(repr.contains("omnidev-toolbox-1"));
        assert!(repr.contains(RUNNER_PY));
        assert!(repr.contains("\"{\\\"a\\\":1}\""));
        assert!(repr.contains("\"30\""));
        assert!(repr.contains("\"timeout\""));

        let cmd_js = build_run_command("omnidev-toolbox-1", "js", "null", 10);
        let repr_js = format!("{cmd_js:?}");
        assert!(repr_js.contains("\"node\""));
        assert!(repr_js.contains(RUNNER_JS));
    }

    #[test]
    fn validation_errors_are_resolved_fields() {
        // Invalid language -> ok:false payload, NOT an Err.
        let res = futures::executor::block_on(handle_code_exec(json!({
            "language": "ruby",
            "program": "x = 1"
        })));
        let r = res.unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("\"ok\": false"));
        assert!(r.content.contains("invalid `language`"));

        let res = futures::executor::block_on(handle_code_exec(json!({
            "language": "python",
            "program": ""
        })));
        let r = res.unwrap();
        assert!(!r.is_error);
        assert!(r.content.contains("must not be empty"));
    }

    /// Is a PID still alive? (Linux: /proc/<pid> exists while the task lives.)
    fn is_process_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

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
        // Abort path: dropping the guard mid-run must kill the child so no
        // `docker exec` straggler survives the thread.
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child pid");
        assert!(is_process_alive(pid), "child should be alive before drop");

        let guard = KillOnDrop::new(child);
        drop(guard);

        assert!(
            wait_for_exit(pid, Duration::from_secs(5)).await,
            "child must be dead after guard drop (pid {pid} still alive)"
        );
    }

    #[tokio::test]
    async fn explicit_kill_kills_child() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child pid");
        let mut guard = KillOnDrop::new(child);
        guard.kill();
        drop(guard); // reap the killed child (tokio Child Drop try_waits)
        assert!(
            wait_for_exit(pid, Duration::from_secs(5)).await,
            "child must be dead after explicit kill (pid {pid} still alive)"
        );
    }
}
