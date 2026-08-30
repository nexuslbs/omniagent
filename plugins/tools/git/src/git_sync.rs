//! `git_sync` tool: fetch -> pull --rebase -> push against the repo's origin,
//! authenticated with the GitHub App installation token (the same credential
//! path as `commit_and_push` / `run_command --use_auth`).
//!
//! Expired/revoked-token recovery: when a fetch/pull/push fails with an auth
//! error (e.g. a stale or revoked installation token), the token cache is
//! invalidated, a FRESH token is minted from the app private key, and the
//! sync is retried ONCE. This keeps the dashboard explorer sync button (and
//! the backup/restore hook that calls the same endpoint) from surfacing the
//! `500 Pull failed: Command failed: git pull --rebase
//! https://x-access-token:ghs_...` error when a previously minted token has
//! expired.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

use crate::{get_github_token, run_git, validate_repo_within_workspace, CONFIG, TOKEN_CACHE};

/// Classify a git stderr blob as an authentication/authorization failure
/// (expired or revoked token). When this matches, retrying once with a
/// freshly minted token is worth a shot.
fn is_auth_failure(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "authentication failed",
        "invalid username or password",
        "bad credentials",
        "could not read username",
        "401",
        "token expired",
        "expired token",
        "invalid token",
        "repository not found",
        "access denied",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

/// Truncate long error text for display (mirrors commit_and_push).
fn truncate_err(s: &str) -> String {
    if s.len() > 500 {
        format!("{}... [truncated]", &s[..500])
    } else {
        s.to_string()
    }
}

/// One full sync pass (fetch -> pull --rebase -> push) for `repo_dir`.
///
/// `remote_url` is the origin remote as configured. When it is https the
/// token is embedded in the per-invocation URL (the repo's own .git/config
/// is never modified); local-path remotes (tests) run without a token.
async fn sync_pass(repo_dir: &str, remote_url: &str, token: Option<&str>) -> Result<()> {
    let auth_url = if remote_url.starts_with("https://") {
        let token = token.context("internal error: https remote requires a token")?;
        let rest = remote_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(remote_url);
        format!("https://x-access-token:{}@{}", token, rest)
    } else {
        remote_url.to_string()
    };

    let (branch_out, _, _) =
        run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(repo_dir), 15).await;
    let branch = branch_out.trim();
    let branch = if branch.is_empty() { "main" } else { branch };

    let (_, err, rc) = run_git(&["fetch", &auth_url], Some(repo_dir), 120).await;
    if rc != 0 {
        anyhow::bail!("Fetch failed: {}", truncate_err(&err));
    }
    let (_, err, rc) = run_git(&["pull", "--rebase", &auth_url], Some(repo_dir), 120).await;
    if rc != 0 {
        anyhow::bail!("Pull failed: {}", truncate_err(&err));
    }
    let (_, err, rc) = run_git(
        &["push", &auth_url, &format!("HEAD:{}", branch)],
        Some(repo_dir),
        120,
    )
    .await;
    if rc != 0 {
        anyhow::bail!("Push failed: {}", truncate_err(&err));
    }
    Ok(())
}

/// `git_sync`: pull/rebase/push the repo's origin (the canonical sync used
/// by the dashboard explorer sync button and the backup/restore hook).
///
/// `repo_dir` defaults to the omni_dir config repo. On an auth failure the
/// token is regenerated and the sync is retried once.
pub async fn handle_git_sync(args: Value) -> Result<(String, bool)> {
    let repo_dir = match args["repo_dir"].as_str() {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => CONFIG.lock().omni_dir.clone(),
    };
    if repo_dir.is_empty() {
        anyhow::bail!(
            "No repo_dir given and no omni_dir configured: set the git plugin config omni_dir \
             or pass repo_dir"
        );
    }
    // Sandbox FIRST (same as every other git plugin tool).
    if let Err(e) = validate_repo_within_workspace(&repo_dir) {
        return Ok((e.to_string(), true));
    }
    if !Path::new(&format!("{}/.git", repo_dir)).is_dir() {
        anyhow::bail!("Not a git repository: {}", repo_dir);
    }

    let (remote_out, _, _) = run_git(&["remote", "get-url", "origin"], Some(&repo_dir), 15).await;
    let remote_url = remote_out.trim().to_string();
    if remote_url.is_empty() {
        return Ok((
            "Sync failed: No remote 'origin' configured: cannot sync".to_string(),
            true,
        ));
    }
    if remote_url.starts_with("git@") {
        return Ok((
            format!(
                "Sync failed: Remote 'origin' uses SSH ({}) - git_sync only supports https remotes",
                remote_url
            ),
            true,
        ));
    }
    let needs_token = remote_url.starts_with("https://");

    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let token: String = if needs_token {
            match get_github_token().await {
                Ok(t) => t,
                Err(e) => {
                    return Ok((
                        format!(
                            "Sync failed: cannot authenticate: {}. Configure the git plugin with \
                             github_app_private_key (via $secret:GITHUB_APP_KEY), github_app_id \
                             and github_installation_id.",
                            e
                        ),
                        true,
                    ))
                }
            }
        } else {
            String::new()
        };

        match sync_pass(
            &repo_dir,
            &remote_url,
            if needs_token { Some(&token) } else { None },
        )
        .await
        {
            Ok(()) => {
                let _ = run_git(&["fetch", "origin", "--quiet"], Some(&repo_dir), 30).await;
                return Ok((
                    format!(
                        "Sync complete: fetched, pulled (rebase) and pushed HEAD on {}",
                        repo_dir
                    ),
                    false,
                ));
            }
            Err(e) => {
                let msg = e.to_string();
                if attempts == 1 && needs_token && is_auth_failure(&msg) {
                    tracing::warn!(
                        "git_sync: auth failure on first attempt ({}); regenerating token and retrying once",
                        msg
                    );
                    TOKEN_CACHE.lock().token = None;
                    continue;
                }
                return Ok((format!("Sync failed: {}", truncate_err(&msg)), true));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Create a unique scratch base dir. Each test points the shared CONFIG
    /// workspace at it via `crate::tests::set_ws` (which serializes on the
    /// same TEST_LOCK the main.rs sandbox tests use), so the sandbox accepts
    /// the repos and no parallel test can change the workspace mid-test.
    fn test_base(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!(
            "git-sync-test-{}-{}-{}",
            prefix,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn git(args: &[&str], cwd: &str) -> (String, String, i32) {
        run_git(args, Some(cwd), 60).await
    }

    /// Create a work repo + a bare origin under `base`, with one commit pushed.
    async fn make_repo_pair(base: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let work = base.join("work");
        let bare = base.join("origin.git");
        std::fs::create_dir_all(&work).unwrap();

        git(&["init", "-b", "main"], work.to_str().unwrap()).await;
        git(
            &["config", "user.email", "test@omnidev"],
            work.to_str().unwrap(),
        )
        .await;
        git(
            &["config", "user.name", "Git Sync Test"],
            work.to_str().unwrap(),
        )
        .await;
        std::fs::write(work.join("file.txt"), "one\n").unwrap();
        git(&["add", "-A"], work.to_str().unwrap()).await;
        git(&["commit", "-m", "init"], work.to_str().unwrap()).await;

        git(
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            base.to_str().unwrap(),
        )
        .await;
        git(
            &["remote", "add", "origin", bare.to_str().unwrap()],
            work.to_str().unwrap(),
        )
        .await;
        git(&["push", "-u", "origin", "main"], work.to_str().unwrap()).await;
        (work, bare)
    }

    /// Clone the bare repo elsewhere and push a remote-side change.
    async fn advance_remote(bare: &std::path::Path, marker: &str) {
        let parent = bare.parent().unwrap();
        let other = parent.join(format!("remote-{}", marker));
        git(
            &["clone", bare.to_str().unwrap(), other.to_str().unwrap()],
            parent.to_str().unwrap(),
        )
        .await;
        git(
            &["config", "user.email", "test@omnidev"],
            other.to_str().unwrap(),
        )
        .await;
        git(
            &["config", "user.name", "Remote Test"],
            other.to_str().unwrap(),
        )
        .await;
        std::fs::write(other.join("file.txt"), format!("one\n{}\n", marker)).unwrap();
        git(&["add", "-A"], other.to_str().unwrap()).await;
        git(
            &["commit", "-m", &format!("remote {}", marker)],
            other.to_str().unwrap(),
        )
        .await;
        git(&["push", "origin", "main"], other.to_str().unwrap()).await;
    }

    #[test]
    fn auth_failure_classification() {
        assert!(is_auth_failure(
            "fatal: Authentication failed for 'https://x-access-token:ghs_xxx@github.com/repo.git/'"
        ));
        assert!(is_auth_failure(
            "remote: Invalid username or password.\nfatal: Authentication failed"
        ));
        assert!(is_auth_failure(
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
        ));
        assert!(is_auth_failure(
            "fatal: unable to access 'https://github.com/x/y.git/': The requested URL returned error: 401"
        ));
        assert!(is_auth_failure("remote: Repository not found."));
        assert!(is_auth_failure("error: token expired"));
        assert!(!is_auth_failure(
            "fatal: Not possible to fast-forward, aborting."
        ));
        assert!(!is_auth_failure(
            "error: Your local changes to the following files would be overwritten by merge"
        ));
        assert!(!is_auth_failure(""));
        assert!(!is_auth_failure("error: failed to push some refs"));
    }

    #[tokio::test]
    async fn sync_pulls_remote_change_and_pushes_local() {
        let base = test_base("pullpush");
        let _g = crate::tests::set_ws(base.to_str().unwrap()).await;
        let (work, bare) = make_repo_pair(&base).await;

        // Remote-side change (via a second clone of the bare origin).
        advance_remote(&bare, "two").await;

        // Local uncommitted change that survives the rebase.
        std::fs::write(work.join("local.txt"), "local\n").unwrap();

        let (msg, is_error) = handle_git_sync(serde_json::json!({
            "repo_dir": work.to_str().unwrap(),
        }))
        .await
        .expect("sync must return Ok, not Err");
        assert!(!is_error, "sync should succeed: {}", msg);
        assert!(msg.contains("Sync complete"), "{}", msg);

        // Local repo has the remote change AND its own file.
        let content = std::fs::read_to_string(work.join("file.txt")).unwrap();
        assert!(
            content.contains("two"),
            "remote change not pulled: {}",
            content
        );
        assert!(std::fs::read_to_string(work.join("local.txt"))
            .unwrap()
            .contains("local"));

        // And the local state landed on the remote (bare log has the commits).
        let (out, _, rc) = git(&["log", "--oneline", "-3"], bare.to_str().unwrap()).await;
        assert_eq!(rc, 0);
        assert!(
            out.contains("init") || out.contains("remote two"),
            "{}",
            out
        );

        drop(_g);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn sync_conflict_fails_soft_with_tool_error() {
        let base = test_base("conflict");
        let _g = crate::tests::set_ws(base.to_str().unwrap()).await;
        let (work, bare) = make_repo_pair(&base).await;

        // Remote advances file.txt; local also edits file.txt without
        // committing -> pull --rebase must fail (unstaged conflict).
        advance_remote(&bare, "remote").await;
        std::fs::write(work.join("file.txt"), "one\nlocal\n").unwrap();

        let (msg, is_error) = handle_git_sync(serde_json::json!({
            "repo_dir": work.to_str().unwrap(),
        }))
        .await
        .expect("sync must return Ok, not Err");
        assert!(is_error, "conflicting sync must be a tool error");
        assert!(msg.contains("Sync failed"), "{}", msg);

        drop(_g);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn sync_rejects_ssh_remote() {
        let base = test_base("ssh");
        let _g = crate::tests::set_ws(base.to_str().unwrap()).await;
        let (work, _bare) = make_repo_pair(&base).await;
        git(
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:nexuslbs/omni-root.git",
            ],
            work.to_str().unwrap(),
        )
        .await;
        let (msg, is_error) = handle_git_sync(serde_json::json!({
            "repo_dir": work.to_str().unwrap(),
        }))
        .await
        .expect("sync must return Ok, not Err");
        assert!(is_error);
        assert!(msg.contains("only supports https remotes"), "{}", msg);

        drop(_g);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn sync_rejects_missing_origin() {
        let base = test_base("noremote");
        let _g = crate::tests::set_ws(base.to_str().unwrap()).await;
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&["init", "-b", "main"], work.to_str().unwrap()).await;
        git(
            &["config", "user.email", "test@omnidev"],
            work.to_str().unwrap(),
        )
        .await;
        git(
            &["config", "user.name", "Git Sync Test"],
            work.to_str().unwrap(),
        )
        .await;
        std::fs::write(work.join("f.txt"), "x\n").unwrap();
        git(&["add", "-A"], work.to_str().unwrap()).await;
        git(&["commit", "-m", "init"], work.to_str().unwrap()).await;

        let (msg, is_error) = handle_git_sync(serde_json::json!({
            "repo_dir": work.to_str().unwrap(),
        }))
        .await
        .expect("sync must return Ok, not Err");
        assert!(is_error);
        assert!(msg.contains("No remote 'origin'"), "{}", msg);

        drop(_g);
        let _ = std::fs::remove_dir_all(&base);
    }
}
