//! Caller-scoped tool execution for `POST /mcp/execute`.
//!
//! Operator requirement (telegram 4134, 2026-10-05): the core HTTP API's
//! tool-execution endpoint used to be a verbatim passthrough - it executed
//! whatever tool name the caller sent, so a profile with a restricted toolset
//! (e.g. `orchestrator`) could invoke capabilities it is not configured to
//! have by POSTing the tool name, bypassing the agent-runtime tool filtering.
//!
//! This module resolves the CALLER's profile and effective toolset with
//! EXACTLY the rule the agent runtime uses (`crate::toolsets`, first-match
//! `workflow_role > workflow > task > board > channel > profile`) and refuses
//! any tool outside that set, WITHOUT executing it.
//!
//! Identity:
//! * the builtin `core__omniagent_api` forwards the executing thread's
//!   identity as HTTP headers (`x-omni-profile`, `x-omni-thread-id`,
//!   `x-omni-channel-id`, taken from its own `AppContext`);
//! * headers take PRIORITY over the body `_meta`, so an agent that has the
//!   builtin cannot downgrade its own identity by rewriting the JSON body;
//! * direct HTTP callers (curl, the deploy harness) declare identity in the
//!   body `_meta` (`profile_name` / `thread_id` / `channel_id`);
//! * a call that declares NO identity at all is DENIED (deny by default,
//!   fail-closed) - there is no silent fallback to "all tools allowed".

use axum::http::HeaderMap;
use serde_json::{json, Value};
use sqlx::PgPool;

/// No caller identity (no profile, no thread) could be resolved.
pub(crate) const CODE_IDENTITY_REQUIRED: &str = "caller_identity_required";
/// The declared profile does not exist in `config/profiles.yml`.
pub(crate) const CODE_UNKNOWN_PROFILE: &str = "caller_profile_unknown";
/// The resolved toolset id is not defined in `config/toolsets.yml`.
pub(crate) const CODE_TOOLSET_UNDEFINED: &str = "caller_toolset_undefined";
/// The requested tool is not part of the caller's effective toolset.
pub(crate) const CODE_NOT_IN_TOOLSET: &str = "tool_not_in_toolset";
/// The thread row of the declared `thread_id` could not be read.
pub(crate) const CODE_LOOKUP_FAILED: &str = "caller_scope_lookup_failed";

/// Header carrying the executing profile name (set by `core__omniagent_api`).
pub(crate) const HDR_PROFILE: &str = "x-omni-profile";
/// Header carrying the executing thread id.
pub(crate) const HDR_THREAD_ID: &str = "x-omni-thread-id";
/// Header carrying the executing channel id (== channel name).
pub(crate) const HDR_CHANNEL_ID: &str = "x-omni-channel-id";

/// Declared caller identity (headers or body `_meta`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallerIdentity {
    pub profile: Option<String>,
    pub thread_id: Option<i64>,
    pub channel_id: Option<String>,
}

/// Trim, drop empty strings: a blank value counts as "not declared".
fn norm(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

impl CallerIdentity {
    /// Identity declared through the HTTP headers set by the builtin
    /// `core__omniagent_api` tool. An unparseable thread id is ignored.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        Self {
            profile: norm(header(HDR_PROFILE)),
            thread_id: header(HDR_THREAD_ID).and_then(|v| v.trim().parse::<i64>().ok()),
            channel_id: norm(header(HDR_CHANNEL_ID)),
        }
    }

    /// Identity declared through the body `_meta` object.
    pub(crate) fn from_meta(meta: Option<&Value>) -> Self {
        let Some(obj) = meta.and_then(|v| v.as_object()) else {
            return Self::default();
        };
        Self {
            profile: norm(obj.get("profile_name").and_then(|v| v.as_str())),
            thread_id: obj.get("thread_id").and_then(|v| v.as_i64()),
            channel_id: norm(obj.get("channel_id").and_then(|v| v.as_str())),
        }
    }

    /// Merge two identities: the HEADER identity wins field by field, so the
    /// builtin-forwarded (truthful) identity can never be downgraded by the
    /// JSON body.
    pub(crate) fn with_header_priority(headers: &Self, body: &Self) -> Self {
        Self {
            profile: headers.profile.clone().or_else(|| body.profile.clone()),
            thread_id: headers.thread_id.or(body.thread_id),
            channel_id: headers
                .channel_id
                .clone()
                .or_else(|| body.channel_id.clone()),
        }
    }
}

/// The effective tool allow-list resolved for one caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallerScope {
    /// `None` = the caller resolves to NO toolset, i.e. every registered tool
    /// (exactly what the corresponding thread/profile would get).
    /// `Some(list)` = only these tools.
    pub allowed: Option<Vec<String>>,
    /// Profile the scope was resolved for (thread profile when a thread id was
    /// declared, otherwise the declared profile).
    pub profile: String,
    /// `(toolset id, level, owner)` when a toolset was resolved.
    pub toolset: Option<(String, String, String)>,
}

/// A refusal: the call must NOT be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeDenial {
    pub status: u16,
    pub code: &'static str,
    pub error: String,
    pub reason: String,
    pub remediation: String,
    pub tool: String,
    pub profile: Option<String>,
    pub toolset: Option<String>,
}

impl ScopeDenial {
    /// JSON body returned to the caller (names the tool and the caller
    /// profile, as the requirement demands).
    pub(crate) fn to_json(&self) -> Value {
        let mut body = json!({
            "success": false,
            "error": self.error,
            "error_code": self.code,
            "tool": self.tool,
            "reason": self.reason,
            "remediation": self.remediation,
        });
        if let Some(profile) = &self.profile {
            body["caller_profile"] = json!(profile);
        }
        if let Some(toolset) = &self.toolset {
            body["toolset"] = json!(toolset);
        }
        body
    }
}

// Flat refusal constructor: one scalar per field keeps every call site readable;
// a params struct would only wrap this single internal helper.
#[allow(clippy::too_many_arguments)]
fn denial(
    status: u16,
    code: &'static str,
    tool: &str,
    profile: Option<String>,
    toolset: Option<String>,
    error: String,
    reason: String,
    remediation: impl Into<String>,
) -> ScopeDenial {
    ScopeDenial {
        status,
        code,
        error,
        reason,
        remediation: remediation.into(),
        tool: tool.to_string(),
        profile,
        toolset,
    }
}

/// Row of the thread identity + stored toolset (`threads.toolset`, the value
/// the thread creator persisted from the first-match resolution).
#[derive(sqlx::FromRow)]
struct ThreadScopeRow {
    profile: String,
    channel_id: String,
    toolset: Option<String>,
}

/// Resolve the caller's effective toolset.
///
/// * a declared `thread_id` that exists in the DB uses the thread's PROFILE
///   and its STORED toolset (the runtime never re-resolves it);
/// * otherwise the declared profile must exist and the standard chain is
///   resolved for it (channel > profile, given the fields the HTTP call can
///   carry);
/// * no identity, an unknown profile or an undefined toolset id is an error -
///   the endpoint then refuses the call.
pub(crate) async fn resolve_scope(
    pool: &PgPool,
    data_dir: &str,
    default_profile: &str,
    identity: &CallerIdentity,
    tool: &str,
) -> Result<CallerScope, ScopeDenial> {
    if let Some(thread_id) = identity.thread_id {
        let row = sqlx::query_as::<_, ThreadScopeRow>(
            "SELECT profile, channel_id, toolset FROM threads WHERE id = $1",
        )
        .bind(thread_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            denial(
                503,
                CODE_LOOKUP_FAILED,
                tool,
                identity.profile.clone(),
                None,
                format!("cannot resolve caller scope: {e}"),
                format!("reading thread {thread_id} failed"),
                "Retry once the database is reachable.",
            )
        })?;
        if let Some(row) = row {
            let id = row
                .toolset
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty());
            let level = format!("thread {thread_id}");
            return match id {
                None => Ok(CallerScope {
                    allowed: None,
                    profile: row.profile,
                    toolset: None,
                }),
                Some(id) => match crate::toolsets::tools_for(data_dir, id) {
                    Ok(tools) => Ok(CallerScope {
                        allowed: Some(tools),
                        profile: row.profile,
                        toolset: Some((id.to_string(), "thread".to_string(), level)),
                    }),
                    Err(message) => Err(denial(
                        403,
                        CODE_TOOLSET_UNDEFINED,
                        tool,
                        Some(row.profile),
                        Some(id.to_string()),
                        format!("toolset '{id}' of thread {thread_id} is not defined in config/toolsets.yml"),
                        message,
                        "Add the toolset to config/toolsets.yml or clear the thread's toolset.",
                    )),
                },
            };
        }
        // Unknown thread id: fall through to the declared profile (if any).
    }

    let Some(profile) = identity.profile.clone() else {
        return Err(denial(
            403,
            CODE_IDENTITY_REQUIRED,
            tool,
            None,
            None,
            "no caller identity: the request declares neither a profile nor a thread".to_string(),
            "mcp/execute is scoped to the caller's toolset and needs the caller profile"
                .to_string(),
            "Send `_meta: {\"profile_name\": \"<profile>\"}` (or a `thread_id`) with the call.",
        ));
    };

    // `ProfileRegistry::get` FALLS BACK to the default profile for an unknown
    // name, so it cannot be used as an existence check: a declared profile is
    // one with an entry in `config/profiles.yml` (the single source of truth),
    // plus the runtime default profile (always resolvable at runtime).
    let declared = crate::profiles_yaml::load_profiles_from(data_dir)
        .map(|file| file.profiles.contains_key(&profile))
        .unwrap_or(false);
    if !declared && profile != default_profile {
        return Err(denial(
            403,
            CODE_UNKNOWN_PROFILE,
            tool,
            Some(profile.clone()),
            None,
            format!("caller profile '{profile}' is not defined in config/profiles.yml"),
            format!("profile '{profile}' does not exist"),
            "Declare a profile defined in config/profiles.yml.",
        ));
    }

    let resolved = crate::toolsets::resolve_for_thread(&crate::toolsets::ThreadToolsetSources {
        data_dir,
        profile: Some(profile.as_str()),
        channel_id: identity.channel_id.as_deref(),
        task: None,
        board: None,
        workflow_id: None,
        workflow_step: None,
    });

    match resolved {
        None => Ok(CallerScope {
            allowed: None,
            profile,
            toolset: None,
        }),
        Some(reference) => match crate::toolsets::tools_for(data_dir, &reference.id) {
            Ok(tools) => Ok(CallerScope {
                allowed: Some(tools),
                profile,
                toolset: Some((
                    reference.id.clone(),
                    reference.level.as_str().to_string(),
                    reference.owner.clone(),
                )),
            }),
            Err(message) => Err(denial(
                403,
                CODE_TOOLSET_UNDEFINED,
                tool,
                Some(profile),
                Some(reference.id.clone()),
                format!(
                    "{} is not defined in config/toolsets.yml",
                    reference.describe()
                ),
                message,
                "Add the toolset to config/toolsets.yml or clear the field that defines it.",
            )),
        },
    }
}

/// Refuse a tool that is not part of the resolved caller toolset.
///
/// The refusal payload is returned by value on purpose: this is not a hot path,
/// and boxing it would push a `Box` through every call site.
#[allow(clippy::result_large_err)]
pub(crate) fn check_tool(scope: &CallerScope, tool: &str) -> Result<(), ScopeDenial> {
    let Some(allowed) = &scope.allowed else {
        return Ok(());
    };
    if allowed.iter().any(|allowed_tool| allowed_tool == tool) {
        return Ok(());
    }
    let toolset = scope
        .toolset
        .as_ref()
        .map(|(id, _, _)| id.clone())
        .unwrap_or_else(|| "<none>".to_string());
    Err(denial(
        403,
        CODE_NOT_IN_TOOLSET,
        tool,
        Some(scope.profile.clone()),
        Some(toolset.clone()),
        format!(
            "tool '{tool}' is not in the toolset of caller profile '{}'",
            scope.profile
        ),
        format!(
            "profile '{}' resolves to toolset '{toolset}'",
            scope.profile
        ),
        "Use a tool the caller profile's toolset allows.",
    ))
}

/// Audit record for every refusal: caller profile, requested tool, toolset
/// and a timestamp (the log line carries all of them).
pub(crate) fn audit_refusal(tool: &str, refusal: &ScopeDenial) {
    let ts_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tracing::warn!(
        target: "mcp_scope",
        tool = %tool,
        caller_profile = refusal.profile.as_deref().unwrap_or("<none>"),
        toolset = refusal.toolset.as_deref().unwrap_or("<none>"),
        code = refusal.code,
        ts_unix = ts_unix,
        "mcp/execute refused: {}",
        refusal.reason
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const TOOLSETS: &str = "\
toolsets:
  readonly:
  - filesystem__read
  - search__wiki
  orchestrator:
  - core__wait_task
  - notes__note_append
";

    const PROFILES: &str = "\
profiles:
  omni: {}
  restricted:
    toolset: readonly
  coordinator:
    toolset: orchestrator
  broken:
    toolset: not_a_toolset
";

    fn fixture(dir: &Path) {
        std::fs::create_dir_all(dir.join("config")).expect("config dir");
        std::fs::write(dir.join("config/toolsets.yml"), TOOLSETS).expect("toolsets.yml");
        std::fs::write(dir.join("config/profiles.yml"), PROFILES).expect("profiles.yml");
    }

    /// A pool that never connects: the profile path does not touch the DB.
    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://omni:omni@127.0.0.1:1/omni")
            .expect("lazy pool")
    }

    #[tokio::test]
    async fn missing_identity_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let err = resolve_scope(
            &lazy_pool(),
            dir.path().to_str().unwrap(),
            "omni",
            &CallerIdentity::default(),
            "ssh__run",
        )
        .await
        .expect_err("anonymous call must be denied");
        assert_eq!(err.status, 403);
        assert_eq!(err.code, CODE_IDENTITY_REQUIRED);
        assert_eq!(err.tool, "ssh__run");
    }

    #[tokio::test]
    async fn unknown_profile_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let identity = CallerIdentity {
            profile: Some("ghost".to_string()),
            ..Default::default()
        };
        let err = resolve_scope(
            &lazy_pool(),
            dir.path().to_str().unwrap(),
            "omni",
            &identity,
            "ssh__run",
        )
        .await
        .expect_err("unknown profile must be denied");
        assert_eq!(err.code, CODE_UNKNOWN_PROFILE);
        assert_eq!(err.profile.as_deref(), Some("ghost"));
    }

    #[tokio::test]
    async fn profile_without_toolset_is_unrestricted() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let identity = CallerIdentity {
            profile: Some("omni".to_string()),
            ..Default::default()
        };
        let scope = resolve_scope(
            &lazy_pool(),
            dir.path().to_str().unwrap(),
            "omni",
            &identity,
            "ssh__run",
        )
        .await
        .expect("omni resolves");
        assert_eq!(scope.allowed, None);
        assert!(check_tool(&scope, "ssh__run").is_ok());
    }

    #[tokio::test]
    async fn profile_toolset_restricts() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let identity = CallerIdentity {
            profile: Some("coordinator".to_string()),
            ..Default::default()
        };
        let scope = resolve_scope(
            &lazy_pool(),
            dir.path().to_str().unwrap(),
            "omni",
            &identity,
            "ssh__run",
        )
        .await
        .expect("coordinator resolves");
        assert_eq!(
            scope.toolset.as_ref().map(|(id, _, _)| id.as_str()),
            Some("orchestrator")
        );
        assert_eq!(
            scope.toolset.as_ref().map(|(_, level, _)| level.as_str()),
            Some("profile")
        );
        assert!(check_tool(&scope, "core__wait_task").is_ok());
        let err = check_tool(&scope, "ssh__run").expect_err("ssh__run is outside the toolset");
        assert_eq!(err.status, 403);
        assert_eq!(err.code, CODE_NOT_IN_TOOLSET);
        assert_eq!(err.profile.as_deref(), Some("coordinator"));
        assert_eq!(err.toolset.as_deref(), Some("orchestrator"));
    }

    #[tokio::test]
    async fn undefined_toolset_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        fixture(dir.path());
        let identity = CallerIdentity {
            profile: Some("broken".to_string()),
            ..Default::default()
        };
        let err = resolve_scope(
            &lazy_pool(),
            dir.path().to_str().unwrap(),
            "omni",
            &identity,
            "filesystem__read",
        )
        .await
        .expect_err("undefined toolset id must be denied");
        assert_eq!(err.code, CODE_TOOLSET_UNDEFINED);
    }

    #[test]
    fn headers_win_over_body() {
        let headers = CallerIdentity {
            profile: Some("coordinator".to_string()),
            thread_id: Some(7),
            channel_id: None,
        };
        let body = CallerIdentity {
            profile: Some("omni".to_string()),
            thread_id: None,
            channel_id: Some("home".to_string()),
        };
        let merged = CallerIdentity::with_header_priority(&headers, &body);
        assert_eq!(merged.profile.as_deref(), Some("coordinator"));
        assert_eq!(merged.thread_id, Some(7));
        assert_eq!(merged.channel_id.as_deref(), Some("home"));
    }

    #[test]
    fn blank_values_count_as_absent() {
        let meta = json!({"profile_name": "  ", "thread_id": 3, "channel_id": ""});
        let identity = CallerIdentity::from_meta(Some(&meta));
        assert_eq!(identity.profile, None);
        assert_eq!(identity.channel_id, None);
        assert_eq!(identity.thread_id, Some(3));
    }

    #[test]
    fn denial_json_names_tool_profile_and_toolset() {
        let refusal = denial(
            403,
            CODE_NOT_IN_TOOLSET,
            "ssh__run",
            Some("coordinator".to_string()),
            Some("orchestrator".to_string()),
            "tool 'ssh__run' is not in the toolset of caller profile 'coordinator'".to_string(),
            "refused".to_string(),
            "use an allowed tool",
        );
        let body = refusal.to_json();
        assert_eq!(body["error_code"], json!(CODE_NOT_IN_TOOLSET));
        assert_eq!(body["tool"], json!("ssh__run"));
        assert_eq!(body["caller_profile"], json!("coordinator"));
        assert_eq!(body["toolset"], json!("orchestrator"));
        assert_eq!(body["success"], json!(false));
    }
}
