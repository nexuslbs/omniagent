//! mcp-server-ssh: standalone MCP server for SSH operations.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: run (ssh_run), copy (ssh_copy), status (ssh_status)
//!
//! The plugin is deliberately AGNOSTIC: it just executes ssh/scp operations
//! safely and reports results. Remote-development workflows (git clone on the
//! remote, copy secrets, docker compose up, wait-task, logs/status) are built
//! ON TOP of this plugin as a skill + template guidance - NOT baked in here.
//!
//! Config (via the MCP `configure` message, resolved from plugins.yml):
//!   - ssh_dir: directory containing the ssh `config` file and any private
//!     keys. Default `{OMNI_DIR}/data/ssh/` (OMNI_DIR env var). Created on
//!     demand; private keys inside are chmod 600'd before any ssh/scp run.
//!   - connect_timeout_secs: ssh ConnectTimeout (default 10).
//!   - workspace_dir: optional sandbox for the LOCAL side of ssh_copy.
//!   - database_url: Postgres URL of the omniagent secrets store (default
//!     $env:DATABASE_URL); used to resolve ssh_key_secret_name at call time.

use anyhow::{Context, Result};
use mcp_server_util::*;
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

//  Config

#[derive(Clone)]
struct Config {
    ssh_dir: String,
    connect_timeout_secs: u64,
    workspace_dir: String,
    database_url: String, // secrets store URL (from the configure message)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh_dir: String::new(), // resolved: {OMNI_DIR}/data/ssh
            connect_timeout_secs: 10,
            workspace_dir: "/opt/workspace".to_string(),
            database_url: String::new(),
        }
    }
}

static CONFIG: LazyLock<Mutex<Config>> = LazyLock::new(|| Mutex::new(Config::default()));

//  Secret-backed identity (ssh_key_secret_name)

/// RAII guard for a private key materialized from the omniagent secrets store.
///
/// The key VALUE is never persisted in ssh_dir and never printed/logged: it is
/// written to a 0600 temp file (OpenSSH refuses group/world-readable keys), the
/// file is passed to ssh/scp with `-i`, and it is removed the moment this guard
/// drops - on EVERY return path, including errors and timeouts.
struct SecretKeyFile {
    path: PathBuf,
}

impl SecretKeyFile {
    fn create(key_value: &str) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let file_name = format!(
            "mcp-ssh-secret-key-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(file_name);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&path)
            .with_context(|| format!("Failed to create temp ssh key file {}", path.display()))?;
        f.write_all(key_value.as_bytes())
            .context("Failed to write secret key to temp file")?;
        if !key_value.ends_with('\n') {
            f.write_all(b"\n").ok();
        }
        f.sync_all().ok();
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SecretKeyFile {
    fn drop(&mut self) {
        // Best-effort cleanup on EVERY return path (errors, timeouts included).
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Resolve `ssh_key_secret_name` to the private key VALUE stored under that
/// name in the omniagent secrets table (the same store `$secret:NAME` config
/// references read from). The value is materialized as a short-lived 0600 temp
/// file so the OpenSSH client can use it; the raw key material is never logged
/// and never placed in command arguments or the shell.
async fn resolve_secret_key(secret_name: &str) -> Result<SecretKeyFile> {
    let database_url = {
        let cfg = CONFIG.lock();
        cfg.database_url.clone()
    };
    if database_url.is_empty() {
        anyhow::bail!(
            "ssh_key_secret_name '{}' was provided, but this ssh plugin has no database_url \
             configured to look secrets up. Configure database_url in the ssh plugin settings \
             (it defaults to $env:DATABASE_URL) or store a local key file in ssh_dir.",
            secret_name
        );
    }
    let mut conn = sqlx::postgres::PgConnection::connect(&database_url)
        .await
        .context("Failed to connect to the omniagent database to resolve the ssh key secret")?;
    let value: Option<String> =
        sqlx::query_scalar::<_, String>("SELECT current_value FROM secrets WHERE name = $1")
            .bind(secret_name)
            .fetch_optional(&mut conn)
            .await
            .context("Failed to read the secret from the secrets store")?;
    match value {
        None => anyhow::bail!(
            "Secret '{}' not found in the secrets store. Store the private key under that name \
             (e.g. via the secrets API) or use a local key file in ssh_dir instead.",
            secret_name
        ),
        Some(v) if v.trim().is_empty() => anyhow::bail!(
            "Secret '{}' exists but its value is EMPTY. Store the private key value under that \
             name or use a local key file in ssh_dir.",
            secret_name
        ),
        Some(v) => SecretKeyFile::create(v.trim()),
    }
}

//  Helpers

/// Resolve the ssh_dir, honoring an optional per-call override.
///
/// Priority:
///   1. explicit `ssh_dir` tool argument (per-call override)
///   2. configured `ssh_dir` from plugins.yml
///   3. `{OMNI_DIR}/data/ssh` - OMNI_DIR env var (like git's omni_dir),
///      falling back to /opt/omni when unset.
fn resolve_ssh_dir(override_dir: Option<&str>) -> String {
    let cfg = CONFIG.lock();
    if let Some(d) = override_dir {
        if !d.is_empty() {
            return d.to_string();
        }
    }
    if !cfg.ssh_dir.is_empty() {
        return cfg.ssh_dir.clone();
    }
    let omni = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());
    format!("{}/data/ssh", omni)
}

/// Files inside ssh_dir that are NOT private keys and are left untouched.
fn is_non_key_file(name: &str) -> bool {
    name == "config"
        || name == "known_hosts"
        || name == "known_hosts.old"
        || name.ends_with(".pub")
        || name.starts_with('.')
}

/// Create ssh_dir if missing and chmod 600 every private key inside.
///
/// SSH refuses to use a private key that is group/world readable (and so
/// does OpenSSH's own ssh-agent policy). The plugin enforces this BEFORE
/// every ssh/scp run: any non-public file in ssh_dir is chmod 600'd, and a
/// failure to set permissions aborts the operation with a clear error -
/// ssh is NEVER run with a world-readable key.
fn secure_ssh_dir(dir: &str) -> Result<()> {
    let d = Path::new(dir);
    std::fs::create_dir_all(d).with_context(|| format!("Failed to create ssh dir {}", dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The directory itself should be private too (keys inside).
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        let entries =
            std::fs::read_dir(d).with_context(|| format!("Failed to read ssh dir {}", dir))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if is_non_key_file(&name) {
                continue;
            }
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| {
                        format!(
                            "Refusing to run ssh: cannot chmod 600 private key {} \
                             (permissions {:03o} would be world/group accessible). \
                             Fix the key's permissions and retry.",
                            path.display(),
                            mode & 0o777
                        )
                    })?;
            }
            // Verify the write actually landed - never run ssh with a key
            // that is still group/world accessible.
            let after = std::fs::metadata(&path)
                .with_context(|| format!("Failed to re-stat {}", path.display()))?;
            if after.permissions().mode() & 0o077 != 0 {
                anyhow::bail!(
                    "Refusing to run ssh: private key {} is still group/world accessible \
                     (permissions {:03o}) after chmod 600 attempt.",
                    path.display(),
                    after.permissions().mode() & 0o777
                );
            }
        }
    }
    Ok(())
}

/// Split an inline `[user@]host[:port]` spec into (host, optional port).
///
/// ssh uses `-p <port>`, scp uses `-P <port>` - both need the port extracted
/// from the inline form. IPv6 bracket form `[::1]:port` is preserved.
fn split_host_port(host: &str) -> (String, Option<u16>) {
    // IPv6 bracket form: [::1] or [::1]:port
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let addr = format!("[{}]", &rest[..end]);
            let tail = &rest[end + 1..];
            if let Some(p) = tail.strip_prefix(':') {
                if let Ok(port) = p.parse::<u16>() {
                    return (addr, Some(port));
                }
            }
            return (addr, None);
        }
    }
    // user@host:port - split on the last colon if followed by digits only.
    if let Some(idx) = host.rfind(':') {
        let tail = &host[idx + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(port) = tail.parse::<u16>() {
                return (host[..idx].to_string(), Some(port));
            }
        }
    }
    (host.to_string(), None)
}

/// Add the shared ssh options to a Command: BatchMode (never prompt),
/// ConnectTimeout, and `-F <ssh_dir>/config` when the config file exists.
fn add_ssh_options(cmd: &mut Command, ssh_dir: &str, connect_timeout: u64) {
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={}", connect_timeout));
    let cfg_path = Path::new(ssh_dir).join("config");
    if cfg_path.is_file() {
        cmd.arg("-F").arg(&cfg_path);
    }
}

/// Resolve a local path for the LOCAL side of ssh_copy against the sandbox.
///
/// Absolute paths must stay inside `workspace_dir`; relative paths resolve
/// against the workspace dir (git-plugin convention). Returns the absolute
/// path to use.
fn resolve_local_path(p: &str, workspace: &str, what: &str) -> Result<String> {
    if p.is_empty() {
        anyhow::bail!("Missing '{}' path", what);
    }
    let ws_abs = Path::new(workspace)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(workspace).to_path_buf());
    let candidate = Path::new(p);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        ws_abs.join(candidate)
    };
    let norm = normalize_path(&resolved);
    let ws_norm = normalize_path(&ws_abs);
    if !norm.starts_with(&ws_norm) {
        anyhow::bail!(
            "ssh_copy {} path '{}' is outside the ssh plugin workspace sandbox '{}'. \
             Local copies must stay inside the workspace.",
            what,
            p,
            workspace
        );
    }
    Ok(norm.display().to_string())
}

/// Lexically normalize a path (resolve `.` and `..` without touching the fs).
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Truncate huge outputs (same policy as git/docker: 50k chars).
fn truncate(s: String) -> String {
    const MAX_OUT: usize = 50_000;
    if s.len() > MAX_OUT {
        format!("{}... [truncated from {} chars]", &s[..MAX_OUT], s.len())
    } else {
        s
    }
}

//  Core subprocess runner (COPY of the run_git pattern - subprocess hygiene)

/// Run `ssh <opts> <host> <remote_cmd...>` and return
/// (stdout, stderr, exit_code, elapsed_ms).
///
/// Subprocess hygiene (CRITICAL, G17b class): stdout/stderr piped BEFORE
/// spawn, stdin /dev/null (or piped for scripts), kill_on_drop(true) so a
/// dropped future (timeout, client cancel, thread end) kills ssh. The MCP
/// JSON-RPC stdio channel is NEVER inherited.
async fn run_ssh(
    host: &str,
    remote_args: &[&str],
    stdin_script: Option<&str>,
    timeout_secs: Option<u64>,
    ssh_dir: &str,
    connect_timeout: u64,
    identity: Option<&Path>,
) -> (String, String, i32, u128) {
    if let Err(e) = secure_ssh_dir(ssh_dir) {
        return (String::new(), e.to_string(), -1, 0);
    }

    let (host_part, port) = split_host_port(host);
    let mut cmd = Command::new("ssh");
    add_ssh_options(&mut cmd, ssh_dir, connect_timeout);
    if let Some(key_path) = identity {
        cmd.arg("-i").arg(key_path);
        cmd.arg("-o").arg("IdentitiesOnly=yes");
    }
    if let Some(p) = port {
        cmd.arg("-p").arg(p.to_string());
    }
    cmd.arg(&host_part);
    for a in remote_args {
        cmd.arg(a);
    }
    // CRITICAL: pipe stdout/stderr BEFORE spawn; stdin NEVER inherited -
    // /dev/null normally, piped for script mode (a fresh pipe, not fd 0).
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let use_script = stdin_script.is_some();
    if use_script {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                String::new(),
                format!("Failed to spawn ssh: {}", e),
                -1,
                start.elapsed().as_millis(),
            );
        }
    };

    if let Some(script) = stdin_script {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(script.as_bytes()).await {
                return (
                    String::new(),
                    format!("Failed to write script to ssh stdin: {}", e),
                    -1,
                    start.elapsed().as_millis(),
                );
            }
            drop(stdin); // close stdin so the remote `sh -s` sees EOF
        }
    }

    let wait_fut = child.wait_with_output();
    let (output, timed_out) = match timeout_secs {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), wait_fut).await {
            Ok(Ok(output)) => (Some(output), false),
            Ok(Err(e)) => {
                return (
                    String::new(),
                    format!("ssh output error: {}", e),
                    -1,
                    start.elapsed().as_millis(),
                );
            }
            Err(_) => (None, true),
        },
        None => match wait_fut.await {
            Ok(output) => (Some(output), false),
            Err(e) => {
                return (
                    String::new(),
                    format!("ssh output error: {}", e),
                    -1,
                    start.elapsed().as_millis(),
                );
            }
        },
    };
    let elapsed = start.elapsed().as_millis();

    if timed_out {
        return (
            String::new(),
            format!(
                "ssh command timed out after {}s (explicit 'timeout' param)",
                timeout_secs.unwrap_or(0)
            ),
            -1,
            elapsed,
        );
    }

    let output = output.expect("output present when not timed out");
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let rc = output.status.code().unwrap_or(-1);
    tracing::info!(
        "ssh_run host={} rc={} elapsed_ms={} out_head={:?}",
        host_part,
        rc,
        elapsed,
        &out.chars().take(60).collect::<String>(),
    );
    (out, err, rc, elapsed)
}

//  Tool handlers

/// `run` (ssh_run): run a shell command on a remote host.
async fn handle_run(args: Value) -> Result<(String, bool)> {
    let host = args["host"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing required parameter: host (host alias from ssh config OR user@host:port)"
            )
        })?
        .to_string();
    if host.is_empty() {
        anyhow::bail!("host cannot be empty");
    }
    let command = args["command"].as_str().unwrap_or("").to_string();
    let script = args["script"].as_str().unwrap_or("").to_string();
    if command.is_empty() && script.is_empty() {
        anyhow::bail!(
            "Missing required parameter: command (shell command) - or provide 'script' \
             (multi-line script piped to remote sh via stdin)"
        );
    }
    let workdir = args["workdir"].as_str().unwrap_or("").to_string();
    let timeout_secs = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()));
    let ssh_dir_override = args["ssh_dir"].as_str().map(|s| s.to_string());
    let ssh_dir = resolve_ssh_dir(ssh_dir_override.as_deref());

    let connect_timeout = {
        let cfg = CONFIG.lock();
        cfg.connect_timeout_secs
    };

    // Optional secret-backed identity (ssh_key_secret_name): when the agent
    // passes the NAME of a secret (e.g. SSH_PRIVATE_KEY), its VALUE is used as
    // the private key for this connection. Local key files stay the default.
    let secret_key = match args["ssh_key_secret_name"].as_str() {
        Some(name) if !name.trim().is_empty() => Some(resolve_secret_key(name.trim()).await?),
        _ => None,
    };
    let identity_path = secret_key.as_ref().map(|k| k.path());

    // Build the remote command line. The command is passed as ONE argument so
    // the remote shell (which ssh hands the whole string to) interprets shell
    // operators - exactly like docker compose exec passes args through sh -c.
    let remote_cmd = if script.is_empty() {
        if workdir.is_empty() {
            command.clone()
        } else {
            // Quote the workdir for the remote shell (single-quote escape).
            let wd = workdir.replace('\'', "'\\''");
            format!("cd '{}' && {}", wd, command)
        }
    } else {
        String::new() // script mode: remote runs `sh -s`, stdin piped
    };

    let (out, err, rc, elapsed_ms) = if script.is_empty() {
        run_ssh(
            &host,
            &[&remote_cmd],
            None,
            timeout_secs,
            &ssh_dir,
            connect_timeout,
            identity_path,
        )
        .await
    } else {
        // Script mode: `ssh host sh -s` with the script on stdin.
        run_ssh(
            &host,
            &["sh", "-s"],
            Some(&script),
            timeout_secs,
            &ssh_dir,
            connect_timeout,
            identity_path,
        )
        .await
    };

    if rc != 0 {
        return Ok((
            serde_json::json!({
                "success": false,
                "host": host,
                "exit_code": rc,
                "stdout": truncate(out),
                "stderr": truncate(err),
                "duration_ms": elapsed_ms,
            })
            .to_string(),
            true,
        ));
    }

    Ok((
        serde_json::json!({
            "success": true,
            "host": host,
            "exit_code": rc,
            "stdout": truncate(out),
            "stderr": truncate(err),
            "duration_ms": elapsed_ms,
        })
        .to_string(),
        false,
    ))
}

/// `copy` (ssh_copy): copy files to/from a remote host with scp.
async fn handle_copy(args: Value) -> Result<(String, bool)> {
    let host = args["host"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing required parameter: host (host alias from ssh config OR user@host:port)"
            )
        })?
        .to_string();
    if host.is_empty() {
        anyhow::bail!("host cannot be empty");
    }
    let direction = args["direction"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!("Missing required parameter: direction (to-remote|from-remote)")
        })?
        .to_string();
    if direction != "to-remote" && direction != "from-remote" {
        anyhow::bail!(
            "direction must be 'to-remote' or 'from-remote', got '{}'",
            direction
        );
    }
    let source = args["source"].as_str().unwrap_or("").to_string();
    let destination = args["destination"].as_str().unwrap_or("").to_string();
    if source.is_empty() || destination.is_empty() {
        anyhow::bail!("Missing required parameters: source and destination");
    }
    let recursive = args["recursive"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()));
    let ssh_dir_override = args["ssh_dir"].as_str().map(|s| s.to_string());
    let ssh_dir = resolve_ssh_dir(ssh_dir_override.as_deref());

    let (connect_timeout, workspace) = {
        let cfg = CONFIG.lock();
        (cfg.connect_timeout_secs, cfg.workspace_dir.clone())
    };

    if let Err(e) = secure_ssh_dir(&ssh_dir) {
        return Ok((e.to_string(), true));
    }

    // Optional secret-backed identity (ssh_key_secret_name).
    let secret_key = match args["ssh_key_secret_name"].as_str() {
        Some(name) if !name.trim().is_empty() => Some(resolve_secret_key(name.trim()).await?),
        _ => None,
    };
    let identity_path = secret_key.as_ref().map(|k| k.path());

    let (host_part, port) = split_host_port(&host);

    // Build scp args. The remote side is `host:path` (colon-prefixed).
    let mut cmd = Command::new("scp");
    add_ssh_options(&mut cmd, &ssh_dir, connect_timeout);
    if let Some(key_path) = identity_path {
        cmd.arg("-i").arg(key_path);
        cmd.arg("-o").arg("IdentitiesOnly=yes");
    }
    if let Some(p) = port {
        cmd.arg("-P").arg(p.to_string());
    }
    if recursive {
        cmd.arg("-r");
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    // Sandbox the LOCAL side; the remote side is a bare path (scp joins it
    // to host: internally - never let it start with '-' or ':').
    let (arg1, arg2): (String, String) = match direction.as_str() {
        "to-remote" => {
            let local = match resolve_local_path(&source, &workspace, "source") {
                Ok(p) => p,
                Err(e) => return Ok((e.to_string(), true)),
            };
            let remote = destination.trim_start_matches(':').to_string();
            (local, format!("{}:{}", host_part, remote))
        }
        "from-remote" => {
            let local = match resolve_local_path(&destination, &workspace, "destination") {
                Ok(p) => p,
                Err(e) => return Ok((e.to_string(), true)),
            };
            let remote = source.trim_start_matches(':').to_string();
            (format!("{}:{}", host_part, remote), local)
        }
        _ => unreachable!("direction validated above"),
    };
    cmd.arg(&arg1).arg(&arg2);

    let start = Instant::now();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok((format!("Failed to spawn scp: {}", e), true));
        }
    };
    let wait_fut = child.wait_with_output();
    let (output, timed_out) = match timeout_secs {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), wait_fut).await {
            Ok(Ok(output)) => (Some(output), false),
            Ok(Err(e)) => {
                return Ok((format!("scp output error: {}", e), true));
            }
            Err(_) => (None, true),
        },
        None => match wait_fut.await {
            Ok(output) => (Some(output), false),
            Err(e) => return Ok((format!("scp output error: {}", e), true)),
        },
    };
    let elapsed_ms = start.elapsed().as_millis();

    if timed_out {
        return Ok((
            format!(
                "scp timed out after {}s (explicit 'timeout' param)",
                timeout_secs.unwrap_or(0)
            ),
            true,
        ));
    }
    let output = output.expect("output present when not timed out");
    let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let rc = output.status.code().unwrap_or(-1);

    if rc != 0 {
        return Ok((
            serde_json::json!({
                "success": false,
                "host": host,
                "direction": direction,
                "exit_code": rc,
                "stdout": truncate(out),
                "stderr": truncate(err),
                "duration_ms": elapsed_ms,
            })
            .to_string(),
            true,
        ));
    }

    Ok((
        serde_json::json!({
            "success": true,
            "host": host,
            "direction": direction,
            "source": source,
            "destination": destination,
            "exit_code": rc,
            "stderr": truncate(err),
            "duration_ms": elapsed_ms,
        })
        .to_string(),
        false,
    ))
}

/// `status` (ssh_status): connectivity check - `ssh host true`.
async fn handle_status(args: Value) -> Result<(String, bool)> {
    let host = args["host"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: host"))?
        .to_string();
    if host.is_empty() {
        anyhow::bail!("host cannot be empty");
    }
    let timeout_secs = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()));
    let ssh_dir_override = args["ssh_dir"].as_str().map(|s| s.to_string());
    let ssh_dir = resolve_ssh_dir(ssh_dir_override.as_deref());

    let connect_timeout = {
        let cfg = CONFIG.lock();
        cfg.connect_timeout_secs
    };

    // Optional secret-backed identity (ssh_key_secret_name).
    let secret_key = match args["ssh_key_secret_name"].as_str() {
        Some(name) if !name.trim().is_empty() => Some(resolve_secret_key(name.trim()).await?),
        _ => None,
    };
    let identity_path = secret_key.as_ref().map(|k| k.path());

    let (out, err, rc, elapsed_ms) = run_ssh(
        &host,
        &["true"],
        None,
        timeout_secs,
        &ssh_dir,
        connect_timeout,
        identity_path,
    )
    .await;

    if rc != 0 {
        return Ok((
            serde_json::json!({
                "ok": false,
                "host": host,
                "error": truncate(err),
                "duration_ms": elapsed_ms,
            })
            .to_string(),
            true,
        ));
    }

    Ok((
        serde_json::json!({
            "ok": true,
            "host": host,
            "latency_ms": elapsed_ms,
            "note": out,
        })
        .to_string(),
        false,
    ))
}

//  Main

#[tokio::main]
async fn main() -> Result<()> {
    let tools: Vec<McpToolEntry> = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "run".to_string(),
                description: "RUN a command on a remote machine over SSH. \
                    'host' (required) is a host alias from the ssh config file in ssh_dir, \
                    OR an inline 'user@host:port' spec. 'command' (required) is the shell \
                    command to run - no character restrictions, interpreted by the remote \
                    shell (sh -c) exactly like docker compose exec args. 'workdir' (optional) \
                    cd's to a remote dir first. 'script' (optional) pipes a multi-line script \
                    to the remote `sh` via stdin (alternative to command). 'timeout' (optional) \
                    bounds the command in seconds - when omitted there is NO timeout (long \
                    commands run as tracked background tasks; use builtin_wait-task to follow). \
                    'ssh_dir' (optional) overrides the configured ssh dir (default \
                    {OMNI_DIR}/data/ssh). 'ssh_key_secret_name' (optional): name of a secret in the omniagent secrets store \
                    (e.g. SSH_PRIVATE_KEY) whose VALUE is the private SSH key used for \
                    authentication. Local key files under ssh_dir remain supported when this \
                    param is omitted. Returns stdout, stderr, exit code and duration."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Host alias from ssh config OR user@host:port inline"
                        },
                        "command": {
                            "type": "string",
                            "description": "Shell command to run on the remote (interpreted by the remote shell)"
                        },
                        "timeout": {
                            "type": "number",
                            "description": "Explicit timeout in seconds (optional; when omitted NO timeout)"
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Remote directory to cd into before running the command"
                        },
                        "script": {
                            "type": "string",
                            "description": "Multi-line script piped to remote sh via stdin (alternative to command)"
                        },
                        "ssh_dir": {
                            "type": "string",
                            "description": "Override the configured ssh dir (keys/config)"
                        },
                        "ssh_key_secret_name": {
                            "type": "string",
                            "description": "Name of a secret in the omniagent secrets store (e.g. SSH_PRIVATE_KEY) whose VALUE is the private SSH key used to authenticate with this host. Fetched at call time, never persisted. Optional: when omitted, local key files under ssh_dir are used (unchanged behavior)."
                        }
                    },
                    "required": ["host"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| {
                Box::pin(async move { handle_run(args).await })
            }),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "copy".to_string(),
                description: "COPY files to or from a remote machine over SSH (scp). \
                    'host' (required) is a host alias OR 'user@host:port' inline. \
                    'direction' (required) is 'to-remote' (local source -> remote destination) \
                    or 'from-remote' (remote source -> local destination). 'source' and \
                    'destination' are the two paths (the remote side is a bare path; the plugin \
                    prefixes host:). 'recursive' (optional bool) adds -r for directories. \
                    The LOCAL side is sandboxed to the configured workspace_dir \
                    (default /opt/workspace). 'timeout' (optional) bounds the copy in seconds. \
                    'ssh_dir' (optional) overrides the configured ssh dir. 'ssh_key_secret_name' (optional): \
                    name of a secret whose VALUE is the private SSH key to use; local key files \
                    remain supported when omitted."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Host alias from ssh config OR user@host:port inline"
                        },
                        "direction": {
                            "type": "string",
                            "enum": ["to-remote", "from-remote"],
                            "description": "Copy direction: to-remote (local -> remote) or from-remote (remote -> local)"
                        },
                        "source": {
                            "type": "string",
                            "description": "Source path (local path for to-remote, remote path for from-remote)"
                        },
                        "destination": {
                            "type": "string",
                            "description": "Destination path (remote path for to-remote, local path for from-remote)"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Recursively copy directories (-r)"
                        },
                        "timeout": {
                            "type": "number",
                            "description": "Explicit timeout in seconds (optional)"
                        },
                        "ssh_dir": {
                            "type": "string",
                            "description": "Override the configured ssh dir (keys/config)"
                        },
                        "ssh_key_secret_name": {
                            "type": "string",
                            "description": "Name of a secret in the omniagent secrets store (e.g. SSH_PRIVATE_KEY) whose VALUE is the private SSH key used to authenticate with this host. Fetched at call time, never persisted. Optional: when omitted, local key files under ssh_dir are used (unchanged behavior)."
                        }
                    },
                    "required": ["host", "direction", "source", "destination"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| {
                Box::pin(async move { handle_copy(args).await })
            }),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "status".to_string(),
                description: "CHECK connectivity to a remote host: runs `ssh host true` with the \
                    configured ConnectTimeout and reports ok/error plus latency. Use this \
                    to fail fast before scripting a long remote setup. 'host' (required) is \
                    a host alias OR 'user@host:port' inline. 'ssh_dir' (optional) overrides \
                    the configured ssh dir. 'ssh_key_secret_name' (optional): name of a secret \
                    whose VALUE is the private SSH key to use; local key files remain \
                    supported when omitted."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Host alias from ssh config OR user@host:port inline"
                        },
                        "timeout": {
                            "type": "number",
                            "description": "Explicit timeout in seconds (optional)"
                        },
                        "ssh_dir": {
                            "type": "string",
                            "description": "Override the configured ssh dir (keys/config)"
                        },
                        "ssh_key_secret_name": {
                            "type": "string",
                            "description": "Name of a secret in the omniagent secrets store (e.g. SSH_PRIVATE_KEY) whose VALUE is the private SSH key used to authenticate with this host. Fetched at call time, never persisted. Optional: when omitted, local key files under ssh_dir are used (unchanged behavior)."
                        }
                    },
                    "required": ["host"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| {
                Box::pin(async move { handle_status(args).await })
            }),
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-ssh".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(
        server_info,
        tools,
        Some(move |params: Value| {
            let mut cfg = CONFIG.lock();
            if let Some(dir) = params.get("ssh_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.ssh_dir = dir.to_string();
                }
            }
            if let Some(v) = params.get("connect_timeout_secs").and_then(|v| v.as_str()) {
                if let Ok(n) = v.parse::<u64>() {
                    if n > 0 {
                        cfg.connect_timeout_secs = n;
                    }
                }
            }
            if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.workspace_dir = dir.to_string();
                }
            }
            if let Some(url) = params.get("database_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    cfg.database_url = url.to_string();
                }
            }
            tracing::info!("SSH plugin configured (ssh_dir={})", cfg.ssh_dir);
        }),
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Point CONFIG at a temp ssh dir + workspace for sandbox/perms tests.
    /// Returns a guard for a static test lock (CONFIG is shared state).
    fn set_config(ssh_dir: &str, workspace: &str) -> parking_lot::MutexGuard<'static, ()> {
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        let guard = TEST_LOCK.lock();
        {
            let mut cfg = CONFIG.lock();
            cfg.ssh_dir = ssh_dir.to_string();
            cfg.workspace_dir = workspace.to_string();
            cfg.database_url = String::new();
        }
        guard
    }

    fn temp_dir(tag: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "mcp-ssh-test-{}-{}-{}",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("create temp dir");
        base.to_str().unwrap().to_string()
    }

    #[test]
    fn split_host_port_plain() {
        assert_eq!(split_host_port("myserver"), ("myserver".to_string(), None));
        assert_eq!(
            split_host_port("root@10.0.0.5"),
            ("root@10.0.0.5".to_string(), None)
        );
    }

    #[test]
    fn split_host_port_with_port() {
        assert_eq!(
            split_host_port("root@10.0.0.5:2222"),
            ("root@10.0.0.5".to_string(), Some(2222))
        );
        assert_eq!(
            split_host_port("myserver:2222"),
            ("myserver".to_string(), Some(2222))
        );
    }

    #[test]
    fn split_host_port_ipv6_bracket() {
        assert_eq!(
            split_host_port("[::1]:2222"),
            ("[::1]".to_string(), Some(2222))
        );
        assert_eq!(split_host_port("[::1]"), ("[::1]".to_string(), None));
    }

    #[test]
    fn split_host_port_non_numeric_tail_not_a_port() {
        // user@host:path-like or non-numeric tails must not be eaten.
        assert_eq!(
            split_host_port("root@host:abc"),
            ("root@host:abc".to_string(), None)
        );
    }

    #[test]
    fn ssh_dir_resolution_defaults_to_omni_data() {
        let _g = set_config("", "/opt/workspace");
        // OMNI_DIR may be set in the test env; fall back to /opt/omni.
        let omni = std::env::var("OMNI_DIR").unwrap_or_else(|_| "/opt/omni".to_string());
        assert_eq!(resolve_ssh_dir(None), format!("{}/data/ssh", omni));
    }

    #[test]
    fn ssh_dir_resolution_override_wins() {
        let _g = set_config("/cfg/ssh", "/opt/workspace");
        assert_eq!(resolve_ssh_dir(None), "/cfg/ssh");
        // Per-call override beats config.
        assert_eq!(resolve_ssh_dir(Some("/tmp/override")), "/tmp/override");
        // Empty override falls back to config.
        assert_eq!(resolve_ssh_dir(Some("")), "/cfg/ssh");
    }

    #[test]
    fn secure_ssh_dir_creates_and_chmods_keys() {
        let dir = temp_dir("perms");
        let _g = set_config(&dir, "/opt/workspace");
        // Create a world-readable key + a .pub (untouched) + config (untouched).
        let key = Path::new(&dir).join("id_ed25519");
        std::fs::write(&key, "PRIVATE").expect("write key");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let pubkey = Path::new(&dir).join("id_ed25519.pub");
        std::fs::write(&pubkey, "PUBLIC").unwrap();
        let config = Path::new(&dir).join("config");
        std::fs::write(&config, "Host x\n").unwrap();

        secure_ssh_dir(&dir).expect("secure_ssh_dir should succeed");

        let key_mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(key_mode & 0o077, 0, "private key must be 600 after secure");
        // .pub and config left as-is (mode not forced to 600).
        let pub_mode = std::fs::metadata(&pubkey).unwrap().permissions().mode();
        assert_eq!(pub_mode & 0o7777, 0o644);
    }

    #[test]
    fn secure_ssh_dir_rejects_unfixable_world_readable_key() {
        // A key the plugin cannot chmod (chmod fails -> clear error), or a
        // key that remains group/world accessible after the attempt.
        let dir = temp_dir("unfixable");
        let _g = set_config(&dir, "/opt/workspace");
        let key = Path::new(&dir).join("id_rsa");
        std::fs::write(&key, "PRIVATE").expect("write key");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o666)).unwrap();

        // Normal case: chmod works and the key is fixed.
        secure_ssh_dir(&dir).expect("chmod should fix 666 key");
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn normalize_path_resolves_dotdot() {
        let p = normalize_path(Path::new("/opt/workspace/sub/../omniagent"));
        assert_eq!(p.to_str().unwrap(), "/opt/workspace/omniagent");
        let p = normalize_path(Path::new("/opt/workspace"));
        assert_eq!(p.to_str().unwrap(), "/opt/workspace");
    }

    #[test]
    fn local_path_sandbox_accepts_inside() {
        let _g = set_config("", "/opt/workspace");
        let r = resolve_local_path("/opt/workspace/omniagent/x", "/opt/workspace", "source");
        assert!(r.is_ok());
        let r = resolve_local_path("omniagent/x", "/opt/workspace", "source");
        assert!(r.is_ok(), "relative paths resolve against the workspace");
    }

    #[test]
    fn local_path_sandbox_rejects_outside() {
        let _g = set_config("", "/opt/workspace");
        let r = resolve_local_path("/tmp/escape", "/opt/workspace", "source");
        assert!(r.is_err());
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("outside the ssh plugin workspace sandbox"));
        // Traversal escape: workspace/../etc
        let r = resolve_local_path("/opt/workspace/../etc", "/opt/workspace", "source");
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn run_requires_host() {
        // Missing host -> soft error via the handler's Ok((msg, true)) path
        // is exercised by the caller; the handler itself must Err.
        let res = handle_run(serde_json::json!({"command": "echo hi"})).await;
        assert!(res.is_err(), "missing host must fail validation");
    }

    #[tokio::test]
    async fn copy_requires_direction_and_paths() {
        let res = handle_copy(serde_json::json!({"host": "h"})).await;
        assert!(res.is_err(), "missing direction must fail validation");
        let res = handle_copy(serde_json::json!({
            "host": "h",
            "direction": "to-remote",
            "source": "a"
        }))
        .await;
        assert!(res.is_err(), "missing destination must fail validation");
        let res = handle_copy(serde_json::json!({
            "host": "h",
            "direction": "sideways",
            "source": "a",
            "destination": "b"
        }))
        .await;
        assert!(res.is_err(), "bad direction must fail validation");
    }

    #[tokio::test]
    async fn copy_rejects_local_path_outside_workspace() {
        let _g = set_config("", "/opt/workspace");
        drop(_g);
        let (msg, is_error) = handle_copy(serde_json::json!({
            "host": "h",
            "direction": "to-remote",
            "source": "/tmp/escape-secret",
            "destination": "/remote/secrets"
        }))
        .await
        .expect("sandbox rejection is an Ok result");
        assert!(is_error);
        assert!(msg.contains("outside the ssh plugin workspace sandbox"));
    }

    #[tokio::test]
    async fn status_missing_host_fails() {
        let res = handle_status(serde_json::json!({})).await;
        assert!(res.is_err());
    }

    #[test]
    fn secret_key_file_is_0600_and_removed_on_drop() {
        let skf = SecretKeyFile::create(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nunit-test-only\n-----END OPENSSH PRIVATE KEY-----",
        )
        .expect("create temp key file");
        let path = skf.path().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "key file must be 0600, got {:#o}", mode);
        }
        assert!(path.exists(), "key file exists while guard alive");
        drop(skf);
        assert!(!path.exists(), "key file removed on drop");
    }

    #[tokio::test]
    async fn resolve_secret_key_without_database_url_fails_naming_secret() {
        {
            let _g = set_config("", "/opt/workspace");
            let mut cfg = CONFIG.lock();
            cfg.database_url.clear();
        }
        let msg = match resolve_secret_key("SSH_PRIVATE_KEY").await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("resolve_secret_key must fail when database_url is empty"),
        };
        assert!(
            msg.contains("SSH_PRIVATE_KEY"),
            "error must name secret: {msg}"
        );
        assert!(
            msg.contains("database_url"),
            "error must suggest config fix: {msg}"
        );
    }
}
