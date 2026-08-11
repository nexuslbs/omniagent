//! mcp-server-git: standalone MCP server for Git/GitHub operations.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: create_github_repo, clone_repo, commit_and_push, status
//!
//! GitHub App authentication uses JWT tokens from a private key file,
//! exchanged for installation access tokens via the GitHub API.

use anyhow::{Context, Result};
use mcp_server_util::*;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

//  Constants

#[derive(Clone)]
struct Config {
    omni_dir: String,
    workspace_dir: String,
    github_app_token: String,
    github_app_private_key: String,
    github_app_id: String,
    github_installation_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            omni_dir: "/opt/omni".to_string(),
            workspace_dir: "/opt/workspace".to_string(),
            github_app_token: String::new(),
            github_app_private_key: String::new(),
            github_app_id: String::new(),
            github_installation_id: String::new(),
        }
    }
}

static CONFIG: Lazy<Mutex<Config>> = Lazy::new(|| Mutex::new(Config::default()));

/// Path to the GitHub App private key used for JWT signing.
///
/// The key provided through plugin config (`github_app_private_key`, e.g.
/// resolved from `$secret:GITHUB_APP_KEY` by the core) is written to a
/// STABLE temp path (chmod 600) and used from there. This keeps the durable
/// key in the secrets store and lets the plugin regenerate installation
/// tokens indefinitely — no 1-hour static token to expire.
///
/// The path is fixed (NOT pid-suffixed) so repeated calls in the same process
/// reuse one file, and if /tmp is ever cleaned mid-run the next call simply
/// rewrites it. The write is verified (stat after write) so a failure surfaces
/// as a clear error instead of a confusing openssl "private key not found".
///
/// Fallback: the legacy on-disk key path for local development — only used
/// when the config key is empty AND that legacy file actually exists.
fn resolve_key_path() -> Result<String> {
    let key_cfg = CONFIG.lock().github_app_private_key.clone();
    if !key_cfg.is_empty() {
        let stable = "/tmp/mcp-git-gh-key.pem".to_string();
        match std::fs::write(&stable, key_cfg.as_bytes()) {
            Ok(()) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&stable, std::fs::Permissions::from_mode(0o600));
                // Verify the file actually exists on disk — if the write was
                // silently lost (e.g. /tmp remounted, cleaned between calls),
                // fail loudly rather than let openssl fail cryptically later.
                if Path::new(&stable).exists() {
                    return Ok(stable);
                }
                anyhow::bail!(
                    "GitHub App private key write to {} was not persisted — cannot sign JWT",
                    stable
                );
            }
            Err(e) => anyhow::bail!(
                "Failed to write GitHub App private key to {}: {}",
                stable,
                e
            ),
        }
    }
    let data_dir = CONFIG.lock().omni_dir.clone();
    let legacy = format!(
        "{0}/data/credentials/nexuslbs-app.2026-06-04.private-key.pem",
        data_dir
    );
    if Path::new(&legacy).exists() {
        return Ok(legacy);
    }
    anyhow::bail!(
        "No GitHub App credentials configured: set github_app_private_key \
         (PEM, via $secret:) plus github_app_id and github_installation_id \
         in the git plugin config (no legacy key at {})",
        legacy
    )
}

const GITHUB_ORG: &str = "nexuslbs";
const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "mcp-server-git";

//  Token Cache

struct TokenCacheInner {
    token: Option<(String, u64)>,
}

impl TokenCacheInner {
    fn get_cached(&self) -> Option<String> {
        let (token, expiry) = self.token.as_ref()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now < *expiry - 60 {
            Some(token.clone())
        } else {
            None
        }
    }

    fn set(&mut self, token: String) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.token = Some((token, now + 570));
    }
}

static TOKEN_CACHE: Lazy<Mutex<TokenCacheInner>> =
    Lazy::new(|| Mutex::new(TokenCacheInner { token: None }));

//  Helpers

/// Base64url encode without padding.
fn base64url_encode(data: &[u8]) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(data)
}

/// Load GITHUB_APP_ID and GITHUB_INSTALLATION_ID from plugin config.
/// Values arrive via the configure message (config_schema defaults resolve
/// `$env:GITHUB_APP_ID` / `$env:GITHUB_INSTALLATION_ID` in the core).
/// No direct env-var or .env reads.
fn load_github_creds() -> Result<(String, String)> {
    let cfg = CONFIG.lock();
    if cfg.github_app_id.is_empty() || cfg.github_installation_id.is_empty() {
        anyhow::bail!(
            "GITHUB_APP_ID and GITHUB_INSTALLATION_ID must be set in the plugin config \
             (config_schema defaults resolve them from the GITHUB_APP_ID / GITHUB_INSTALLATION_ID \
             env vars of the omniagent process)"
        );
    }
    Ok((
        cfg.github_app_id.clone(),
        cfg.github_installation_id.clone(),
    ))
}

/// Create a JWT using RS256 via openssl subprocess.
///
/// Fully async (tokio::process): openssl runs as a child process, never
/// blocking an async worker thread (Aug 2026 all-plugins-async push).
async fn create_jwt(app_id: &str) -> Result<String> {
    let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System time before epoch")?
        .as_secs();

    let payload_obj = serde_json::json!({
        "iat": now as i64 - 60,
        "exp": now as i64 + 600,
        "iss": app_id,
    });
    let payload = base64url_encode(payload_obj.to_string().as_bytes());

    let signing_input = format!("{}.{}", header, payload);

    let key_path = resolve_key_path()?;
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign", &key_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn openssl process")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("Failed to open openssl stdin")?;
        stdin.write_all(signing_input.as_bytes()).await?;
        stdin.flush().await?;
    }

    // Bounded: openssl signing is local and instant; a 15s cap is generous.
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .context("openssl signing timed out")?
        .context("Failed to wait for openssl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("openssl signing failed: {}", stderr);
    }

    let signature = base64url_encode(&output.stdout);
    Ok(format!("{}.{}", signing_input, signature))
}

/// Exchange a JWT for a GitHub App installation access token.
async fn get_installation_token(app_id: &str, inst_id: &str) -> Result<String> {
    // Check cache first
    {
        let cache = TOKEN_CACHE.lock();
        if let Some(cached) = cache.get_cached() {
            return Ok(cached);
        }
    }

    let jwt = create_jwt(app_id).await?;
    let url = format!("{}/app/installations/{}/access_tokens", GITHUB_API, inst_id);

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .body("")
        .send()
        .await
        .context("GitHub API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API error {}: {}", status, body);
    }

    let data: Value = response
        .json()
        .await
        .context("Failed to parse GitHub API response")?;
    let token = data["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No token in GitHub API response"))?
        .to_string();

    {
        let mut cache = TOKEN_CACHE.lock();
        cache.set(token.clone());
    }
    Ok(token)
}

/// Get a GitHub installation access token.
///
/// Preferred: regenerate via JWT → installation-token exchange using the app
/// private key (`github_app_private_key`, e.g. resolved from
/// `$secret:GH_APP_PRIVATE_KEY` by the core). Tokens are cached and
/// re-exchanged before GitHub's 1-hour expiry, so auth never goes stale.
///
/// Legacy fallback: a static token from config (`github_app_token`, e.g.
/// `$secret:GH_APP_TOKEN`) — used directly, no JWT/private key needed.
async fn get_github_token() -> Result<String> {
    let (key_cfg, static_token) = {
        let cfg = CONFIG.lock();
        (
            cfg.github_app_private_key.clone(),
            cfg.github_app_token.clone(),
        )
    };

    // Durable path: regenerate an installation token from the app private key.
    // resolve_key_path() now returns Err (with a clear message) when neither
    // the config key nor a legacy key file exists — treat that as "no key".
    if !key_cfg.is_empty() || resolve_key_path().is_ok() {
        let (app_id, inst_id) = load_github_creds()?;
        return get_installation_token(&app_id, &inst_id).await;
    }

    // Legacy fallback: a static installation token from config.
    if !static_token.is_empty() {
        return Ok(static_token);
    }

    anyhow::bail!(
        "No GitHub App credentials configured: set github_app_private_key \
         (PEM, via $secret:) plus github_app_id and github_installation_id \
         in the git plugin config"
    )
}

/// Run a git command and return (stdout, stderr, exit_code).
///
/// Fully async (tokio::process) — a git subprocess NEVER blocks an async
/// worker thread (Aug 2026 all-plugins-async push). `kill_on_drop(true)`
/// guarantees that when the timeout fires (the future holding the child is
/// dropped), git is killed rather than left lingering holding repo locks.
async fn run_git(args: &[&str], cwd: Option<&str>, timeout_secs: u64) -> (String, String, i32) {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // CRITICAL: pipe stdout/stderr BEFORE spawn. `wait_with_output()` only
    // captures pipes that were configured on the Command; without this, git
    // inherits the plugin's own stdout/stderr (the MCP channel + logs), every
    // run returns empty output, and the MCP JSON-RPC stream gets corrupted by
    // leaked git output.
    // stdin MUST be /dev/null, not inherited: a spawned git child that
    // inherits fd 0 (the MCP JSON-RPC pipe) can consume protocol bytes meant
    // for the server reader — the same request-loss class as the docker
    // compose CLI children (G17b, Aug 2026).
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (String::new(), format!("Failed to spawn git: {}", e), -1),
    };

    let timeout = Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            tracing::info!(
                "run_git {:?} -> rc={} stdout_len={} stderr_len={} out_head={:?}",
                args,
                output.status.code().unwrap_or(-1),
                out.len(),
                err.len(),
                &out.chars().take(60).collect::<String>(),
            );
            (out, err, output.status.code().unwrap_or(-1))
        }
        Ok(Err(e)) => (String::new(), format!("git output error: {}", e), -1),
        Err(_) => (
            String::new(),
            format!("git command timed out after {}s", timeout_secs),
            -1,
        ),
    }
}

//  Tool Handlers

/// `create_github_repo`: create a repository under nexuslbs org.
async fn handle_create_github_repo(args: Value) -> Result<(String, bool)> {
    let repo_name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: name"))?
        .to_string();

    if repo_name.is_empty() {
        anyhow::bail!("Repository name cannot be empty");
    }

    let description = args["description"].as_str().unwrap_or("");
    let private = args["private"].as_bool().unwrap_or(false);
    let token = get_github_token().await?;

    let url = format!("{}/orgs/{}/repos", GITHUB_API, GITHUB_ORG);
    let payload = serde_json::json!({
        "name": repo_name,
        "description": description,
        "private": private,
        "auto_init": false,
        "gitignore_template": "",
    });

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", USER_AGENT)
        .json(&payload)
        .send()
        .await
        .context("GitHub API request failed")?;

    let status = response.status();
    let body: Value = response.json().await.unwrap_or(serde_json::json!({}));

    if status.is_success() || status.as_u16() == 422 {
        let clone_url_default = format!("https://github.com/{}/{}.git", GITHUB_ORG, repo_name);
        let html_url_default = format!("https://github.com/{}/{}", GITHUB_ORG, repo_name);

        let clone_url = body["clone_url"].as_str().unwrap_or(&clone_url_default);
        let html_url = body["html_url"].as_str().unwrap_or(&html_url_default);

        let note = if status.as_u16() == 422 {
            "Repository already exists"
        } else {
            "Repository created"
        };

        let result = serde_json::json!({
            "success": true,
            "repo_name": repo_name,
            "clone_url": clone_url,
            "html_url": html_url,
            "note": note,
        });

        return Ok((serde_json::to_string_pretty(&result)?, false));
    }

    let err_msg = body["message"].as_str().unwrap_or("Unknown error");
    Ok((
        format!("GitHub API error ({}): {}", status.as_u16(), err_msg),
        true,
    ))
}

/// Resolve the base directory git operations should run in.
///
/// Priority:
///   1. Explicit `workspace_dir` from plugin config (see plugin.json).
///   2. `/opt/workspace` if it exists (dev/omnidev layout).
///   3. `{omni_dir}/data/workspace` (created on demand).
///
/// The git plugin must NEVER default to its own plugin directory — that
/// pollutes the repo tree with clones.
fn resolve_workspace_dir() -> String {
    let (cfg_ws, omni) = {
        let cfg = CONFIG.lock();
        (cfg.workspace_dir.clone(), cfg.omni_dir.clone())
    };
    if !cfg_ws.is_empty() {
        return cfg_ws;
    }
    if Path::new("/opt/workspace").is_dir() {
        return "/opt/workspace".to_string();
    }
    format!("{}/data/workspace", omni)
}

/// Ensure the workspace directory exists and is usable.
fn ensure_workspace_dir() -> Result<String> {
    let dir = resolve_workspace_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create workspace dir {}", dir))?;
    Ok(dir)
}

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem (pure string/component math, no symlinks).
fn normalize_path(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
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

/// Validate that a repo path is INSIDE the git workspace sandbox.
///
/// Git operations are only allowed on repositories located in the configured
/// `workspace_dir` (default `/opt/workspace`) or one of its subdirectories.
/// This prevents the agent from running git against arbitrary paths outside
/// the workspace (e.g. `/opt/omni`, `/`, or other containers' data).
///
/// Accepts the workspace dir itself (a repo at the workspace root) and any
/// descendant. Rejects absolute paths outside the workspace. Resolves `..`
/// components so `workspace/../etc` cannot escape the sandbox.
fn validate_repo_within_workspace(repo_dir: &str) -> Result<()> {
    let ws = resolve_workspace_dir();
    let ws_abs = Path::new(&ws);
    let repo_abs = Path::new(repo_dir);

    // If the repo path is relative, it's relative to the workspace root.
    let repo_path = if repo_abs.is_absolute() {
        repo_abs.to_path_buf()
    } else {
        ws_abs.join(repo_abs)
    };

    let ws_norm = normalize_path(ws_abs);
    let repo_norm = normalize_path(&repo_path);

    if !repo_norm.starts_with(&ws_norm) {
        anyhow::bail!(
            "Path '{}' is outside the git workspace sandbox '{}'. \
             Git operations are only allowed in the workspace dir and its subdirectories.",
            repo_dir,
            ws
        );
    }
    Ok(())
}

/// Assert that a path argument (absolute, or relative to `repo_dir`) resolves
/// inside the workspace sandbox. `label` names the git flag for the error.
fn assert_path_in_workspace(ws: &str, repo_dir: &str, label: &str, p: &str) -> Result<()> {
    let joined = if Path::new(p).is_absolute() {
        p.to_string()
    } else {
        Path::new(repo_dir).join(p).display().to_string()
    };
    let norm = normalize_path(Path::new(&joined));
    let ws_norm = normalize_path(Path::new(ws));
    if !norm.starts_with(&ws_norm) {
        anyhow::bail!(
            "git argument '{}' references '{}', which is outside the git workspace sandbox \
             '{}'. All git operations must stay inside the workspace.",
            label,
            p,
            ws
        );
    }
    Ok(())
}

/// Git config keys whose value is a shell command git may execute.
/// `-c <key>=<value>` overrides carrying these are rejected outright: the
/// command could write anywhere (e.g. `core.pager='sh -c "echo x > /tmp/pwn"'`).
const EXEC_CONFIG_KEYS: &[&str] = &[
    "alias.",
    "core.sshcommand",
    "core.pager",
    "core.editor",
    "core.fsmonitor",
    "core.gitproxy",
    "core.askpass",
    "credential.helper",
    "filter.",
    "difftool.",
    "mergetool.",
    "pager.",
    "sequence.editor",
    "gpg.program",
    "gpg.ssh.program",
    "man.viewer",
];

/// Git config keys whose value is a filesystem path. The path itself must
/// stay inside the workspace (e.g. `core.hooksPath=/etc` would execute
/// attacker-controlled hooks on commit).
const PATH_CONFIG_KEYS: &[&str] = &[
    "core.hookspath",
    "core.excludesfile",
    "core.attributesfile",
    "include.path",
    "http.sslcert",
    "http.sslchainfile",
    "http.sslkey",
];

/// Reject `-c <key>=<value>` config overrides that are exec vectors or point
/// paths outside the workspace. Benign overrides (user.name, core.autocrlf,
/// core.bare, ...) pass through.
fn validate_config_override(ws: &str, repo_dir: &str, kv: &str) -> Result<()> {
    let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
    let key_lc = key.trim().to_ascii_lowercase();

    for pat in EXEC_CONFIG_KEYS {
        let pat = *pat;
        if key_lc == pat || key_lc.starts_with(pat) {
            anyhow::bail!(
                "git argument '-c {}' is not allowed: '{}' configures a command git may \
                 execute, which could write outside the git workspace sandbox '{}'.",
                kv,
                key,
                ws
            );
        }
    }
    for pat in PATH_CONFIG_KEYS {
        let pat = *pat;
        if key_lc == pat || key_lc.starts_with(pat) {
            assert_path_in_workspace(ws, repo_dir, &format!("-c {}", key), value)?;
            break;
        }
    }
    Ok(())
}

/// Extract positional (non-flag) arguments, skipping values of flags that take
/// one. Handles `--flag=value` and attached short values (`-ofile`).
fn positional_args(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i + 1..].iter().cloned());
            break;
        }
        if a.starts_with('-') {
            if a.contains('=') {
                i += 1;
                continue;
            }
            let mut consumed = false;
            for vf in value_flags {
                if a == vf {
                    i += 2;
                    consumed = true;
                    break;
                }
                // attached short form: -o<value>
                if a.starts_with(vf) && vf.len() == 2 && a.len() > 2 {
                    i += 1;
                    consumed = true;
                    break;
                }
            }
            if !consumed {
                i += 1;
            }
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Validate that git ARGUMENTS cannot redirect writes (or executions) outside
/// the workspace sandbox.
///
/// `validate_repo_within_workspace` only pins the process cwd to a repo inside
/// the workspace. Git arguments themselves can redirect operations elsewhere:
///
/// - Global redirects: `-C <dir>`, `--git-dir=`, `--work-tree=`,
///   `--git-common-dir=`, `--object-dir=`, `--exec-path=`, `--template=`,
///   `--shallow-file=` (path or exec vectors).
/// - `-c <key>=<value>` overrides that execute commands (`core.pager`,
///   `alias.*`, `credential.helper`, ...) or point paths outside
///   (`core.hooksPath`, `core.excludesFile`, ...).
/// - Subcommand destinations: `clone <url> <dir>`, `init <dir>`,
///   `config --file <path>`, `archive --output=`, `format-patch -o`,
///   `diff --output=`, `apply --directory=`, `bundle create <file>`,
///   `worktree add|move <path>`, `checkout-index --prefix=`.
/// - `config --global` / `--system` (writes ~/.gitconfig / /etc/gitconfig).
/// - `maintenance start|register` (writes cron/systemd timers outside the repo).
///
/// Returns a tool-error style message (via the caller mapping to
/// `Ok((msg, true))`) so legitimate blocks never trip the MCP circuit breaker.
fn validate_git_args_within_workspace(repo_dir: &str, git_args: &[String]) -> Result<()> {
    let ws = resolve_workspace_dir();

    // ── 1. Global flags (everything before the subcommand token) ──
    let mut subcmd: Option<&str> = None;
    let mut i = 0;
    while i < git_args.len() {
        let a = git_args[i].as_str();
        if !a.starts_with('-') {
            subcmd = Some(a);
            break;
        }
        match a {
            "-C" | "--git-dir" | "--work-tree" | "--git-common-dir" | "--object-dir"
            | "--exec-path" | "--template" | "--shallow-file" => {
                let v = git_args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("git flag '{}' is missing its value", a))?;
                assert_path_in_workspace(&ws, repo_dir, a, v)?;
                i += 2;
                continue;
            }
            "-c" => {
                let kv = git_args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("git flag '-c' is missing its value"))?;
                validate_config_override(&ws, repo_dir, kv)?;
                i += 2;
                continue;
            }
            _ => {
                // --flag=value / -c<kv> / -C<dir> attached forms
                if let Some(v) = a
                    .strip_prefix("--git-dir=")
                    .or_else(|| a.strip_prefix("--work-tree="))
                    .or_else(|| a.strip_prefix("--git-common-dir="))
                    .or_else(|| a.strip_prefix("--object-dir="))
                    .or_else(|| a.strip_prefix("--exec-path="))
                    .or_else(|| a.strip_prefix("--template="))
                    .or_else(|| a.strip_prefix("--shallow-file="))
                {
                    assert_path_in_workspace(&ws, repo_dir, a, v)?;
                    i += 1;
                    continue;
                }
                if let Some(v) = a.strip_prefix("-C") {
                    if !v.is_empty() {
                        assert_path_in_workspace(&ws, repo_dir, "-C", v)?;
                    }
                    i += 1;
                    continue;
                }
                if let Some(kv) = a.strip_prefix("-c") {
                    if !kv.is_empty() {
                        validate_config_override(&ws, repo_dir, kv)?;
                    }
                    i += 1;
                    continue;
                }
                // benign global flag (--no-pager, --version, --help, ...)
                i += 1;
            }
        }
    }

    // ── 2. Subcommand-specific write destinations ──
    // The loop above breaks AT the subcommand token; skip past it so the
    // subcommand itself isn't mistaken for a positional argument.
    let rest = if subcmd.is_some() {
        &git_args[i + 1..]
    } else {
        &git_args[i..]
    };
    let sc = subcmd.unwrap_or("");
    match sc {
        "clone" => {
            // value flags: -o <name>, -b <branch>, -u <upload-pack>,
            // -c <k=v>, --config <k=v>, --depth, --branch, --origin,
            // --reference, --reference-if-able, --separate-git-dir,
            // --template, --filter, --jobs, --server-option
            let value_flags = [
                "-o",
                "-b",
                "-u",
                "-c",
                "--config",
                "--depth",
                "--branch",
                "--origin",
                "--upload-pack",
                "--reference",
                "--reference-if-able",
                "--separate-git-dir",
                "--template",
                "--filter",
                "--jobs",
                "--server-option",
                "--config-env",
            ];
            // explicit path-valued flags must stay inside the workspace
            for w in rest.windows(2) {
                let flag = w[0].as_str();
                if matches!(
                    flag,
                    "--reference" | "--reference-if-able" | "--separate-git-dir" | "--template"
                ) {
                    assert_path_in_workspace(&ws, repo_dir, flag, &w[1])?;
                }
                if flag == "-c" || flag == "--config" || flag == "--config-env" {
                    validate_config_override(&ws, repo_dir, &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--reference=")
                    .or_else(|| arg.strip_prefix("--reference-if-able="))
                    .or_else(|| arg.strip_prefix("--separate-git-dir="))
                    .or_else(|| arg.strip_prefix("--template="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
                if let Some(kv) = arg
                    .strip_prefix("--config=")
                    .or_else(|| arg.strip_prefix("--config-env="))
                {
                    validate_config_override(&ws, repo_dir, kv)?;
                }
            }
            let pos = positional_args(rest, &value_flags);
            // clone [opts] <repo> [<dir>] — destination is the LAST positional
            if let Some(dest) = pos.last() {
                assert_path_in_workspace(&ws, repo_dir, "clone destination", dest)?;
            }
        }
        "init" => {
            let value_flags = [
                "-b",
                "--initial-branch",
                "--separate-git-dir",
                "--template",
                "--object-format",
                "--ref-format",
            ];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "--separate-git-dir" | "--template") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--separate-git-dir=")
                    .or_else(|| arg.strip_prefix("--template="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let pos = positional_args(rest, &value_flags);
            if let Some(dir) = pos.first() {
                assert_path_in_workspace(&ws, repo_dir, "init directory", dir)?;
            }
        }
        "config" => {
            for arg in rest {
                if arg == "--global" || arg == "--system" {
                    anyhow::bail!(
                        "git config {} is not allowed: it writes to {} outside the git \
                         workspace sandbox '{}'. Use repo-local config (omit the flag).",
                        arg,
                        if arg == "--global" {
                            "~/.gitconfig"
                        } else {
                            "/etc/gitconfig"
                        },
                        ws
                    );
                }
            }
            let value_flags = [
                "--file",
                "-f",
                "--blob",
                "--type",
                "--get",
                "--add",
                "--unset",
                "--unset-all",
                "--replace-all",
                "--rename-section",
                "--remove-section",
            ];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "--file" | "-f") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--file=")
                    .or_else(|| arg.strip_prefix("-f="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        "archive" => {
            let value_flags = [
                "--output", "-o", "--prefix", "--format", "--remote", "--exec",
            ];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "--output" | "-o") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
                if w[0] == "--exec" {
                    anyhow::bail!(
                        "git archive --exec runs a remote helper command — not allowed in \
                         the git workspace sandbox '{}'.",
                        ws
                    );
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--output=")
                    .or_else(|| arg.strip_prefix("-o="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        "format-patch" => {
            let value_flags = [
                "-o",
                "--output-directory",
                "--attach",
                "--inline",
                "--subject-prefix",
                "--filename-max-length",
                "--from",
                "--to",
                "--cc",
                "--add-header",
                "--base",
            ];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "-o" | "--output-directory") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--output-directory=")
                    .or_else(|| arg.strip_prefix("-o="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        "diff" | "diff-files" | "diff-index" | "diff-tree" | "apply" | "fast-export" => {
            let value_flags = [
                "--output",
                "--directory",
                "--prefix",
                "--format",
                "--refspec",
            ];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "--output" | "--directory" | "--prefix") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg
                    .strip_prefix("--output=")
                    .or_else(|| arg.strip_prefix("--directory="))
                    .or_else(|| arg.strip_prefix("--prefix="))
                {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        "checkout-index" => {
            let value_flags = ["--prefix"];
            for w in rest.windows(2) {
                if w[0] == "--prefix" {
                    assert_path_in_workspace(&ws, repo_dir, "--prefix", &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg.strip_prefix("--prefix=") {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        "bundle" => {
            // git bundle create <file> [<refs>...]
            let value_flags: [&str; 0] = [];
            let pos = positional_args(rest, &value_flags);
            if pos.first().map(|s| s.as_str()) == Some("create") {
                if let Some(file) = pos.get(1) {
                    assert_path_in_workspace(&ws, repo_dir, "bundle create", file)?;
                }
            }
        }
        "worktree" => {
            // git worktree add|move <path> ...
            let value_flags = ["-b", "-B", "--reason", "--detach", "--checkout"];
            let pos = positional_args(rest, &value_flags);
            if let Some(action) = pos.first() {
                if action == "add" || action == "move" {
                    if let Some(path) = pos.get(1) {
                        assert_path_in_workspace(
                            &ws,
                            repo_dir,
                            &format!("worktree {}", action),
                            path,
                        )?;
                    }
                }
            }
        }
        "maintenance" => {
            for arg in rest {
                if arg == "start" || arg == "register" {
                    anyhow::bail!(
                        "git maintenance {} is not allowed: it registers cron/systemd \
                         timers outside the git workspace sandbox '{}'.",
                        arg,
                        ws
                    );
                }
            }
        }
        "credential-store" => {
            let value_flags = ["--file", "-f"];
            for w in rest.windows(2) {
                if matches!(w[0].as_str(), "--file" | "-f") {
                    assert_path_in_workspace(&ws, repo_dir, &w[0], &w[1])?;
                }
            }
            for arg in rest {
                if let Some(v) = arg.strip_prefix("--file=") {
                    assert_path_in_workspace(&ws, repo_dir, arg, v)?;
                }
            }
            let _ = positional_args(rest, &value_flags);
        }
        _ => {}
    }

    Ok(())
}

/// `clone_repo`: clone a git repository to local filesystem.
async fn handle_clone_repo(args: Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?
        .to_string();

    if url.is_empty() {
        anyhow::bail!("URL cannot be empty");
    }

    let base_dir = ensure_workspace_dir()?;

    let target_dir = args["dir"].as_str().unwrap_or("").to_string();

    // Resolve the clone destination:
    //   - absolute `dir`  → used as-is
    //   - relative `dir`  → resolved under the workspace dir
    //   - no `dir`        → {workspace_dir}/{repo-name}
    let repo_name = url
        .trim_end_matches('/')
        .split('/')
        .next_back()
        .unwrap_or("repo")
        .trim_end_matches(".git")
        .to_string();

    let actual_dir = if target_dir.is_empty() {
        Path::new(&base_dir).join(&repo_name).display().to_string()
    } else if Path::new(&target_dir).is_absolute() {
        target_dir
    } else {
        Path::new(&base_dir).join(&target_dir).display().to_string()
    };

    // Sandbox: clone destinations must stay inside the workspace dir.
    if let Err(e) = validate_repo_within_workspace(&actual_dir) {
        return Ok((e.to_string(), true));
    }

    // Run the clone with an explicit cwd so relative paths never resolve
    // against the plugin's own directory.
    let (_stdout, stderr, rc) = run_git(&["clone", &url, &actual_dir], Some(&base_dir), 120).await;

    if rc != 0 {
        let git_dir = format!("{}/.git", actual_dir);
        if Path::new(&git_dir).is_dir() {
            let abs_path = Path::new(&actual_dir)
                .canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or(actual_dir);
            return Ok((
                serde_json::json!({
                    "success": true,
                    "path": abs_path,
                    "note": "Repository already exists locally"
                })
                .to_string(),
                false,
            ));
        }
        return Ok((format!("Clone failed: {}", stderr), true));
    }

    let abs_path = Path::new(&actual_dir)
        .canonicalize()
        .map(|p| p.display().to_string())
        .unwrap_or(actual_dir);

    Ok((
        serde_json::json!({
            "success": true,
            "path": abs_path,
            "note": "Repository cloned successfully"
        })
        .to_string(),
        false,
    ))
}

/// `commit_and_push`: stage, commit, and push changes.
async fn handle_commit_and_push(args: Value) -> Result<(String, bool)> {
    let repo_dir = args["repo_dir"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: repo_dir"))?
        .to_string();

    if repo_dir.is_empty() {
        anyhow::bail!("repo_dir cannot be empty");
    }

    let message = args["message"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: message"))?
        .to_string();

    if message.is_empty() {
        anyhow::bail!("Commit message cannot be empty");
    }

    // Sandbox FIRST: repo must live inside the configured workspace dir.
    // Returned as a tool error (is_error=true), NOT a handler Err — a
    // sandbox rejection is an expected outcome, not a server failure, and
    // must not trip the MCP circuit breaker.
    if let Err(e) = validate_repo_within_workspace(&repo_dir) {
        return Ok((e.to_string(), true));
    }

    let git_dir = format!("{}/.git", repo_dir);
    if !Path::new(&git_dir).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    // Stage files
    let files = args["files"].as_array();
    let (_stdout, stderr, rc) = if let Some(files_arr) = files {
        let file_strs: Vec<&str> = files_arr.iter().filter_map(|v| v.as_str()).collect();
        if file_strs.is_empty() {
            run_git(&["add", "-A"], Some(&repo_dir), 30).await
        } else {
            let mut git_args = vec!["add"];
            git_args.extend(&file_strs);
            run_git(&git_args, Some(&repo_dir), 30).await
        }
    } else {
        run_git(&["add", "-A"], Some(&repo_dir), 30).await
    };

    if rc != 0 {
        return Ok((format!("git add failed: {}", stderr), true));
    }

    // Commit
    let (out, stderr, rc) = run_git(&["commit", "-m", &message], Some(&repo_dir), 30).await;

    let mut commit_note = String::new();
    if rc != 0 {
        if stderr.contains("nothing to commit") || out.contains("nothing to commit") {
            // Nothing new to commit — but there may still be unpushed
            // commits ahead of origin. Fall through to the push below so
            // `commit_and_push` actually pushes pending work.
            commit_note = "Nothing to commit: working tree clean".to_string();
        } else {
            return Ok((format!("git commit failed: {}", stderr), true));
        }
    }

    // Push
    let token = match get_github_token().await {
        Ok(t) => t,
        Err(e) => {
            return Ok((format!("Cannot push: {}", e), true));
        }
    };

    // Get remote URL
    let (remote_stdout, _, _) =
        run_git(&["remote", "get-url", "origin"], Some(&repo_dir), 15).await;
    let remote_url = remote_stdout.trim().to_string();

    if remote_url.is_empty() {
        anyhow::bail!("No remote 'origin' configured: cannot push");
    }

    // Get current branch
    let (branch_out, _, _) =
        run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(&repo_dir), 15).await;
    let branch = if branch_out.trim().is_empty() {
        "main"
    } else {
        branch_out.trim()
    };

    // Build push URL with token
    let push_url = if remote_url.starts_with("https://") {
        let rest = remote_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&remote_url);
        format!("https://x-access-token:{}@{}", token, rest)
    } else {
        remote_url.clone()
    };

    let (_push_stdout, push_stderr, push_rc) = run_git(
        &["push", &push_url, &format!("HEAD:{}", branch)],
        Some(&repo_dir),
        120,
    )
    .await;

    if push_rc != 0 {
        // Truncate stderr for display
        let truncated = if push_stderr.len() > 500 {
            format!("{}... [truncated]", &push_stderr[..500])
        } else {
            push_stderr.clone()
        };
        return Ok((format!("Push failed: {}", truncated), true));
    }

    // Update local tracking refs
    run_git(&["fetch", "origin", "--quiet"], Some(&repo_dir), 30).await;

    let note = if commit_note.is_empty() {
        "Committed and pushed".to_string()
    } else {
        format!("{}; pushed pending commits", commit_note)
    };

    Ok((
        serde_json::json!({
            "success": true,
            "repo_dir": Path::new(&repo_dir)
                .canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or(repo_dir),
            "note": note
        })
        .to_string(),
        false,
    ))
}

/// `status`: get git status of a repository.
async fn handle_status(args: Value) -> Result<(String, bool)> {
    let repo_dir = args["repo_dir"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: repo_dir"))?
        .to_string();

    if repo_dir.is_empty() {
        anyhow::bail!("repo_dir cannot be empty");
    }

    // Sandbox FIRST: repo must live inside the configured workspace dir.
    if let Err(e) = validate_repo_within_workspace(&repo_dir) {
        return Ok((e.to_string(), true));
    }

    let git_dir = format!("{}/.git", repo_dir);
    if !Path::new(&git_dir).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    let (status_out, _, _) = run_git(&["status"], Some(&repo_dir), 30).await;
    let (branch_out, _, _) =
        run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(&repo_dir), 15).await;

    Ok((
        serde_json::json!({
            "success": true,
            "repo_dir": Path::new(&repo_dir)
                .canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or(repo_dir),
            "branch": branch_out.trim(),
            "status": status_out,
        })
        .to_string(),
        false,
    ))
}

/// Build the `-c` config override that rewrites `https://host/...` to the
/// token-bearing URL for ONE git invocation.
///
/// CRITICAL: no quotes around either side. In a config FILE, quotes are
/// syntactic and stripped by the parser; but with `git -c`, the value is used
/// LITERALLY, so `insteadOf="https://host"` (with quote characters) never
/// matches the real URL — git falls back to unauthenticated https and fails
/// with `could not read Username ... terminal prompts disabled`. Unquoted, git
/// splits `url.<base>.insteadOf` on the last dot and the rewrite works
/// (verified with GIT_TRACE: the remote-https helper is invoked with the
/// token URL).
fn build_instead_of_override(token: &str, host_path: &str) -> String {
    let token_url = format!("https://x-access-token:{}@{}", token, host_path);
    let orig_url = format!("https://{}", host_path);
    format!("url.{}.insteadOf={}", token_url, orig_url)
}

/// `run_command`: run an arbitrary git command in a repository.
///
/// Generic escape hatch — lets the agent run ANY git command the focused
/// tools (status / clone_repo / commit_and_push / create_github_repo) don't
/// cover, e.g. `git log`, `git diff`, `git branch`, `git remote -v`,
/// `git fetch`, `git reset --soft`, `git stash`, `git tag`.
///
/// Args are passed as an array (never a shell string) so no shell injection
/// is possible. `use_auth: true` injects the GitHub App installation token
/// via a `-c url.<token-url>.insteadOf=<original-url>` config override (the
/// repo's own .git/config is NEVER modified), so push/fetch/pull against the
/// origin https remote work with the same auth as commit_and_push.
async fn handle_run_command(args: Value) -> Result<(String, bool)> {
    let repo_dir = args["repo_dir"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: repo_dir"))?
        .to_string();

    if repo_dir.is_empty() {
        anyhow::bail!("repo_dir cannot be empty");
    }

    // Sandbox FIRST: repo must live inside the configured workspace dir.
    if let Err(e) = validate_repo_within_workspace(&repo_dir) {
        return Ok((e.to_string(), true));
    }

    let git_dir = format!("{}/.git", repo_dir);
    if !Path::new(&git_dir).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    let git_args = args["args"]
        .as_array()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Missing required parameter: args (array of git arguments, \
                 e.g. [\"log\", \"--oneline\", \"-5\"])"
            )
        })?
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_string())
        .collect::<Vec<String>>();

    if git_args.is_empty() {
        anyhow::bail!("args cannot be empty");
    }

    // Refuse NUL bytes (would truncate the argv at exec time).
    for a in &git_args {
        if a.contains('\u{0}') {
            anyhow::bail!("Forbidden NUL byte in git argument");
        }
    }

    // Sandbox the ARGUMENTS themselves: git flags like `-C`, `--git-dir`,
    // `--work-tree`, `clone <url> <dir>`, `init <dir>`, `config --global`,
    // `archive --output=`, `-c core.hooksPath=...` can redirect writes (and
    // executions) outside the workspace even when repo_dir is inside it.
    // Returned as a tool error (is_error=true) — never a handler Err — so
    // legitimate blocks don't trip the MCP circuit breaker.
    if let Err(e) = validate_git_args_within_workspace(&repo_dir, &git_args) {
        return Ok((e.to_string(), true));
    }

    let use_auth = args["use_auth"].as_bool().unwrap_or(false);
    let timeout_secs = args["timeout"]
        .as_u64()
        .or_else(|| args["timeout"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(60);

    // ── Auth injection: `git -c url.<token-url>.insteadOf=<orig-url> <args>` ──
    // Never mutates the repo config; only affects this one invocation.
    let mut full_args: Vec<String> = Vec::new();
    if use_auth {
        let token = match get_github_token().await {
            Ok(t) => t,
            Err(e) => return Ok((format!("Cannot authenticate: {}", e), true)),
        };
        // Read the origin remote to know which https host to rewrite.
        let (remote_out, _, _) =
            run_git(&["remote", "get-url", "origin"], Some(&repo_dir), 15).await;
        let remote_url = remote_out.trim().to_string();
        if remote_url.starts_with("https://") {
            let rest = remote_url
                .split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(&remote_url);
            // git -c url.https://x-access-token:TOKEN@host.insteadOf=https://host
            // NOTE: no quotes around either side. In a config FILE, quotes are
            // syntactic and stripped; but with `-c`, git treats them LITERALLY,
            // so `insteadOf="https://host"` would never match the real URL and
            // the push would fall back to unauthenticated https (username
            // prompt / fatal). Unquoted, git splits url.<base>.insteadOf on the
            // last dot and the rewrite works (verified with GIT_TRACE).
            let host_path = rest.split('/').next().unwrap_or(rest);
            let override_cfg = build_instead_of_override(&token, host_path);
            full_args.push("-c".to_string());
            full_args.push(override_cfg);
            tracing::info!(
                "run_command: injected auth for host {} (repo {})",
                host_path,
                repo_dir
            );
        } else {
            tracing::warn!(
                "run_command: use_auth=true but origin '{}' is not https — running unauthenticated",
                remote_url
            );
        }
    }
    full_args.extend(git_args.iter().cloned());

    let arg_refs: Vec<&str> = full_args.iter().map(|s| s.as_str()).collect();
    let (stdout, stderr, rc) = run_git(&arg_refs, Some(&repo_dir), timeout_secs).await;

    // Truncate huge outputs (same policy as docker_compose: 50k chars).
    const MAX_OUT: usize = 50_000;
    let trunc = |s: String| -> String {
        if s.len() > MAX_OUT {
            format!("{}... [truncated from {} chars]", &s[..MAX_OUT], s.len())
        } else {
            s
        }
    };

    Ok((
        serde_json::json!({
            "success": rc == 0,
            "exit_code": rc,
            "command": format!("git {}", git_args.join(" ")),
            "stdout": trunc(stdout),
            "stderr": trunc(stderr),
        })
        .to_string(),
        rc != 0,
    ))
}

//  Main

#[tokio::main]
async fn main() -> Result<()> {
    let tools: Vec<McpToolEntry> = vec![
        McpToolEntry {
            def: McpToolDef {
                name: "create_github_repo".to_string(),
                description:
                    "CREATE a new repository under the nexuslbs organization on GitHub. \
                    The repository is created with no auto-init: it will be empty until pushed to. \
                    If the repository already exists, returns its URL with a note."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Repository name (e.g. 'my-new-project')"
                        },
                        "description": {
                            "type": "string",
                            "description": "Repository description (optional)"
                        },
                        "private": {
                            "type": "boolean",
                            "description": "Whether the repository should be private (default: false)"
                        }
                    },
                    "required": ["name"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_create_github_repo(args).await })),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "clone_repo".to_string(),
                description:
                    "CLONE a git repository to the local filesystem. \
                    Clones into the git workspace directory (/opt/workspace in dev, \
                    configurable via the git plugin 'workspace_dir' setting) — NEVER \
                    into the plugin directory. If 'dir' is absolute it is used as-is; \
                    if relative it is resolved under the workspace directory; if omitted \
                    it defaults to the repository name inside the workspace directory. \
                    If the directory already exists with a .git folder, returns success with a note."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Git clone URL (HTTPS or SSH)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "Target directory: absolute path, or relative path resolved under the git workspace dir (defaults to the repository name in the workspace dir)"
                        }
                    },
                    "required": ["url"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_clone_repo(args).await })),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "commit_and_push".to_string(),
                description:
                    "STAGE, COMMIT, and PUSH changes to GitHub. \
                    Stages all changes by default, or specific files if 'files' is provided. \
                    If there's nothing to commit, returns success with a note. \
                    Authentication is handled internally via GitHub App installation token."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo_dir": {
                            "type": "string",
                            "description": "Path to the git repository"
                        },
                        "message": {
                            "type": "string",
                            "description": "Commit message"
                        },
                        "files": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Specific files to stage (optional, defaults to all changes)"
                        }
                    },
                    "required": ["repo_dir", "message"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_commit_and_push(args).await })),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "status".to_string(),
                description:
                    "GET the git status of a repository (branch, changes, etc.)".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo_dir": {
                            "type": "string",
                            "description": "Path to the git repository"
                        }
                    },
                    "required": ["repo_dir"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_status(args).await })),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "run_command".to_string(),
                description:
                    "RUN an arbitrary git command inside a repository. \
                    Generic escape hatch when the focused git tools (status, clone_repo, \
                    commit_and_push, create_github_repo) are not specific enough. \
                    Examples: [\"log\", \"--oneline\", \"-10\"], [\"diff\"], [\"branch\", \"-a\"], \
                    [\"remote\", \"-v\"], [\"fetch\", \"origin\"], [\"reset\", \"--soft\", \"HEAD~1\"], \
                    [\"stash\"], [\"tag\", \"-l\"], [\"show\", \"HEAD\"]. \
                    'repo_dir' (required) is the path to the git repository; \
                    'args' (required) is the array of git arguments — NEVER a shell string, \
                    so no shell injection is possible. \\
                    SANDBOX: both 'repo_dir' AND every path-bearing argument must stay inside \\
                    the git workspace dir (/opt/workspace by default). Redirect flags \\
                    (-C, --git-dir, --work-tree, --object-dir, --exec-path, --template, \\
                    --shallow-file), write destinations (clone <url> <dir>, init <dir>, \\
                    config --file, archive --output, format-patch -o, diff --output, \\
                    apply --directory, bundle create, worktree add/move, checkout-index \\
                    --prefix), config --global/--system, maintenance start/register, and \\
                    exec/path -c overrides (core.pager, alias.*, credential.helper, \\
                    core.hooksPath, core.excludesFile, ...) are validated and rejected \\
                    when they would leave the workspace. \\
                    'use_auth' (optional bool, default false): when true, injects the GitHub App \\
                    installation token for this single invocation (via a -c url.insteadOf override — \
                    the repo's own .git/config is NEVER modified) so push/fetch/pull against the \
                    origin https remote authenticate like commit_and_push does. \
                    'timeout' (optional number, default 60s) overrides the command timeout."
                        .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo_dir": {
                            "type": "string",
                            "description": "Path to the git repository"
                        },
                        "args": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Git arguments as an array, e.g. [\"log\", \"--oneline\", \"-5\"]"
                        },
                        "use_auth": {
                            "type": "boolean",
                            "description": "Inject GitHub App installation token for this invocation (push/fetch/pull against https origin). Default false."
                        },
                        "timeout": {
                            "type": "number",
                            "description": "Override default timeout in seconds (default 60)"
                        }
                    },
                    "required": ["repo_dir", "args"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_run_command(args).await })),
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-git".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server_with_config(
        server_info,
        tools,
        Some(move |params: Value| {
            // parking_lot: lock() cannot fail — no poisoning, so config
            // reloads can never be silently skipped after a panic.
            let mut cfg = CONFIG.lock();
            if let Some(dir) = params.get("omni_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.omni_dir = dir.to_string();
                }
            }
            if let Some(dir) = params.get("workspace_dir").and_then(|v| v.as_str()) {
                if !dir.is_empty() {
                    cfg.workspace_dir = dir.to_string();
                }
            }
            if let Some(v) = params.get("github_app_token").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    cfg.github_app_token = v.to_string();
                }
            }
            if let Some(v) = params
                .get("github_app_private_key")
                .and_then(|v| v.as_str())
            {
                if !v.is_empty() {
                    cfg.github_app_private_key = v.to_string();
                }
            }
            if let Some(v) = params.get("github_app_id").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    cfg.github_app_id = v.to_string();
                }
            }
            if let Some(v) = params
                .get("github_installation_id")
                .and_then(|v| v.as_str())
            {
                if !v.is_empty() {
                    cfg.github_installation_id = v.to_string();
                }
            }
            tracing::info!("Git plugin configured with omni_dir + github creds");
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

    /// Point the CONFIG workspace_dir at a temp dir for sandbox tests.
    ///
    /// Returns a guard for a static test lock: the CONFIG workspace_dir is
    /// shared mutable state, so every test that calls `set_ws` must bind the
    /// returned guard (`let _g = set_ws(...)`) to serialize against the other
    /// sandbox tests — otherwise a parallel test can flip the dir mid-assert.
    fn set_ws(dir: &str) -> parking_lot::MutexGuard<'static, ()> {
        static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
        // parking_lot: lock() cannot fail, so a panic in one test can never
        // poison the lock and cascade opaque failures onto the others.
        let guard = TEST_LOCK.lock();
        CONFIG.lock().workspace_dir = dir.to_string();
        guard
    }

    /// Create a unique temp dir; returns (workspace_base, repo_path_inside).
    ///
    /// Tests must NOT hardcode host paths like `/opt/workspace/omniagent`: the
    /// production Docker build context has no such directory, so handlers that
    /// probe `{repo}/.git` fail with "Not a git repository", and `git` spawned
    /// with a missing cwd fails to start (rc=-1). A temp dir keeps the sandbox
    /// lexically valid and gives handlers a real dir to probe.
    fn temp_ws() -> (String, String) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "mcp-git-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("create temp repo dir");
        (
            base.to_str().unwrap().to_string(),
            repo.to_str().unwrap().to_string(),
        )
    }

    /// `temp_ws()` plus a real `git init` — for handler tests that probe
    /// `{repo}/.git` before validating arguments.
    async fn temp_git_repo() -> (String, String) {
        let (ws, repo) = temp_ws();
        let (_, err, rc) = run_git(&["init", "-q"], Some(&repo), 15).await;
        assert_eq!(rc, 0, "git init in temp dir failed: {}", err);
        (ws, repo)
    }

    #[test]
    fn sandbox_accepts_workspace_root() {
        let ws = "/opt/workspace";
        let _g = set_ws(ws);
        assert!(validate_repo_within_workspace(ws).is_ok());
    }

    #[test]
    fn sandbox_accepts_subdirectory() {
        let _g = set_ws("/opt/workspace");
        assert!(validate_repo_within_workspace("/opt/workspace/playground/movie-db").is_ok());
        assert!(validate_repo_within_workspace("/opt/workspace/omniagent").is_ok());
    }

    #[test]
    fn sandbox_rejects_outside_path() {
        let _g = set_ws("/opt/workspace");
        let err = validate_repo_within_workspace("/opt/omni/plugins").unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the git workspace sandbox"));
        assert!(validate_repo_within_workspace("/etc").is_err());
        assert!(validate_repo_within_workspace("/tmp").is_err());
    }

    #[test]
    fn sandbox_rejects_traversal_escape() {
        let _g = set_ws("/opt/workspace");
        // workspace/../etc would resolve to /opt/etc — outside the sandbox.
        assert!(validate_repo_within_workspace("/opt/workspace/../etc").is_err());
        // But a traversal that stays inside is fine.
        assert!(validate_repo_within_workspace("/opt/workspace/sub/../omniagent").is_ok());
    }

    #[test]
    fn sandbox_accepts_relative_repo_path() {
        let _g = set_ws("/opt/workspace");
        // Relative paths resolve against the workspace root.
        assert!(validate_repo_within_workspace("omniagent").is_ok());
        assert!(validate_repo_within_workspace("").is_ok()); // workspace itself
    }

    #[test]
    fn sandbox_respects_custom_workspace_dir() {
        let _g = set_ws("/tmp/custom-ws");
        assert!(validate_repo_within_workspace("/tmp/custom-ws/project").is_ok());
        assert!(validate_repo_within_workspace("/opt/workspace/project").is_err());
    }

    #[tokio::test]
    async fn sandbox_clone_destination_enforced() {
        let _g = set_ws("/opt/workspace");
        // Clone with an absolute dir outside the workspace must be rejected
        // as a tool error (is_error=true), before any network access.
        let args = serde_json::json!({
            "url": "https://github.com/nexuslbs/foo.git",
            "dir": "/tmp/escape-clone",
        });
        let (msg, is_error) = handle_clone_repo(args)
            .await
            .expect("returns Ok with is_error");
        assert!(is_error, "clone outside sandbox should be an error result");
        assert!(msg.contains("outside the git workspace sandbox"));
    }

    #[tokio::test]
    async fn sandbox_rejections_do_not_trip_handler_error() {
        let _g = set_ws("/opt/workspace");
        // A sandbox rejection must come back as Ok((msg, true)) — NOT as an
        // Err — so the MCP circuit breaker doesn't count it as a server
        // failure and open the circuit after a few legitimate blocks.
        let (msg, is_error) = handle_status(serde_json::json!({
            "repo_dir": "/tmp/not-a-repo-outside",
        }))
        .await
        .expect("sandbox rejection must be an Ok result");
        assert!(is_error);
        assert!(msg.contains("outside the git workspace sandbox"));
    }

    // ── Argument-level sandbox: redirect flags & write destinations ──

    /// Shorthand: run arg validation against a repo inside a temp workspace.
    fn validate_args(args: &[&str]) -> Result<()> {
        let (ws, repo) = temp_ws();
        let _g = set_ws(&ws);
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        validate_git_args_within_workspace(&repo, &owned)
    }

    #[test]
    fn args_reject_chdir_escape() {
        // The proven live escape: git -C /tmp init wrote /tmp/.git.
        let err = validate_args(&["-C", "/tmp", "init"]).unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the git workspace sandbox"));
        // Attached form too.
        assert!(validate_args(&["-C/tmp", "init"]).is_err());
    }

    #[test]
    fn args_reject_git_dir_redirect() {
        assert!(validate_args(&["--git-dir=/etc", "status"]).is_err());
        assert!(validate_args(&["--work-tree=/", "status"]).is_err());
        assert!(validate_args(&["--git-common-dir=/opt/omni", "status"]).is_err());
        assert!(validate_args(&["--object-dir=/var/lib", "cat-file", "-p", "HEAD"]).is_err());
        assert!(validate_args(&["--exec-path=/tmp", "rev-parse", "HEAD"]).is_err());
        assert!(validate_args(&["--template=/etc", "init"]).is_err());
        assert!(validate_args(&["--shallow-file=/tmp/s", "rev-parse", "HEAD"]).is_err());
        // Space-separated form.
        assert!(validate_args(&["--git-dir", "/etc", "status"]).is_err());
    }

    #[test]
    fn args_reject_clone_destination_escape() {
        let err = validate_args(&["clone", "https://github.com/nexuslbs/foo.git", "/tmp/foo"])
            .unwrap_err();
        assert!(err.to_string().contains("clone destination"));
        // Relative escape via .. — repo_dir is /opt/workspace/omniagent, so
        // ../../escape resolves to /opt/escape (outside the workspace).
        assert!(validate_args(&[
            "clone",
            "https://github.com/nexuslbs/foo.git",
            "../../escape"
        ])
        .is_err());
        // A .. that stays inside the workspace is fine.
        assert!(
            validate_args(&["clone", "https://github.com/nexuslbs/foo.git", "../escape"]).is_ok()
        );
        // Inside workspace is fine.
        assert!(
            validate_args(&["clone", "https://github.com/nexuslbs/foo.git", "subdir/foo"]).is_ok()
        );
    }

    #[test]
    fn args_reject_init_directory_escape() {
        let err = validate_args(&["init", "/tmp/foo"]).unwrap_err();
        assert!(err.to_string().contains("init directory"));
        assert!(validate_args(&["init", "subdir"]).is_ok());
    }

    #[test]
    fn args_reject_config_global_and_file_escape() {
        let err = validate_args(&["config", "--global", "user.name", "x"]).unwrap_err();
        assert!(err.to_string().contains("~/.gitconfig"));
        assert!(validate_args(&["config", "--system", "core.autocrlf", "true"]).is_err());
        let err = validate_args(&["config", "--file", "/tmp/cfg", "user.name", "x"]).unwrap_err();
        assert!(err
            .to_string()
            .contains("outside the git workspace sandbox"));
        // Repo-local config is fine.
        assert!(validate_args(&["config", "user.name", "x"]).is_ok());
    }

    #[test]
    fn args_reject_output_redirection_escape() {
        assert!(validate_args(&["archive", "--output=/tmp/out.tar", "HEAD"]).is_err());
        assert!(validate_args(&["archive", "-o", "/tmp/out.tar", "HEAD"]).is_err());
        assert!(validate_args(&["format-patch", "-o", "/tmp/patchdir"]).is_err());
        assert!(validate_args(&["format-patch", "--output-directory=/tmp/patchdir"]).is_err());
        assert!(validate_args(&["diff", "--output=/tmp/d.patch"]).is_err());
        assert!(validate_args(&["apply", "--directory=/tmp", "x.patch"]).is_err());
        assert!(validate_args(&["checkout-index", "--prefix=/tmp/", "--all"]).is_err());
        assert!(validate_args(&["bundle", "create", "/tmp/x.bundle", "main"]).is_err());
        assert!(validate_args(&["worktree", "add", "/tmp/wt", "-b", "x"]).is_err());
        // Inside-workspace outputs are fine.
        assert!(validate_args(&["archive", "--output=out.tar", "HEAD"]).is_ok());
        assert!(validate_args(&["format-patch", "-o", "patches"]).is_ok());
        assert!(validate_args(&["worktree", "add", "wt", "-b", "x"]).is_ok());
    }

    #[test]
    fn args_reject_exec_config_overrides() {
        let err =
            validate_args(&["-c", "core.pager=sh -c 'echo x > /tmp/pwn'", "log"]).unwrap_err();
        assert!(err.to_string().contains("configures a command"));
        assert!(validate_args(&["-c", "alias.evil=!rm -rf /", "evil"]).is_err());
        assert!(validate_args(&["-c", "credential.helper=!sh -c 'x'", "fetch"]).is_err());
        assert!(validate_args(&["-c", "core.hooksPath=/etc", "commit"]).is_err());
        assert!(validate_args(&["-c", "core.excludesFile=/tmp/x", "status"]).is_err());
        // Benign overrides pass through.
        assert!(validate_args(&["-c", "user.name=Test", "commit"]).is_ok());
        assert!(validate_args(&["-c", "core.autocrlf=true", "status"]).is_ok());
    }

    #[test]
    fn args_reject_maintenance_register() {
        let err = validate_args(&["maintenance", "start"]).unwrap_err();
        assert!(err.to_string().contains("cron/systemd"));
        assert!(validate_args(&["maintenance", "register"]).is_err());
        // Running maintenance in the repo is fine.
        assert!(validate_args(&["maintenance", "run"]).is_ok());
    }

    #[test]
    fn args_allow_benign_commands() {
        assert!(validate_args(&["log", "--oneline", "-10"]).is_ok());
        assert!(validate_args(&["diff"]).is_ok());
        assert!(validate_args(&["status"]).is_ok());
        assert!(validate_args(&["remote", "-v"]).is_ok());
        assert!(validate_args(&["fetch", "origin"]).is_ok());
        assert!(validate_args(&["reset", "--soft", "HEAD~1"]).is_ok());
        assert!(validate_args(&["stash"]).is_ok());
        assert!(validate_args(&["tag", "-l"]).is_ok());
        assert!(validate_args(&["show", "HEAD"]).is_ok());
        assert!(validate_args(&["branch", "-a"]).is_ok());
        assert!(validate_args(&["push", "origin", "main"]).is_ok());
    }

    #[tokio::test]
    async fn args_rejections_do_not_trip_handler_error() {
        let (ws, repo) = temp_git_repo().await;
        let _g = set_ws(&ws);
        // -C escape must come back as Ok((msg, true)), not Err.
        let (msg, is_error) = handle_run_command(serde_json::json!({
            "repo_dir": repo,
            "args": ["-C", "/tmp", "init"],
        }))
        .await
        .expect("sandbox rejection must be an Ok result");
        assert!(is_error);
        assert!(msg.contains("outside the git workspace sandbox"));
    }

    // ── Auth injection (use_auth) ──

    #[test]
    fn auth_override_has_no_literal_quotes() {
        // Regression: with `git -c`, quotes are LITERAL (unlike config files
        // where they're stripped). `insteadOf="https://host"` never matches
        // the real URL, so the push fell back to unauthenticated https and
        // failed with "could not read Username ... terminal prompts disabled".
        let cfg = build_instead_of_override("ghs_TESTTOKEN", "github.com");
        assert_eq!(
            cfg,
            "url.https://x-access-token:ghs_TESTTOKEN@github.com.insteadOf=https://github.com"
        );
        assert!(
            !cfg.contains('"'),
            "override must not contain literal quotes: {}",
            cfg
        );
    }

    #[test]
    fn auth_override_handles_host_with_port() {
        let cfg = build_instead_of_override("tok", "github.com:8443");
        assert!(cfg.contains("https://x-access-token:tok@github.com:8443"));
        assert!(cfg.ends_with(".insteadOf=https://github.com:8443"));
        assert!(!cfg.contains('"'));
    }

    #[tokio::test]
    async fn auth_override_roundtrip_parses_as_git_config_key() {
        // The override must be a syntactically valid `-c name=value` pair that
        // git can parse without error (git config --list round-trips it).
        let cfg = build_instead_of_override("ghs_TESTTOKEN", "github.com");
        // git config --list will fail loudly if the key is malformed.
        // (-c is a global option, so it must precede the subcommand.)
        // Use a temp dir as cwd — the build context has no /opt/workspace.
        let (_ws, repo) = temp_ws();
        let (out, _, rc) =
            run_git(&["-c", cfg.as_str(), "config", "--list"], Some(&repo), 15).await;
        assert_eq!(rc, 0, "git must parse the override: {}", out);
    }

    // ── Mutex poisoning resilience ──
    //
    // Regression tests (Aug 2026): the git plugin's shared state (CONFIG,
    // TOKEN_CACHE) was std::sync::Mutex + .lock().unwrap(). A panic while a
    // handler held the lock POISONED it, permanently bricking the plugin
    // (every later .lock().unwrap() panics) and, in tests, cascading opaque
    // PoisonError failures onto unrelated tests. parking_lot::Mutex never
    // poisons. These tests simulate the panic and prove the plugin survives:
    // they FAIL on the old std::sync::Mutex code and PASS with parking_lot.

    /// Run `f` and report whether it panicked. Simulates a hypothetical
    /// handler bug that panics mid-critical-section while holding a lock.
    fn caught_panic<F: FnOnce()>(f: F) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
    }

    #[tokio::test]
    async fn panic_while_holding_config_does_not_poison_plugin() {
        // Plugin configured and healthy.
        let _g = set_ws("/tmp/poison-test-ws");

        // A handler panics while CONFIG is held. With std::sync::Mutex the
        // mutex is poisoned on unwind; parking_lot releases it cleanly.
        assert!(
            caught_panic(|| {
                let _guard = CONFIG.lock();
                panic!("simulated handler panic while holding CONFIG");
            }),
            "the simulated panic must have been caught"
        );

        // The plugin must still be fully operational.
        assert_eq!(resolve_workspace_dir(), "/tmp/poison-test-ws");
        drop(_g);

        // A git call that ERRORS (sandbox rejection) must come back as a
        // clean tool error — not panic on a poisoned lock.
        let (msg, is_error) = handle_run_command(serde_json::json!({
            "repo_dir": "/etc",
            "args": ["status"],
        }))
        .await
        .expect("git call after simulated poison must return Ok, not panic");
        assert!(is_error);
        assert!(msg.contains("outside the git workspace sandbox"));
    }

    #[test]
    fn panic_while_holding_token_cache_does_not_poison_plugin() {
        assert!(
            caught_panic(|| {
                let _guard = TOKEN_CACHE.lock();
                panic!("simulated handler panic while holding TOKEN_CACHE");
            }),
            "the simulated panic must have been caught"
        );

        // The cache must still be lockable and in its default empty state.
        let cache = TOKEN_CACHE.lock();
        assert!(cache.get_cached().is_none());
    }

    #[tokio::test]
    async fn git_handler_errors_leave_shared_state_healthy() {
        // A git call that fails must not corrupt the shared config: the
        // next call sees the same workspace and behaves identically.
        let _g = set_ws("/tmp/err-test-ws");
        drop(_g);

        let (msg, is_error) = handle_run_command(serde_json::json!({
            "repo_dir": "/etc",
            "args": ["init"],
        }))
        .await
        .expect("sandbox rejection returns Ok, not Err");
        assert!(is_error);
        assert!(msg.contains("outside the git workspace sandbox"));

        // Config intact; a subsequent pure validation still works.
        assert_eq!(resolve_workspace_dir(), "/tmp/err-test-ws");
        assert!(validate_repo_within_workspace("/tmp/err-test-ws").is_ok());
    }
}
