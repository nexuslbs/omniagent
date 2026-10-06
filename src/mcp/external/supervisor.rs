//! Child-liveness supervision for external MCP servers.
//!
//! Before this module an external (stdio) MCP server that died at runtime was
//! INVISIBLE: the reader task logged a bare `closed stdout` at info level, no
//! exit status, no pid, no stderr tail and no recovery. The plugin's tools then
//! either disappeared from the registry (a reload while the child was dead) or
//! kept failing with a vague message, and every thread saw a bare
//! `Unknown tool: <plugin>__<tool>` with no way to tell why (production
//! incident 2026-09-27: an external bridge tool).
//!
//! This module is the single, process-wide record of what each external MCP
//! server is actually doing:
//!
//! * a lifecycle state per server (`never started` / `starting` / `running` /
//!   `crashed` / `restarting` / `failed`),
//! * the exit record of the last crash (when, pid, exit code or signal, stderr
//!   tail),
//! * the captured stderr tail of the child (bounded ring buffer),
//! * the supervised-restart bookkeeping (attempt, backoff),
//! * and a `revive` hook the server layer installs so a respawned child gets
//!   its tools re-registered in the MCP registry.
//!
//! Precedent for a process-wide runtime status registry: the platform plugins
//! self-report through `crate::platform::external::platform_runtime_status`,
//! which the plugin API already surfaces.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::RwLock;

/// How many stderr lines of a child are kept per server.
pub const STDERR_TAIL_LINES: usize = 40;

/// Supervised restart attempts before the supervisor gives up LOUDLY.
pub const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Backoff before restart attempt `attempt` (1-based): 1s, 2s, 4s, 8s, 16s.
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_secs(1u64 << shift)
}

/// Lifecycle state of one external MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessState {
    /// Configured/enabled but never spawned (or supervision was cleared).
    NeverStarted,
    /// Spawn in progress / handshake not finished yet.
    Starting,
    /// Child alive and handshake done.
    Running,
    /// Child exited unexpectedly; kept until a restart succeeds or gives up.
    Crashed,
    /// Supervised restart in progress (attempt N, backoff).
    Restarting,
    /// Restarts exhausted: the server stays down until a manual restart.
    Failed,
}

impl LivenessState {
    pub fn as_str(self) -> &'static str {
        match self {
            LivenessState::NeverStarted => "never_started",
            LivenessState::Starting => "starting",
            LivenessState::Running => "running",
            LivenessState::Crashed => "crashed",
            LivenessState::Restarting => "restarting",
            LivenessState::Failed => "failed",
        }
    }
}

/// The recorded exit of a crashed MCP child.
#[derive(Debug, Clone)]
pub struct ExitRecord {
    /// Unix seconds when the exit was observed.
    pub at: u64,
    /// OS pid of the dead child (when known).
    pub pid: Option<u32>,
    /// `exit code 137`, `signal: 9 (SIGKILL)`, `wait error: ...`.
    pub status: String,
    /// Captured stderr tail at exit time.
    pub stderr_tail: Vec<String>,
}

/// A snapshot of one server's liveness.
#[derive(Debug, Clone)]
pub struct ServerLiveness {
    pub state: LivenessState,
    pub pid: Option<u32>,
    pub tool_count: usize,
    /// Number of successful supervised restarts so far.
    pub restarts: u32,
    pub exit: Option<ExitRecord>,
    pub restart_attempt: Option<u32>,
    pub restart_delay_secs: Option<u64>,
    pub last_error: Option<String>,
    /// Unix seconds of the last state transition.
    pub changed_at: u64,
    /// Human-readable one-line explanation of the current state.
    pub message: String,
}

static SERVERS: Lazy<RwLock<HashMap<String, ServerLiveness>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

static STDERR_TAILS: Lazy<RwLock<HashMap<String, VecDeque<String>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

type ReviveHook = Arc<dyn Fn(String) + Send + Sync>;

static REVIVE_HOOK: Lazy<RwLock<Option<ReviveHook>>> = Lazy::new(|| RwLock::new(None));

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Timestamp helper: unix seconds plus a readable UTC form.
fn fmt_ts(secs: u64) -> String {
    use chrono::{TimeZone, Utc};
    match Utc.timestamp_opt(secs as i64, 0).single() {
        Some(t) => t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        None => format!("unix:{secs}"),
    }
}

/// Store/refresh the summary state of one server (internal).
fn put(
    server: &str,
    state: LivenessState,
    message: String,
    update: impl FnOnce(&mut ServerLiveness),
) {
    let mut map = SERVERS.write();
    let entry = map
        .entry(server.to_string())
        .or_insert_with(|| ServerLiveness {
            state: LivenessState::NeverStarted,
            pid: None,
            tool_count: 0,
            restarts: 0,
            exit: None,
            restart_attempt: None,
            restart_delay_secs: None,
            last_error: None,
            changed_at: now_secs(),
            message: String::new(),
        });
    entry.state = state;
    entry.message = message;
    entry.changed_at = now_secs();
    update(entry);
}

/// Forget a server (plugin disabled/removed, or a fresh client registered).
pub fn clear(server: &str) {
    SERVERS.write().remove(server);
    STDERR_TAILS.write().remove(server);
}

/// Record that a server was declared/enabled but never produced a child.
pub fn record_never_started(server: &str, reason: &str) {
    let msg = format!("MCP server '{}' was never started: {}", server, reason);
    put(server, LivenessState::NeverStarted, msg, |e| {
        e.last_error = Some(reason.to_string());
        e.pid = None;
    });
}

/// Record that a child was spawned and the handshake is running.
pub fn record_starting(server: &str, pid: Option<u32>) {
    let msg = format!(
        "MCP server '{}' starting (child {})",
        server,
        match pid {
            Some(p) => format!("pid {p}"),
            None => "pid unknown".to_string(),
        }
    );
    put(server, LivenessState::Starting, msg, |e| {
        e.pid = pid;
        e.tool_count = 0;
    });
}

/// Record a healthy child + completed handshake.
pub fn record_running(server: &str, pid: Option<u32>, tool_count: usize) {
    let msg = format!(
        "MCP server '{}' running (child {}, {tool_count} tool(s))",
        server,
        match pid {
            Some(p) => format!("pid {p}"),
            None => "pid unknown".to_string(),
        }
    );
    put(server, LivenessState::Running, msg, |e| {
        e.pid = pid;
        e.tool_count = tool_count;
        e.restart_attempt = None;
        e.restart_delay_secs = None;
    });
}

/// Record a spawn/handshake failure (the child never became usable).
pub fn record_start_failed(server: &str, reason: &str) {
    let msg = format!("MCP server '{}' failed to start: {}", server, reason);
    put(server, LivenessState::Failed, msg, |e| {
        e.last_error = Some(reason.to_string());
    });
}

/// The exact ERROR log line for a death. Building it here (rather than in the
/// watchdog) keeps the log and the reported status byte-identical.
pub fn crash_message(
    server: &str,
    pid: Option<u32>,
    status: &str,
    stderr_tail: &[String],
) -> String {
    let mut msg = format!(
        "MCP server '{}' DIED: child {} exited with {} at {} - its liveness watchdog is restarting it",
        server,
        match pid {
            Some(p) => format!("pid {p}"),
            None => "pid unknown".to_string(),
        },
        status,
        fmt_ts(now_secs()),
    );
    if stderr_tail.is_empty() {
        msg.push_str("; no stderr output was captured (the child wrote nothing before exiting)");
    } else {
        msg.push_str(&format!(
            "; last {} stderr line(s) of the child:",
            stderr_tail.len()
        ));
        for line in stderr_tail {
            msg.push_str(&format!("\n  [mcp:{}] {}", server, line));
        }
    }
    msg
}

/// Record a crash and return the message to log.
pub fn record_crashed(
    server: &str,
    pid: Option<u32>,
    status: &str,
    stderr_tail: &[String],
) -> String {
    let msg = crash_message(server, pid, status, stderr_tail);
    let tail: Vec<String> = stderr_tail.to_vec();
    put(server, LivenessState::Crashed, msg.clone(), |e| {
        e.pid = pid;
        e.exit = Some(ExitRecord {
            at: now_secs(),
            pid,
            status: status.to_string(),
            stderr_tail: tail,
        });
        e.last_error = Some(format!("crashed: {status}"));
        e.tool_count = 0;
    });
    msg
}

/// Record that a supervised restart attempt is scheduled.
pub fn record_restarting(server: &str, attempt: u32, delay: Duration) {
    let msg = format!(
        "MCP server '{}' crashed - supervised restart attempt {}/{} in {}s",
        server,
        attempt,
        MAX_RESTART_ATTEMPTS,
        delay.as_secs()
    );
    put(server, LivenessState::Restarting, msg, |e| {
        e.restart_attempt = Some(attempt);
        e.restart_delay_secs = Some(delay.as_secs());
        e.pid = None;
    });
}

/// Record a successful supervised restart.
pub fn record_restarted(server: &str, pid: Option<u32>, tool_count: usize) {
    {
        let mut map = SERVERS.write();
        if let Some(e) = map.get_mut(server) {
            e.restarts = e.restarts.saturating_add(1);
        }
    }
    record_running(server, pid, tool_count);
}

/// Record that restarts are exhausted: the server stays down until a manual
/// restart. This is a LOUD terminal state, never a silent one.
pub fn record_give_up(server: &str, attempts: u32, last_error: &str) -> String {
    let msg = format!(
        "MCP server '{}' is DOWN: {attempts} supervised restart attempt(s) failed; giving up. \
         Last failure: {last_error}. Remediation: {}",
        server,
        remediation(server)
    );
    put(server, LivenessState::Failed, msg.clone(), |e| {
        e.restart_attempt = None;
        e.restart_delay_secs = None;
        e.last_error = Some(last_error.to_string());
    });
    msg
}

/// Record an intentional stop (shutdown / disable / replaced client).
pub fn record_stopped(server: &str, why: &str) {
    let msg = format!("MCP server '{}' stopped intentionally ({})", server, why);
    put(server, LivenessState::NeverStarted, msg, |e| {
        e.pid = None;
        e.tool_count = 0;
        e.restart_attempt = None;
        e.restart_delay_secs = None;
    });
}

/// Keep one stderr line of a child (bounded ring buffer) and mirror it to the
/// process log prefixed with the plugin name, so child stderr stays visible
/// even though it is now captured instead of inherited.
pub fn push_stderr_line(server: &str, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    {
        let mut tails = STDERR_TAILS.write();
        let buf = tails.entry(server.to_string()).or_default();
        if buf.len() >= STDERR_TAIL_LINES {
            buf.pop_front();
        }
        buf.push_back(trimmed.to_string());
    }
    tracing::info!("[mcp:{}] {}", server, trimmed);
}

/// Captured stderr tail of a server (oldest first).
pub fn stderr_tail(server: &str) -> Vec<String> {
    STDERR_TAILS
        .read()
        .get(server)
        .map(|b| b.iter().cloned().collect())
        .unwrap_or_default()
}

/// Snapshot of one server's liveness (None = no supervision record at all).
pub fn runtime_status(server: &str) -> Option<ServerLiveness> {
    SERVERS.read().get(server).cloned()
}

/// One-line truthful summary for a server; falls back to a clear statement
/// when there is no record (the plugin never reached the spawn stage).
pub fn summary_line(server: &str) -> String {
    match runtime_status(server) {
        Some(s) => s.message,
        None => format!(
            "MCP server '{}' has no liveness record: it was never spawned in this process",
            server
        ),
    }
}

/// Remediation hint for a plugin that is down.
pub fn remediation(server: &str) -> String {
    format!(
        "POST /api/plugins/tools/remote/{server}/restart (or enable the '{server}' plugin in the dashboard), then check the omniagent log for the child's stderr"
    )
}

/// Explain why a name is not resolvable, from the plugin's point of view.
///
/// `known_servers` are the MCP servers that hold a live client in this process;
/// `data_dir` is the omni dir (used for the plugin config lookup).
pub fn unknown_tool_diagnosis(tool: &str, data_dir: &str, known_servers: &[String]) -> String {
    let Some((plugin, _short)) = split_plugin(tool) else {
        return String::new();
    };
    if plugin.eq_ignore_ascii_case("core") || plugin.is_empty() {
        return String::new();
    }

    let entries =
        crate::plugins_yaml::load_raw(data_dir, &crate::plugins_yaml::PluginYamlType::Tool)
            .unwrap_or_default();
    let declared = entries.get(&plugin);
    let has_config = crate::mcp::external::config::server_config_exists(data_dir, &plugin);
    let live_client = known_servers.iter().any(|s| s == &plugin);

    let mut out = String::new();
    if let Some(entry) = declared {
        if !entry.enabled {
            out.push_str(&format!(
                "\nWhy: the tool plugin '{plugin}' exists but is DISABLED in tools.yml, so none of its tools are registered."
            ));
            out.push_str(&format!(
                "\nRemediation: enable the '{plugin}' plugin (dashboard > Plugins, or tools.yml) and retry."
            ));
            return out;
        }
    } else if !has_config {
        out.push_str(&format!(
            "\nWhy: no tool plugin named '{plugin}' is configured (no tools.yml entry and no MCP server config on disk), so '{tool}' cannot resolve."
        ));
        out.push_str(&format!(
            "\nRemediation: install/enable the '{plugin}' plugin, or use a core tool. Registered plugins: {}",
            list_servers(known_servers)
        ));
        return out;
    }

    // Declared + enabled (or at least configured): report the real liveness.
    let status = runtime_status(&plugin);
    match status {
        Some(s) if s.state == LivenessState::Running => {
            out.push_str(&format!(
                "\nWhy: the plugin '{plugin}' is running ({}) but does not expose '{tool}'. The tool name may have changed.",
                s.message
            ));
            out.push_str(
                "\nRemediation: GET /mcp/tools lists the registered tools of a running plugin.",
            );
        }
        Some(s) => {
            out.push_str(&format!(
                "\nWhy: the plugin '{plugin}' is enabled but its MCP server is NOT serving tools: {}.",
                s.message
            ));
            out.push_str(&format!(
                "\nRemediation: {}",
                if live_client {
                    remediation(&plugin)
                } else {
                    format!(
                        "{} (no live MCP client is registered for '{plugin}')",
                        remediation(&plugin)
                    )
                }
            ));
        }
        None => {
            out.push_str(&format!(
                "\nWhy: the plugin '{plugin}' is enabled but has no liveness record: its external MCP child was never spawned or died before registering, so '{tool}' is not in the registry."
            ));
            out.push_str(&format!("\nRemediation: {}", remediation(&plugin)));
        }
    }
    out
}

fn list_servers(servers: &[String]) -> String {
    if servers.is_empty() {
        return "(none)".to_string();
    }
    let mut names: Vec<&String> = servers.iter().collect();
    names.sort();
    names
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split `{plugin}__{tool}` into its plugin prefix and tool part.
fn split_plugin(tool: &str) -> Option<(String, String)> {
    match tool.split_once("__") {
        Some((p, t)) if !p.is_empty() => Some((p.to_string(), t.to_string())),
        _ => None,
    }
}

/// Install the hook the server layer uses to re-register a respawned server's
/// tools (a reload that ran while the child was dead must not permanently
/// remove the plugin's tools from the registry).
pub fn set_revive_hook(hook: ReviveHook) {
    *REVIVE_HOOK.write() = Some(hook);
}

/// Tell the server layer that `server` came back and its tools should be
/// present in the registry again. No-op when no hook is installed.
pub fn notify_tools_revived(server: &str) {
    let hook = REVIVE_HOOK.read().clone();
    if let Some(hook) = hook {
        hook(server.to_string());
    }
}

type RestartHook = Arc<dyn Fn(String) + Send + Sync>;

static RESTART_HOOK: Lazy<RwLock<Option<RestartHook>>> = Lazy::new(|| RwLock::new(None));

/// Install the hook the child watchdog calls when an MCP child dies.
///
/// The hook is deliberately a `dyn` boundary: the watchdog future must not
/// await the restart chain (watchdog -> restart -> respawn -> new watchdog),
/// because that recursive future cannot be proven `Send` structurally.
pub fn set_restart_hook(hook: RestartHook) {
    *RESTART_HOOK.write() = Some(hook);
}

/// Ask the installed hook to restart `server` after its child died. Without a
/// hook the server stays in its loud `crashed` state until a manual restart.
pub fn request_restart(server: &str) {
    let hook = RESTART_HOOK.read().clone();
    match hook {
        Some(hook) => hook(server.to_string()),
        None => tracing::error!(
            "MCP server '{}' crashed but no restart hook is installed - it stays down until a manual restart",
            server
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_exponential() {
        assert_eq!(backoff_for_attempt(1).as_secs(), 1);
        assert_eq!(backoff_for_attempt(2).as_secs(), 2);
        assert_eq!(backoff_for_attempt(3).as_secs(), 4);
        assert_eq!(backoff_for_attempt(4).as_secs(), 8);
        assert_eq!(backoff_for_attempt(5).as_secs(), 16);
        // Bounded: never grows past the cap.
        assert_eq!(backoff_for_attempt(9).as_secs(), 32);
    }

    #[test]
    fn crash_message_names_plugin_pid_exit_status_and_stderr_tail() {
        let tail = vec!["boom: out of memory".to_string()];
        let msg = crash_message("external", Some(4242), "exit code 137", &tail);
        assert!(msg.contains("MCP server 'external' DIED"), "got: {msg}");
        assert!(msg.contains("pid 4242"), "got: {msg}");
        assert!(msg.contains("exit code 137"), "got: {msg}");
        assert!(msg.contains("boom: out of memory"), "got: {msg}");
    }

    #[test]
    fn crash_message_says_so_when_no_stderr_was_captured() {
        let msg = crash_message("external", None, "signal: 9 (SIGKILL)", &[]);
        assert!(msg.contains("signal: 9 (SIGKILL)"), "got: {msg}");
        assert!(msg.contains("no stderr output was captured"), "got: {msg}");
    }

    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_last_lines() {
        let server = "tail-test-server";
        clear(server);
        for i in 0..(STDERR_TAIL_LINES + 5) {
            push_stderr_line(server, &format!("line-{i}"));
        }
        let tail = stderr_tail(server);
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(
            tail.last().map(String::as_str),
            Some(format!("line-{}", STDERR_TAIL_LINES + 4).as_str())
        );
        clear(server);
        assert!(stderr_tail(server).is_empty());
    }

    #[test]
    fn lifecycle_states_are_recorded_truthfully() {
        let server = "lifecycle-test-server";
        clear(server);
        assert!(runtime_status(server).is_none());

        record_starting(server, Some(7));
        assert_eq!(
            runtime_status(server).unwrap().state,
            LivenessState::Starting
        );

        record_running(server, Some(7), 3);
        let s = runtime_status(server).unwrap();
        assert_eq!(s.state, LivenessState::Running);
        assert_eq!(s.tool_count, 3);
        assert!(s.message.contains("running"));

        let msg = record_crashed(server, Some(7), "exit code 1", &["dead".to_string()]);
        let s = runtime_status(server).unwrap();
        assert_eq!(s.state, LivenessState::Crashed);
        let exit = s.exit.clone().expect("exit record");
        assert_eq!(exit.status, "exit code 1");
        assert_eq!(exit.pid, Some(7));
        assert_eq!(exit.stderr_tail, vec!["dead".to_string()]);
        assert!(msg.contains("exit code 1"));

        record_restarting(server, 2, Duration::from_secs(2));
        let s = runtime_status(server).unwrap();
        assert_eq!(s.state, LivenessState::Restarting);
        assert_eq!(s.restart_attempt, Some(2));

        record_restarted(server, Some(9), 1);
        let s = runtime_status(server).unwrap();
        assert_eq!(s.state, LivenessState::Running);
        assert_eq!(s.restarts, 1);

        let msg = record_give_up(server, MAX_RESTART_ATTEMPTS, "spawn failed");
        let s = runtime_status(server).unwrap();
        assert_eq!(s.state, LivenessState::Failed);
        assert!(msg.contains("DOWN"));
        assert!(msg.contains("spawn failed"));
        clear(server);
    }

    #[test]
    fn diagnosis_explains_a_never_started_enabled_plugin() {
        // Empty data dir: no plugin config, so the diagnosis says the plugin is
        // not configured - which is itself actionable and never a bare name.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = unknown_tool_diagnosis(
            "phantom__do_thing",
            &dir.path().to_string_lossy(),
            &["filesystem".to_string(), "search".to_string()],
        );
        assert!(out.contains("phantom"), "got: {out}");
        assert!(out.contains("Remediation"), "got: {out}");
        assert!(out.contains("filesystem"), "got: {out}");
        // A core tool name never gets a plugin diagnosis.
        assert!(
            unknown_tool_diagnosis("core__nope", &dir.path().to_string_lossy(), &[]).is_empty()
        );
        // A name without the plugin grammar gets nothing.
        assert!(
            unknown_tool_diagnosis("plain_name", &dir.path().to_string_lossy(), &[]).is_empty()
        );
    }
}
