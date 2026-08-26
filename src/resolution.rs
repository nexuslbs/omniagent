//! Universal fallback resolution - resolve fields with fallbacks ONCE at load.
//!
//! User principle (2026-08-19): ANY place that uses fields that have
//! fallbacks MUST resolve the fallbacks FIRST, before any shallow use of the
//! raw fields. Resolve as EARLY as possible - right after the row/config is
//! loaded - compute the effective values once, and let consumers use the
//! RESOLVED struct. Never shallow-read a raw field that has a fallback chain
//! (see `Reference/Field-Resolution.md`).
//!
//! Domains covered here:
//! - Kanban task fields (Phase 1 - the live bug): `resolve_task_defaults`
//!   (task → board → channel → global settings). The fail-routing path used
//!   to read `kanban_tasks.workflow_id` raw, so board-based tasks
//!   (workflow_id NULL) lost their workflow entirely and a reviewer
//!   reject landed on `blocked` instead of an executor rework thread.
//! - Channel fields (Phase 2): `resolve_channel` / `effective_channel_name`
//!   (raw name → default-chain + profile/provider/model fallbacks).
//! - Provider/model (Phase 3): the canonical resolver is
//!   `crate::db::threads::resolve_thread_identity` (thread → channel →
//!   profile → global settings → env), called ONCE at thread creation and
//!   persisted on the thread row; `ResolvedThreadProviderModel` is its
//!   provider/model projection.
//! - Settings (Phase 4): `crate::agent::config::AgentConfig` is the
//!   resolved-at-load snapshot (fields are read via `get(key, default)` once
//!   at load; consumers read the snapshot fields, never re-apply defaults).

use crate::channels_yaml::{load_channels_from, ChannelDef};

// ─────────────────────────────────────────────────────────────────────────────
// Phase 1 - Kanban task defaults (task → board → channel → global)
// ─────────────────────────────────────────────────────────────────────────────

/// Raw kanban-task fallback fields - exactly the columns that participate in
/// the task → board → channel → global fallback chain. Consumers MUST pass
/// these to [`resolve_task_defaults`] and use the returned
/// [`ResolvedTaskDefaults`]; they must NEVER read these raw fields directly
/// for behavior (grep audit gate).
#[derive(Debug, Clone, Default)]
pub struct TaskFallbackFields<'a> {
    /// `kanban_tasks.board` (NULL for non-board tasks).
    pub board: Option<&'a str>,
    /// `kanban_tasks.workflow_id` (NULL for board-based tasks).
    pub workflow_id: Option<&'a str>,
    /// `kanban_tasks.channel_id` (NULL for board-based tasks).
    pub channel_id: Option<&'a str>,
    /// `kanban_tasks.profile` (NULL for board-based tasks).
    pub profile: Option<&'a str>,
    /// `kanban_tasks.plan`.
    pub plan: Option<bool>,
    /// `kanban_tasks.template`.
    pub template: Option<&'a str>,
}

/// Effective kanban-task execution options, resolved ONCE at load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedTaskDefaults {
    /// Effective workflow key (task → board). `None` = plain (no-workflow) task.
    pub workflow_id: Option<String>,
    /// Effective channel name (task → board → `default_kanban_channel`
    /// setting → ""). `""` means no channel (thread fails with "no channel
    /// defined" - fail-with-record, never a silent substitution).
    pub channel_id: String,
    /// Effective profile name (task → board → channel → default profile).
    pub profile: String,
    /// Effective plan budget (task → board).
    pub plan: Option<bool>,
    /// Effective thread template (task → board).
    pub template: Option<String>,
}

/// Load a single channel definition from `{data_dir}/config/channels.yml`.
/// Missing file / missing channel / empty name → `None`.
fn channel_def_from(data_dir: &str, name: &str) -> Option<ChannelDef> {
    if name.trim().is_empty() {
        return None;
    }
    load_channels_from(data_dir)
        .ok()
        .and_then(|file| file.channels.get(name).cloned())
}

/// Resolve the effective kanban-task defaults.
///
/// Chain: `Kanban Task > Board > Channel > Global Settings`.
///
/// - `workflow_id`: task → board.workflow
/// - `channel_id`: task → board.channel → `default_kanban_channel` → ""
/// - `profile`: task → board.profile → channel.profile → default profile
/// - `plan`: task → board.plan
///
/// Fail-loud (mirrors `crate::boards::task_board` semantics): when boards.yml
/// exists and the task's board is NULL or unknown → `Err` - never a silent
/// empty fallback that changes behavior. When boards.yml is absent (feature
/// disabled) the board contributes nothing and the task behaves exactly like
/// a non-board task.
pub fn resolve_task_defaults(
    data_dir: &str,
    task: &TaskFallbackFields<'_>,
) -> Result<ResolvedTaskDefaults, String> {
    let board_cfg = crate::boards::task_board(data_dir, task.board)?;

    // workflow_id: task → board.
    let workflow_id = task
        .workflow_id
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            board_cfg
                .as_ref()
                .and_then(|b| b.workflow.clone())
                .filter(|s| !s.is_empty())
        });

    // channel_id: task → board → default_kanban_channel setting → "".
    let explicit_channel = task
        .channel_id
        .filter(|s| !s.is_empty())
        .or_else(|| board_cfg.as_ref().and_then(|b| b.channel.as_deref()));
    let channel_id = effective_channel_name(data_dir, explicit_channel, "default_kanban_channel");

    // profile: task → board.profile → channel.profile → default profile.
    let channel_profile = if channel_id.is_empty() {
        None
    } else {
        channel_def_from(data_dir, &channel_id)
            .and_then(|c| c.profile)
            .filter(|p| !p.trim().is_empty())
    };
    let profile = task
        .profile
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            board_cfg
                .as_ref()
                .and_then(|b| b.profile.clone())
                .filter(|p| !p.trim().is_empty())
        })
        .or(channel_profile)
        .unwrap_or_else(crate::profile::default_profile_name);

    // plan: task → board.
    let plan = task
        .plan
        .or_else(|| board_cfg.as_ref().and_then(|b| b.plan));
    // template: task → board.
    let template = task
        .template
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            board_cfg
                .as_ref()
                .and_then(|b| b.template.clone())
                .filter(|s| !s.is_empty())
        });

    Ok(ResolvedTaskDefaults {
        workflow_id,
        channel_id,
        profile,
        plan,
        template,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2 - Effective channel (raw name → default chain + field fallbacks)
// ─────────────────────────────────────────────────────────────────────────────

/// Effective channel, resolved ONCE at load: the channel NAME (explicit →
/// default setting → "") plus the channel's fallback-bearing fields
/// (profile/provider/model). Global-settings fallback for provider/model is
/// applied at thread-identity resolution (`resolve_thread_identity`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedChannel {
    /// Effective channel name. `""` = no channel (fail-with-record).
    pub name: String,
    /// Channel-level profile override (channels.yml `profile`).
    pub profile: Option<String>,
    /// Channel-level provider override (channels.yml `provider`).
    pub provider: Option<String>,
    /// Channel-level model override (channels.yml `model`).
    pub model: Option<String>,
}

/// Effective channel NAME for a producer (kanban dispatch, hooks, scheduler):
/// an explicit name wins (even when it is not a known channel - the caller
/// then fails the thread with "channel not found", never silently substitutes
/// the default); otherwise the named default-channel setting in settings.yml
/// is used; when neither resolves to a known channel → `""` (the caller
/// creates the thread with an empty channel and fails it with "no channel
/// defined").
///
/// data_dir-parameterized mirror of `channels_yaml::resolve_default_channel`
/// so resolvers are testable without the process-global data dir.
pub fn effective_channel_name(
    data_dir: &str,
    explicit: Option<&str>,
    setting_name: &str,
) -> String {
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let value = crate::server::settings::load_settings_file(data_dir)
        .get(setting_name)
        .cloned()
        .unwrap_or_default();
    let name = value.trim().to_string();
    if name.is_empty() || channel_def_from(data_dir, &name).is_none() {
        return String::new();
    }
    name
}

/// Resolve the effective channel (name + profile/provider/model fallbacks)
/// for a producer. See [`effective_channel_name`] for the name chain; the
/// profile/provider/model come from the channel's channels.yml definition
/// (empty when the channel is unknown - the caller fails with "channel not
/// found").
pub fn resolve_channel(
    data_dir: &str,
    explicit: Option<&str>,
    setting_name: &str,
) -> ResolvedChannel {
    let name = effective_channel_name(data_dir, explicit, setting_name);
    let def = channel_def_from(data_dir, &name);
    ResolvedChannel {
        name,
        profile: def.as_ref().and_then(|c| c.profile.clone()),
        provider: def.as_ref().and_then(|c| c.provider.clone()),
        model: def.as_ref().and_then(|c| c.model.clone()),
    }
}

/// Resolved channel identity: the channel's effective profile/provider/model
/// with fallbacks applied AT LOAD TIME (channels.yml → profile registry →
/// global default provider). Every channel loader hands out these resolved
/// values - never shallow/empty yml fields that still need resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedChannelIdentity {
    /// Effective profile name (yml `profile` → default profile).
    pub profile: String,
    /// Effective provider (yml `provider` → resolved profile's provider →
    /// global default provider). `None` = no provider anywhere.
    pub provider: Option<String>,
    /// Effective model (yml `model` → profile model when the channel does
    /// not pin a provider → the provider's default model).
    pub model: Option<String>,
    /// Effective plan (yml `plan` → profile `plan`). `None` = the plugin
    /// decides at runtime.
    pub plan: Option<bool>,
    /// Effective template (yml `template` → profile `template`).
    pub template: Option<String>,
}

/// Resolve a channel definition's identity fields with fallback, AT LOAD
/// TIME. The chain mirrors `crate::db::threads::resolve_thread_identity`'s
/// channel tier (provider: channel → profile → global; model resolved at the
/// same tier as the provider) so a channel loaded through this resolver
/// reproduces exactly what thread creation would resolve - and a channels.yml
/// edit (e.g. switching a channel's provider) takes effect on the NEXT load,
/// with no restart and no boot-time cache.
pub fn resolve_channel_identity(data_dir: &str, def: &ChannelDef) -> ResolvedChannelIdentity {
    let profile = def
        .profile
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::profile::default_profile_name);

    let profile_data = crate::profile::ProfileRegistry::new(data_dir)
        .get(&profile)
        .cloned();

    let provider = def
        .provider
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            profile_data
                .as_ref()
                .and_then(|p| p.provider.as_deref())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            let prov = crate::agent::config::get_global()
                .map(|g| g.read().default_provider.clone())
                .unwrap_or_default();
            (!prov.trim().is_empty()).then_some(prov)
        });

    let model = def
        .model
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            // When the channel pins a provider but no model, the profile's
            // model is NOT used (channel tier semantics: provider default).
            if def
                .provider
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            {
                None
            } else {
                profile_data
                    .as_ref()
                    .and_then(|p| p.model.as_deref())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            }
        })
        .or_else(|| {
            provider
                .as_deref()
                .and_then(crate::llm::resolve_default_model)
        });

    let plan = def
        .plan
        .or_else(|| profile_data.as_ref().and_then(|p| p.plan));

    let template = def
        .template
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            profile_data
                .as_ref()
                .and_then(|p| p.template.as_deref())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        });

    ResolvedChannelIdentity {
        profile,
        provider,
        model,
        plan,
        template,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 3 - Per-thread provider/model (resolved once at thread creation)
// ─────────────────────────────────────────────────────────────────────────────

/// Resolved per-thread provider/model identity, resolved ONCE at thread
/// creation and persisted on the thread row (running threads never
/// re-resolve it).
///
/// Chain: thread-stamped → channel → profile → global settings → env
/// (LLM_PROVIDER/LLM_MODEL with openai/gpt-4 fallbacks).
///
/// The canonical resolver is `crate::db::threads::resolve_thread_identity`
/// (it returns profile + provider + model); this struct is the
/// provider/model projection used by consumers that only need those two
/// fields. Every thread creator funnels through
/// `crate::db::threads::create_thread_with_cause`, which calls
/// `resolve_thread_identity` exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedThreadProviderModel {
    pub provider: String,
    pub model: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp data dir with the given boards.yml / channels.yml /
    /// settings.yml content (None = file absent).
    fn temp_data_dir(
        boards_yml: Option<&str>,
        channels_yml: Option<&str>,
        settings_yml: Option<&str>,
    ) -> String {
        let dir = std::env::temp_dir().join(format!(
            "resolution-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        if let Some(y) = boards_yml {
            std::fs::write(dir.join("config").join("boards.yml"), y).unwrap();
        }
        if let Some(y) = channels_yml {
            std::fs::write(dir.join("config").join("channels.yml"), y).unwrap();
        }
        if let Some(y) = settings_yml {
            std::fs::write(dir.join("config").join("settings.yml"), y).unwrap();
        }
        dir.to_str().unwrap().to_string()
    }

    #[test]
    fn resolve_task_defaults_plain_task_boards_disabled() {
        // No boards.yml → board contributes nothing; the task's own fields
        // are the effective values (workflow/channel/profile/plan).
        let dir = temp_data_dir(None, None, None);
        let resolved = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: None,
                workflow_id: Some("omniagent-dev"),
                channel_id: Some("kanban"),
                profile: Some("omni"),
                plan: Some(true),
                template: None,
            },
        )
        .expect("boards disabled → no board error");
        assert_eq!(resolved.workflow_id.as_deref(), Some("omniagent-dev"));
        assert_eq!(resolved.channel_id, "kanban");
        assert_eq!(resolved.profile, "omni");
        assert_eq!(resolved.plan, Some(true));
    }

    #[test]
    fn resolve_channel_identity_passes_through_yml_fields() {
        let dir = temp_data_dir(
            None,
            Some(
                r#"
channels:
  mm-kanban:
    profile: omni
    provider: opencode-go
    model: opencode-mini
"#,
            ),
            None,
        );
        let def = channel_def_from(&dir, "mm-kanban").expect("channel present");
        let r = resolve_channel_identity(&dir, &def);
        assert_eq!(r.profile, "omni");
        assert_eq!(r.provider.as_deref(), Some("opencode-go"));
        assert_eq!(r.model.as_deref(), Some("opencode-mini"));
    }

    #[test]
    fn resolve_channel_identity_falls_back_to_default_profile() {
        // Channel without identity fields: profile falls back to the default
        // profile name; provider falls back to the profile registry (absent
        // here) → None.
        let dir = temp_data_dir(None, Some("channels:\n  bare:\n    platform: cli\n"), None);
        let def = channel_def_from(&dir, "bare").expect("channel present");
        let r = resolve_channel_identity(&dir, &def);
        assert_eq!(r.profile, crate::profile::default_profile_name());
        // Provider falls back to the resolved profile's provider (the default
        // profile ships deepseek) - the loader returns resolved data, never
        // None while a default profile exists.
        let profile_name = crate::profile::default_profile_name();
        let expected = crate::profile::ProfileRegistry::new(&dir)
            .get(&profile_name)
            .and_then(|p| p.provider.clone());
        assert_eq!(r.provider, expected);
    }

    #[test]
    fn resolve_channel_identity_preserves_wf_test_pins() {
        // Regression guard 83f461b: the wf-test channel pins noop /
        // test-tool-caller - the resolver must preserve the channel override.
        let dir = temp_data_dir(
            None,
            Some(
                r#"
channels:
  wf-test:
    provider: noop
    model: test-tool-caller
"#,
            ),
            None,
        );
        let def = channel_def_from(&dir, "wf-test").expect("channel present");
        let r = resolve_channel_identity(&dir, &def);
        assert_eq!(r.provider.as_deref(), Some("noop"));
        assert_eq!(r.model.as_deref(), Some("test-tool-caller"));
    }

    #[test]
    fn resolve_task_defaults_board_task_gets_board_defaults() {
        // THE BUG: a board task has workflow_id/channel_id/profile NULL - the
        // resolver must supply them from the board (workflow + channel +
        // profile + plan).
        let dir = temp_data_dir(
            Some(
                "boards:\n  omnidev:\n    channel: kanban\n    profile: omni\n    workflow: omniagent-dev\n    plan: false\n",
            ),
            Some("channels:\n  kanban:\n    profile: omni\n"),
            None,
        );
        let resolved = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(resolved.workflow_id.as_deref(), Some("omniagent-dev"));
        assert_eq!(resolved.channel_id, "kanban");
        assert_eq!(resolved.profile, "omni");
        assert_eq!(resolved.plan, Some(false));
    }

    #[test]
    fn resolve_task_defaults_task_wins_over_board() {
        let dir = temp_data_dir(
            Some(
                "boards:\n  omnidev:\n    channel: kanban\n    profile: omni\n    workflow: omniagent-dev\n",
            ),
            None,
            None,
        );
        let resolved = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: Some("task-wf"),
                channel_id: Some("task-channel"),
                profile: Some("task-profile"),
                plan: Some(true),
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(resolved.workflow_id.as_deref(), Some("task-wf"));
        assert_eq!(resolved.channel_id, "task-channel");
        assert_eq!(resolved.profile, "task-profile");
        assert_eq!(resolved.plan, Some(true));
    }

    #[test]
    fn resolve_task_defaults_invalid_board_fails_loud() {
        let dir = temp_data_dir(
            Some("boards:\n  omnidev:\n    channel: kanban\n"),
            None,
            None,
        );
        // NULL board with boards enabled → explicit error.
        let err = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: None,
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .unwrap_err();
        assert_eq!(err, "task has no board");
        // Unknown board → explicit error.
        let err = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: Some("nope"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("not found in boards.yml"), "got: {err}");
    }

    #[test]
    fn resolve_task_defaults_channel_default_setting() {
        // Board gives no channel; the default_kanban_channel setting supplies
        // it (only when it names a known channel).
        let dir = temp_data_dir(
            Some("boards:\n  omnidev:\n    workflow: omniagent-dev\n"),
            Some("channels:\n  kanban:\n    profile: omni\n"),
            Some("kanban:\n  default_kanban_channel: kanban\n"),
        );
        let resolved = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(
            resolved.channel_id, "kanban",
            "default setting fills channel"
        );
        assert_eq!(resolved.profile, "omni", "channel profile fallback");
        // Unknown default → "" (fail-with-record at thread creation).
        let dir2 = temp_data_dir(
            Some("boards:\n  omnidev:\n    workflow: omniagent-dev\n"),
            Some("channels:\n  kanban:\n"),
            Some("kanban:\n  default_kanban_channel: missing-channel\n"),
        );
        let resolved2 = resolve_task_defaults(
            &dir2,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(resolved2.channel_id, "");
    }

    #[test]
    fn resolve_task_defaults_profile_chain() {
        // profile: task → board → channel → default_profile_name.
        let dir = temp_data_dir(
            Some("boards:\n  omnidev:\n    channel: kanban\n    workflow: wf\n"),
            Some("channels:\n  kanban:\n    profile: channel-profile\n"),
            None,
        );
        let resolved = resolve_task_defaults(
            &dir,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(resolved.profile, "channel-profile");

        // Board profile beats channel profile.
        let dir2 = temp_data_dir(
            Some(
                "boards:\n  omnidev:\n    channel: kanban\n    workflow: wf\n    profile: board-profile\n",
            ),
            Some("channels:\n  kanban:\n    profile: channel-profile\n"),
            None,
        );
        let resolved2 = resolve_task_defaults(
            &dir2,
            &TaskFallbackFields {
                board: Some("omnidev"),
                workflow_id: None,
                channel_id: None,
                profile: None,
                plan: None,
                template: None,
            },
        )
        .expect("valid board");
        assert_eq!(resolved2.profile, "board-profile");
    }

    #[test]
    fn effective_channel_name_chain() {
        let dir = temp_data_dir(
            None,
            Some("channels:\n  kanban:\n    profile: omni\n  cron:\n"),
            Some("kanban:\n  default_kanban_channel: kanban\n"),
        );
        // Explicit wins (even unknown - caller fails with channel-not-found).
        assert_eq!(
            effective_channel_name(&dir, Some("kanban"), "default_kanban_channel"),
            "kanban"
        );
        assert_eq!(
            effective_channel_name(&dir, Some("no-such"), "default_kanban_channel"),
            "no-such"
        );
        // No explicit → default setting (known channel).
        assert_eq!(
            effective_channel_name(&dir, None, "default_kanban_channel"),
            "kanban"
        );
        // Whitespace explicit falls through to the default.
        assert_eq!(
            effective_channel_name(&dir, Some("  "), "default_kanban_channel"),
            "kanban"
        );
        // Default naming a missing channel → "".
        let dir2 = temp_data_dir(
            None,
            Some("channels:\n  kanban:\n"),
            Some("kanban:\n  default_kanban_channel: missing\n"),
        );
        assert_eq!(
            effective_channel_name(&dir2, None, "default_kanban_channel"),
            ""
        );
    }

    #[test]
    fn resolve_channel_carries_field_fallbacks() {
        let dir = temp_data_dir(
            None,
            Some(
                "channels:\n  kanban:\n    profile: omni\n    provider: openai\n    model: gpt-4\n",
            ),
            None,
        );
        let ch = resolve_channel(&dir, Some("kanban"), "default_kanban_channel");
        assert_eq!(ch.name, "kanban");
        assert_eq!(ch.profile.as_deref(), Some("omni"));
        assert_eq!(ch.provider.as_deref(), Some("openai"));
        assert_eq!(ch.model.as_deref(), Some("gpt-4"));
        // Unknown channel: name kept, field fallbacks empty (fail-with-record).
        let ch2 = resolve_channel(&dir, Some("ghost"), "default_kanban_channel");
        assert_eq!(ch2.name, "ghost");
        assert_eq!(ch2.profile, None);
    }

    #[test]
    fn resolved_thread_provider_model_shapes() {
        // Phase 3 struct: the canonical resolution lives in
        // resolve_thread_identity (threads.rs); this asserts the projection
        // type carries provider+model.
        let ident = ResolvedThreadProviderModel {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
        };
        assert_eq!(ident.provider, "openai");
        assert_eq!(ident.model, "gpt-4");
        assert_ne!(ident.provider, ident.model);
    }

    #[test]
    fn settings_snapshot_equals_get_with_default() {
        // Phase 4: AgentConfig fields are resolved-at-load snapshots of
        // get(key, default). Assert the loader produces the defaults when
        // settings.yml is absent (the snapshot path, not a re-read).
        let dir = temp_data_dir(None, None, None);
        let settings = crate::server::settings::load_settings_file(&dir);
        // token budget defaults (soft 100000 / hard 500000) live in config.rs;
        // here we assert the settings map itself defaults to empty (AgentConfig
        // applies the defaults table at load - that is the snapshot).
        assert!(settings.is_empty(), "no settings.yml → empty snapshot");
    }
}
