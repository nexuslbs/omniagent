//! Self-restart guard (agent phase 1.5, P2 #6): an agent must never tear down
//! the container it runs inside. Moved VERBATIM out of `main_loop.rs` (pure
//! module reorganization, no behavior change) so the main loop stays focused
//! on loop mechanics and the guard lives where its tests document it.

// ── Phase 1.5: Self-restart guard (P2 #6) ─────────────────────────────────
// An agent must never tear down the container it runs inside: a
// `docker compose restart/down/stop/rm/kill` against its OWN compose
// project kills its own thread (thread 488 self-kill). The guard resolves
// the docker-authoritative compose PROJECT NAME of both sides - the agent's
// own container (`com.docker.compose.project` label via `docker inspect`)
// and the target project (`docker compose ... config --format json` →
// `.name`, the exact resolution compose itself performs) - and blocks a
// destructive verb ONLY when the two names are EQUAL. `up` is NEVER blocked
// for any project, and other, unrelated compose projects are
// always manageable. Resolution is DELEGATED to docker/compose; compose's
// precedence chain (name:, COMPOSE_PROJECT_NAME, --project-name, multiple
// -f files, project-directory) is never reimplemented here.

/// Destructive compose verbs that would tear down a running project.
/// `up` is deliberately absent: bringing containers up is never destructive.
const DESTRUCTIVE_COMPOSE_VERBS: &[&str] = &["restart", "down", "stop", "rm", "kill"];

/// The compose CLI binary used for the probes: the operator setting
/// `compose_cli`, default `docker` (audit HV-A2). A podman-docker shim or a
/// differently named CLI on PATH is configured here instead of a code change.
fn compose_cli() -> String {
    crate::runtime_settings::get_str("compose_cli", "docker")
}

/// Emitted ONCE per process when the self-project probe cannot resolve, so a
/// silently degraded self-restart guard is VISIBLE (audit HV-A2: outside a
/// container the guard used to degrade without a word).
static SELF_GUARD_DEGRADED_LOGGED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True when the "guard degraded" WARN still has to be emitted.
fn should_log_degraded(already_logged: bool) -> bool {
    !already_logged
}

fn warn_self_guard_degraded_once() {
    if should_log_degraded(
        SELF_GUARD_DEGRADED_LOGGED.swap(true, std::sync::atomic::Ordering::SeqCst),
    ) {
        tracing::warn!(
            "[self-restart-guard] cannot resolve this agent's own compose project (docker inspect of $HOSTNAME failed); \
             the self-restart guard is DEGRADED and will not block a destructive verb against its own stack"
        );
    }
}

/// Pure decision: block iff the verb is destructive AND both project names
/// resolved AND they are equal. `up`/any other verb → never blocked; an
/// unresolvable name on either side → never blocked (cannot prove self-kill).
fn guard_blocks(verb: &str, self_project: Option<&str>, target_project: Option<&str>) -> bool {
    if !DESTRUCTIVE_COMPOSE_VERBS.contains(&verb) {
        return false;
    }
    match (self_project, target_project) {
        (Some(s), Some(t)) => s == t,
        _ => false,
    }
}

/// Extract the compose verb (first whitespace-separated token of `command`).
fn compose_verb(args: &serde_json::Value) -> Option<&str> {
    let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let verb = cmd.split_whitespace().next().unwrap_or("");
    if verb.is_empty() {
        None
    } else {
        Some(verb)
    }
}

/// Build the delegated `docker compose ... config --format json` invocation
/// that resolves the TARGET project's effective name exactly as compose does.
fn build_target_config_cmd(
    project_dir: &str,
    compose_files: &[String],
    env_file: Option<&str>,
) -> Vec<String> {
    let mut cmd = vec![
        "compose".to_string(),
        "--project-directory".to_string(),
        project_dir.to_string(),
    ];
    for f in compose_files {
        cmd.push("-f".to_string());
        cmd.push(f.clone());
    }
    if let Some(env) = env_file {
        cmd.push("--env-file".to_string());
        cmd.push(env.to_string());
    }
    cmd.push("config".to_string());
    cmd.push("--format".to_string());
    cmd.push("json".to_string());
    cmd
}

/// Build the delegated `docker inspect` invocation that reads the agent's OWN
/// compose project name from the `com.docker.compose.project` label.
fn build_self_inspect_cmd(container_id: &str) -> Vec<String> {
    vec![
        "inspect".to_string(),
        container_id.to_string(),
        "--format".to_string(),
        "{{index .Config.Labels \"com.docker.compose.project\"}}".to_string(),
    ]
}

/// Resolve the agent's own compose project name (authoritative label).
async fn resolve_self_project() -> Option<String> {
    // In a container $HOSTNAME is the container ID.
    let cid = std::env::var("HOSTNAME").ok()?;
    let out = tokio::process::Command::new(compose_cli())
        .env_clear()
        .env("PATH", crate::process_env::child_path())
        .args(build_self_inspect_cmd(&cid))
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() || name.contains("Error") {
        None
    } else {
        Some(name)
    }
}

/// Resolve the TARGET project's effective name by delegating to compose
/// (`config --format json` → `.name`). Fallback: a RUNNING project's
/// containers carry the `com.docker.compose.project` label - read it via
/// `docker ps` filtered by the project's working_dir label.
async fn resolve_target_project(
    project_dir: &str,
    compose_files: &[String],
    env_file: Option<&str>,
) -> Option<String> {
    let out = tokio::process::Command::new(compose_cli())
        // Platform-level env isolation (2026-09-01): the probe inherits NO
        // ambient env (the agent process carries /opt/omni/.env vars, e.g.
        // COMPOSE_PROJECT_NAME=omni-stack, which docker compose would prefer
        // over the env_file). Empty env + explicit minimal PATH only.
        .env_clear()
        .env("PATH", crate::process_env::child_path())
        .args(build_target_config_cmd(
            project_dir,
            compose_files,
            env_file,
        ))
        .output()
        .await
        .ok()?;
    if out.status.success() {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    // Fallback: match the running project by its working_dir label.
    let filter = format!(
        "label=com.docker.compose.project.working_dir={}",
        project_dir
    );
    let out = tokio::process::Command::new(compose_cli())
        .env_clear()
        .env("PATH", crate::process_env::child_path())
        .args(vec![
            "ps".to_string(),
            "-a".to_string(),
            "--filter".to_string(),
            filter,
            "--format".to_string(),
            "{{json .Labels}}".to_string(),
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Ok(labels) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(name) = labels
                .get("com.docker.compose.project")
                .and_then(|n| n.as_str())
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Build the user-facing refusal message for a blocked self-restart
/// `docker_compose` call. Deliberately GENERIC: it must never name a
/// concrete deployment or stack of a specific operator installation.
fn self_restart_block_message(
    verb: &str,
    target_project: Option<&str>,
    self_project: Option<&str>,
) -> String {
    format!(
        "Blocked: docker_compose '{verb}' targets compose project '{target}' - the stack that hosts this agent (self project '{self_name}'). \
         Tearing down the hosting stack kills this thread. Only the operator may restart it. \
         You may manage OTHER, unrelated compose projects freely; `up` is never blocked.",
        target = target_project.unwrap_or("?"),
        self_name = self_project.unwrap_or("?"),
    )
}

/// Evaluate the Phase 1.5 guard for one docker_compose tool call. Returns the
/// block message when the call would tear down the agent's own project.
pub(crate) async fn self_restart_guard_block(args_json: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let verb = compose_verb(&args)?;
    // `up` (and any non-destructive verb) is NEVER blocked - skip the
    // resolution overhead entirely.
    if !DESTRUCTIVE_COMPOSE_VERBS.contains(&verb) {
        return None;
    }
    let project_dir = args
        .get("project_dir")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    if project_dir.is_empty() {
        return None;
    }
    let compose_files: Vec<String> = match args.get("compose_file") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let env_file = args.get("env_file").and_then(|e| e.as_str());
    let self_project = resolve_self_project().await;
    if self_project.is_none() {
        warn_self_guard_degraded_once();
    }
    let target_project = resolve_target_project(project_dir, &compose_files, env_file).await;
    if guard_blocks(verb, self_project.as_deref(), target_project.as_deref()) {
        Some(self_restart_block_message(
            verb,
            target_project.as_deref(),
            self_project.as_deref(),
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod self_restart_guard_tests {
    use super::*;

    #[test]
    fn self_restart_degradation_warns_once() {
        assert!(
            should_log_degraded(false),
            "first unresolvable probe must warn"
        );
        assert!(!should_log_degraded(true), "repeat probes stay silent");
    }

    #[test]
    fn blocks_when_self_equals_target() {
        assert!(guard_blocks(
            "restart",
            Some("omnistable"),
            Some("omnistable")
        ));
        assert!(guard_blocks("down", Some("omnidev"), Some("omnidev")));
        assert!(guard_blocks("stop", Some("omnistable"), Some("omnistable")));
        assert!(guard_blocks("rm", Some("omnistable"), Some("omnistable")));
        assert!(guard_blocks("kill", Some("omnistable"), Some("omnistable")));
    }
    #[test]
    fn refusal_message_is_generic() {
        // The user-facing refusal text must not name the operator's concrete
        // deployment/stack (audit V-14): only a generic description.
        let msg = self_restart_block_message("down", Some("some-project"), Some("some-project"));
        assert!(msg.contains("the stack that hosts this agent"));
        assert!(msg.contains("docker_compose 'down'"));
        assert!(msg.contains("some-project"));
        assert!(msg.contains("`up` is never blocked"));
        assert!(!msg.contains("omnidev"));
        assert!(!msg.contains("omnistable"));
    }

    #[test]
    fn allows_different_projects() {
        // An omnistable agent MUST be able to manage the omnidev dev stack.
        assert!(!guard_blocks(
            "restart",
            Some("omnistable"),
            Some("omnidev")
        ));
        assert!(!guard_blocks("down", Some("omnidev"), Some("omnistable")));
        assert!(!guard_blocks("stop", Some("omnistable"), Some("omnidev")));
    }

    #[test]
    fn up_is_never_blocked() {
        assert!(!guard_blocks("up", Some("omnistable"), Some("omnistable")));
        assert!(!guard_blocks("up", Some("omnistable"), Some("omnidev")));
        assert!(!guard_blocks("up", None, None));
    }

    #[test]
    fn unresolvable_names_are_never_blocked() {
        // Cannot prove self-kill → allow (the compose call itself will fail
        // if the project does not exist).
        assert!(!guard_blocks("restart", None, Some("omnistable")));
        assert!(!guard_blocks("restart", Some("omnistable"), None));
        assert!(!guard_blocks("restart", None, None));
    }

    #[test]
    fn non_destructive_verbs_never_block() {
        assert!(!guard_blocks("ps", Some("omnistable"), Some("omnistable")));
        assert!(!guard_blocks(
            "logs",
            Some("omnistable"),
            Some("omnistable")
        ));
        assert!(!guard_blocks(
            "exec",
            Some("omnistable"),
            Some("omnistable")
        ));
    }

    #[test]
    fn resolution_is_delegated_to_compose_config() {
        // The guard must NOT reimplement compose's precedence chain: the
        // target name comes from `docker compose ... config --format json`
        // and the self name from the docker-inspect label.
        let cmd = build_target_config_cmd(
            "/opt/workspace/omni-stack",
            &[
                "docker-compose.yml".to_string(),
                "docker-compose.dev.yml".to_string(),
            ],
            Some("/opt/workspace/omni-deployer/omnidev.env"),
        );
        assert_eq!(
            cmd,
            vec![
                "compose",
                "--project-directory",
                "/opt/workspace/omni-stack",
                "-f",
                "docker-compose.yml",
                "-f",
                "docker-compose.dev.yml",
                "--env-file",
                "/opt/workspace/omni-deployer/omnidev.env",
                "config",
                "--format",
                "json",
            ]
        );
        let inspect = build_self_inspect_cmd("abc123");
        assert_eq!(inspect[0], "inspect");
        assert_eq!(inspect[1], "abc123");
        assert!(inspect[3].contains("com.docker.compose.project"));
    }

    #[test]
    fn effective_name_differs_from_project_dir_basename() {
        // `docker compose config` resolves the REAL project name (which may
        // differ from the project-dir basename due to `name:`,
        // COMPOSE_PROJECT_NAME, --project-name, or -f overrides). Delegation
        // means the guard compares docker-authoritative names, never paths.
        let cmd = build_target_config_cmd("/opt/workspace/omni-stack", &[], None);
        assert!(cmd.contains(&"config".to_string()));
        assert!(cmd.contains(&"--format".to_string()));
        assert!(cmd.contains(&"json".to_string()));
        // The only path that appears is the delegated --project-directory
        // argument; there is no path-derived project-name logic.
        assert_eq!(cmd.iter().filter(|c| c.contains("omni-stack")).count(), 1);
    }

    #[test]
    fn verb_parsed_from_command_arg() {
        let args = serde_json::json!({"command": "restart", "project_dir": "/p"});
        assert_eq!(compose_verb(&args), Some("restart"));
        let args = serde_json::json!({"command": "up -d", "project_dir": "/p"});
        assert_eq!(compose_verb(&args), Some("up"));
        let args = serde_json::json!({});
        assert_eq!(compose_verb(&args), None);
    }
}