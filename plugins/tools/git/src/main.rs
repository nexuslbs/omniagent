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
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

//  Constants 

fn private_key_path() -> String {
    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| {
        eprintln!("FATAL: OMNI_DIR must be set");
        std::process::exit(1);
    });
    format!(

"{0}/data/credentials/nexuslbs-app.2026-06-04.private-key.pem", data_dir)
}

fn dot_env_path() -> String {
    let data_dir = std::env::var("OMNI_DIR").unwrap_or_else(|_| {
        eprintln!("FATAL: OMNI_DIR must be set");
        std::process::exit(1);
    });
    format!("{}/.env", data_dir)
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

/// Load GITHUB_APP_ID and GITHUB_INSTALLATION_ID from env or .env file.
fn load_github_creds() -> Result<(String, String)> {
    let app_id = std::env::var("GITHUB_APP_ID").ok();
    let inst_id = std::env::var("GITHUB_INSTALLATION_ID").ok();

    if let (Some(a), Some(i)) = (app_id.as_ref(), inst_id.as_ref()) {
        return Ok((a.clone(), i.clone()));
    }

    let mut found_app_id = app_id;
    let mut found_inst_id = inst_id;

    if Path::new(&dot_env_path()).exists() {
        let content = std::fs::read_to_string(&dot_env_path())
            .context("Failed to read .env file")?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let val = v.trim().trim_matches('\'').trim_matches('"').to_string();
                match k.trim() {
                    "GITHUB_APP_ID" if found_app_id.is_none() => found_app_id = Some(val),
                    "GITHUB_INSTALLATION_ID" if found_inst_id.is_none() => found_inst_id = Some(val),
                    _ => {}
                }
            }
        }
    }

    match (found_app_id, found_inst_id) {
        (Some(a), Some(i)) => Ok((a, i)),
        _ => anyhow::bail!(
            "GITHUB_APP_ID and GITHUB_INSTALLATION_ID must be set in environment or {}",
            dot_env_path()
        ),
    }
}

/// Create a JWT using RS256 via openssl subprocess.
fn create_jwt(app_id: &str) -> Result<String> {
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

    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign", &private_key_path()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn openssl process")?;

    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .context("Failed to open openssl stdin")?;
        stdin.write_all(signing_input.as_bytes())?;
        stdin.flush()?;
    }

    let output = child
        .wait_with_output()
        .context("Failed to wait for openssl")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("openssl signing failed: {}", stderr);
    }

    let signature = base64url_encode(&output.stdout);
    Ok(format!("{}.{}", signing_input, signature))
}

/// Exchange a JWT for a GitHub App installation access token.
fn get_installation_token(app_id: &str, inst_id: &str) -> Result<String> {
    // Check cache first
    {
        let cache = TOKEN_CACHE
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        if let Some(cached) = cache.get_cached() {
            return Ok(cached);
        }
    }

    let jwt = create_jwt(app_id)?;
    let url = format!("{}/app/installations/{}/access_tokens", GITHUB_API, inst_id);

    let client = reqwest::blocking::Client::builder()
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
        .context("GitHub API request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        anyhow::bail!("GitHub API error {}: {}", status, body);
    }

    let data: Value = response
        .json()
        .context("Failed to parse GitHub API response")?;
    let token = data["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No token in GitHub API response"))?
        .to_string();

    {
        let mut cache = TOKEN_CACHE
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {}", e))?;
        cache.set(token.clone());
    }
    Ok(token)
}

/// Get a GitHub installation access token.
fn get_github_token() -> Result<String> {
    let (app_id, inst_id) = load_github_creds()?;
    get_installation_token(&app_id, &inst_id)
}

/// Run a git command and return (stdout, stderr, exit_code).
fn run_git(args: &[&str], cwd: Option<&str>, timeout_secs: u64) -> (String, String, i32) {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (String::new(), format!("Failed to spawn git: {}", e), -1),
    };

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    let timeout = Duration::from_secs(timeout_secs);
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => (
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
            output.status.code().unwrap_or(-1),
        ),
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
fn handle_create_github_repo(args: Value) -> Result<(String, bool)> {
    let repo_name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: name"))?
        .to_string();

    if repo_name.is_empty() {
        anyhow::bail!("Repository name cannot be empty");
    }

    let description = args["description"].as_str().unwrap_or("");
    let private = args["private"].as_bool().unwrap_or(false);
    let token = get_github_token()?;

    let url = format!("{}/orgs/{}/repos", GITHUB_API, GITHUB_ORG);
    let payload = serde_json::json!({
        "name": repo_name,
        "description": description,
        "private": private,
        "auto_init": false,
        "gitignore_template": "",
    });

    let client = reqwest::blocking::Client::builder()
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
        .context("GitHub API request failed")?;

    let status = response.status();
    let body: Value = response.json().unwrap_or(serde_json::json!({}));

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
        format!(
            "GitHub API error ({}): {}",
            status.as_u16(),
            err_msg
        ),
        true,
    ))
}

/// `clone_repo`: clone a git repository to local filesystem.
fn handle_clone_repo(args: Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?
        .to_string();

    if url.is_empty() {
        anyhow::bail!("URL cannot be empty");
    }

    let target_dir = args["dir"].as_str().unwrap_or("").to_string();

    let actual_dir = if target_dir.is_empty() {
        url.trim_end_matches('/')
            .split('/')
            .next_back()
            .unwrap_or("repo")
            .trim_end_matches(".git")
            .to_string()
    } else {
        target_dir
    };

    let (_stdout, stderr, rc) = run_git(&["clone", &url, &actual_dir], None, 120);

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
fn handle_commit_and_push(args: Value) -> Result<(String, bool)> {
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

    let git_dir = format!("{}/.git", repo_dir);
    if !Path::new(&git_dir).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    // Stage files
    let files = args["files"].as_array();
    let (_stdout, stderr, rc) = if let Some(files_arr) = files {
        let file_strs: Vec<&str> = files_arr.iter().filter_map(|v| v.as_str()).collect();
        if file_strs.is_empty() {
            run_git(&["add", "-A"], Some(&repo_dir), 30)
        } else {
            let mut git_args = vec!["add"];
            git_args.extend(&file_strs);
            run_git(&git_args, Some(&repo_dir), 30)
        }
    } else {
        run_git(&["add", "-A"], Some(&repo_dir), 30)
    };

    if rc != 0 {
        return Ok((format!("git add failed: {}", stderr), true));
    }

    // Commit
    let (out, stderr, rc) = run_git(&["commit", "-m", &message], Some(&repo_dir), 30);

    if rc != 0 {
        if stderr.contains("nothing to commit") || out.contains("nothing to commit") {
            return Ok((
                serde_json::json!({
                    "success": true,
                    "note": "Nothing to commit: working tree clean"
                })
                .to_string(),
                false,
            ));
        }
        return Ok((format!("git commit failed: {}", stderr), true));
    }

    // Push
    let token = match get_github_token() {
        Ok(t) => t,
        Err(e) => {
            return Ok((format!("Cannot push: {}", e), true));
        }
    };

    // Get remote URL
    let (remote_stdout, _, _) = run_git(&["remote", "get-url", "origin"], Some(&repo_dir), 15);
    let remote_url = remote_stdout.trim().to_string();

    if remote_url.is_empty() {
        anyhow::bail!("No remote 'origin' configured: cannot push");
    }

    // Get current branch
    let (branch_out, _, _) = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(&repo_dir), 15);
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

    let (_push_stdout, push_stderr, push_rc) =
        run_git(&["push", &push_url, &format!("HEAD:{}", branch)], Some(&repo_dir), 120);

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
    run_git(&["fetch", "origin", "--quiet"], Some(&repo_dir), 30);

    Ok((
        serde_json::json!({
            "success": true,
            "repo_dir": Path::new(&repo_dir)
                .canonicalize()
                .map(|p| p.display().to_string())
                .unwrap_or(repo_dir),
            "note": "Committed and pushed"
        })
        .to_string(),
        false,
    ))
}

/// `status`: get git status of a repository.
fn handle_status(args: Value) -> Result<(String, bool)> {
    let repo_dir = args["repo_dir"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter: repo_dir"))?
        .to_string();

    if repo_dir.is_empty() {
        anyhow::bail!("repo_dir cannot be empty");
    }

    let git_dir = format!("{}/.git", repo_dir);
    if !Path::new(&git_dir).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    let (status_out, _, _) = run_git(&["status"], Some(&repo_dir), 30);
    let (branch_out, _, _) = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(&repo_dir), 15);

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
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_create_github_repo(args) })),
        },
        McpToolEntry {
            def: McpToolDef {
                name: "clone_repo".to_string(),
                description:
                    "CLONE a git repository to the local filesystem. \
                    If no target directory is specified, it defaults to the repository name. \
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
                            "description": "Target directory for the clone (optional, defaults to repo name)"
                        }
                    },
                    "required": ["url"]
                }),
            },
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_clone_repo(args) })),
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
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_commit_and_push(args) })),
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
            handler: Box::new(|args: Value, _meta: Option<McpMeta>| Box::pin(async move { handle_status(args) })),
        },
    ];

    let server_info = ServerInfo {
        name: "mcp-server-git".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
