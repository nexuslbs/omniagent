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
//!
//! Truthful remote-tracking state (production incident 2026-09-10, telegram
//! thread 1696): the push goes to the NAMED remote `origin` (with the token
//! injected per invocation via `-c url.<token>.insteadOf=<origin>`, never
//! written into the repo's `.git/config`), because only a named-remote push
//! makes git update `refs/remotes/origin/<branch>` - which is what
//! `git status` and the dashboard ahead indicator read. After the push the
//! tracking ref is reconciled (authenticated fetch with a CHECKED exit code,
//! then `update-ref` as a fallback) so a successful sync can never leave the
//! UI reporting "N commits to push".

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

use crate::{
    build_instead_of_override, get_github_token, refresh_remote_tracking_ref, run_git,
    run_git_with_config, validate_repo_within_workspace, RefRefresh, CONFIG, TOKEN_CACHE,
};

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
/// `auth_cfg` carries the per-invocation `-c url.<token-url>.insteadOf=<url>`
/// override that authenticates the NAMED remote `origin` without ever writing
/// the token into the repo's `.git/config`. It is empty for local-path remotes
/// (tests) that need no token.
///
/// Returns the branch that was synced (the caller needs it to reconcile
/// `refs/remotes/origin/<branch>`).
async fn sync_pass(repo_dir: &str, auth_cfg: &[String]) -> Result<String> {
    let (branch_out, _, _) =
        run_git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(repo_dir), 15).await;
    let branch = branch_out.trim();
    let branch = if branch.is_empty() { "main" } else { branch };

    let (_, err, rc) =
        run_git_with_config(auth_cfg, &["fetch", "origin"], Some(repo_dir), 120).await;
    if rc != 0 {
        anyhow::bail!("Fetch failed: {}", truncate_err(&err));
    }
    let (_, err, rc) = run_git_with_config(
        auth_cfg,
        &["pull", "--rebase", "origin"],
        Some(repo_dir),
        120,
    )
    .await;
    if rc != 0 {
        anyhow::bail!("Pull failed: {}", truncate_err(&err));
    }
    // Push to the NAMED remote: an explicit-URL push (`git push <url>
    // HEAD:<branch>`) does NOT update refs/remotes/origin/<branch>, so the
    // dashboard kept showing "N commits to push" after a successful sync.
    let (_, err, rc) = run_git_with_config(
        auth_cfg,
        &["push", "origin", &format!("HEAD:{}", branch)],
        Some(repo_dir),
        120,
    )
    .await;
    if rc != 0 {
        anyhow::bail!("Push failed: {}", truncate_err(&err));
    }
    Ok(branch.to_string())
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

        // Auth injection: `-c url.<token-url>.insteadOf=<origin-url>` for this
        // invocation only (the repo's .git/config is never modified).
        let auth_cfg: Vec<String> = if needs_token {
            let rest = remote_url
                .split_once("://")
                .map(|(_, r)| r)
                .unwrap_or(&remote_url);
            let host_path = rest.split('/').next().unwrap_or(rest);
            vec![
                "-c".to_string(),
                build_instead_of_override(&token, host_path),
            ]
        } else {
            Vec::new()
        };

        match sync_pass(&repo_dir, &auth_cfg).await {
            Ok(branch) => {
                // Make `refs/remotes/origin/<branch>` agree with the push that
                // just landed, so `git status` / the dashboard ahead indicator
                // cannot keep reporting commits that are already on GitHub.
                let (head_out, _, head_rc) =
                    run_git(&["rev-parse", "HEAD"], Some(&repo_dir), 15).await;
                let head_sha = head_out.trim().to_string();
                let refresh = if head_rc == 0 && !head_sha.is_empty() {
                    refresh_remote_tracking_ref(&repo_dir, &branch, &head_sha, &auth_cfg).await
                } else {
                    RefRefresh {
                        ok: false,
                        note: format!(
                            "WARNING: could not read local HEAD; refs/remotes/origin/{} not \
                             verified - git status and the dashboard may still report commits to \
                             push",
                            branch
                        ),
                    }
                };
                return Ok((
                    format!(
                        "Sync complete: fetched, pulled (rebase) and pushed HEAD on {} ({})",
                        repo_dir, refresh.note
                    ),
                    !refresh.ok,
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

    /// Regression (production incident 2026-09-10, telegram thread 1696): the
    /// dashboard kept showing "2 commits to push" although the sync endpoint
    /// had pushed them to GitHub, because the push went to an explicit URL
    /// and the post-push `fetch origin --quiet` (unauthenticated, exit code
    /// discarded) never moved `refs/remotes/origin/<branch>`.
    ///
    /// The repo's fetch refspec is removed so that even a *successful*
    /// `git fetch origin` cannot refresh `origin/main`: the tracking ref can
    /// then only become truthful if the sync explicitly reconciles it after
    /// the push. With the old explicit-URL push this test fails (ahead == 1);
    /// it passes once the ref is reconciled.
    #[tokio::test]
    async fn sync_refreshes_remote_tracking_ref() {
        let base = test_base("trackref");
        let _g = crate::tests::set_ws(base.to_str().unwrap()).await;
        let (work, bare) = make_repo_pair(&base).await;

        // No fetch refspec: `git fetch origin` updates FETCH_HEAD only.
        git(
            &["config", "--unset-all", "remote.origin.fetch"],
            work.to_str().unwrap(),
        )
        .await;

        // A local commit that only the sync will push.
        std::fs::write(work.join("file.txt"), "one\ntwo\n").unwrap();
        git(&["add", "-A"], work.to_str().unwrap()).await;
        git(&["commit", "-m", "local ahead"], work.to_str().unwrap()).await;

        // Sanity: the tracking ref is behind before the sync.
        let (before, _, _) = git(
            &["rev-list", "--count", "origin/main..HEAD"],
            work.to_str().unwrap(),
        )
        .await;
        assert_eq!(before.trim(), "1", "test setup should be 1 commit ahead");

        let (msg, is_error) = handle_git_sync(serde_json::json!({
            "repo_dir": work.to_str().unwrap(),
        }))
        .await
        .expect("sync must return Ok, not Err");
        assert!(!is_error, "sync should succeed: {}", msg);
        assert!(msg.contains("Sync complete"), "{}", msg);

        // The commit REALLY reached the remote...
        let (head, _, _) = git(&["rev-parse", "HEAD"], work.to_str().unwrap()).await;
        let (remote_head, _, _) =
            git(&["rev-parse", "refs/heads/main"], bare.to_str().unwrap()).await;
        assert_eq!(
            head.trim(),
            remote_head.trim(),
            "push did not land on the remote: {}",
            msg
        );

        // ... and the LOCAL remote-tracking ref reflects it, so neither
        // `git status` nor the dashboard reports work that is already pushed.
        let (ahead, _, rc) = git(
            &["rev-list", "--count", "origin/main..HEAD"],
            work.to_str().unwrap(),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(
            ahead.trim(),
            "0",
            "remote-tracking ref stale after a successful sync (ahead={}, msg={})",
            ahead,
            msg
        );
        let (tracking, _, _) = git(
            &["rev-parse", "refs/remotes/origin/main"],
            work.to_str().unwrap(),
        )
        .await;
        assert_eq!(tracking.trim(), head.trim());

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
