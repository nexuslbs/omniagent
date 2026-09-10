//! Shared command handlers for `/model` and other channel commands.
//!
//! Provides a unified parsing + validation layer so that CLI, Telegram,
//! and external platform plugins all use the same logic.

use crate::err_msg;
use crate::error::{AppResult, Error};
use sqlx::PgPool;

use crate::db::types::Channel;
use crate::plugins_yaml;

// ---------------------------------------------------------------------------
// ModelCommand: parsed result
// ---------------------------------------------------------------------------

/// The parsed result of a `/model` command.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelAction {
    /// Show current provider/model status.
    Show,
    /// Set provider and/or model on the channel.
    Set {
        provider: Option<String>,
        model: Option<String>,
    },
    /// Reset (clear to NULL) provider and/or model.
    /// `true` means clear that field.
    Reset { provider: bool, model: bool },
}

/// Parsed `/model` command.
#[derive(Debug, Clone)]
pub struct ModelCommand {
    pub action: ModelAction,
}

/// Parse a `/model` command text from any platform.
///
/// Valid forms:
///   `//model`                        → Show
///   `//model <provider>`             → Set provider, keep model
///   `//model <provider> <model>`     → Set both
///   `//model reset`                  → Reset both
///   `//model reset provider`         → Reset provider only
///   `//model reset model`            → Reset model only
pub fn parse_model_command(input: &str) -> AppResult<ModelCommand> {
    let trimmed = input.trim();
    let rest = trimmed
        .strip_prefix("//model")
        .or_else(|| trimmed.strip_prefix("/model"))
        .or_else(|| trimmed.strip_prefix("$model"))
        .unwrap_or(trimmed)
        .trim();

    if rest.is_empty() {
        return Ok(ModelCommand {
            action: ModelAction::Show,
        });
    }

    if rest == "reset" {
        return Ok(ModelCommand {
            action: ModelAction::Reset {
                provider: true,
                model: true,
            },
        });
    }

    if let Some(target) = rest.strip_prefix("reset ") {
        let target = target.trim();
        match target {
            "provider" => {
                return Ok(ModelCommand {
                    action: ModelAction::Reset {
                        provider: true,
                        model: false,
                    },
                });
            }
            "model" => {
                return Ok(ModelCommand {
                    action: ModelAction::Reset {
                        provider: false,
                        model: true,
                    },
                });
            }
            _ => {
                err_msg!(
                    "Unknown reset target '{}'. Use: $model reset, $model reset provider, $model reset model",
                    target
                );
            }
        }
    }

    // `$model <provider>` or `$model <provider> <model>`
    let parts: Vec<&str> = rest.split_whitespace().collect();
    match parts.len() {
        1 => Ok(ModelCommand {
            action: ModelAction::Set {
                provider: Some(parts[0].to_string()),
                model: None,
            },
        }),
        2 => Ok(ModelCommand {
            action: ModelAction::Set {
                provider: Some(parts[0].to_string()),
                model: Some(parts[1].to_string()),
            },
        }),
        _ => {
            err_msg!("Usage: //model [provider] [model] | //model reset [provider|model]");
        }
    }
}

/// Validate that a provider name exists and is enabled in the providers YAML file.
/// Returns Ok(()) if valid, Err with a message if not found.
pub fn validate_provider(data_dir: &str, provider_name: &str) -> AppResult<()> {
    let provider_enabled = plugins_yaml::provider_exists_and_enabled(data_dir, provider_name)
        .map_err(|e| Error::Message(format!("Failed to check provider: {}", e)))?;

    if provider_enabled {
        Ok(())
    } else {
        err_msg!(
            "Unknown provider '{}'. Register it as a provider plugin first.",
            provider_name
        )
    }
}

/// Format a status line showing the current provider/model for a channel.
pub fn format_model_status(provider: Option<&str>, model: Option<&str>) -> String {
    let provider_str =
        provider.unwrap_or("(not set: will use profile default or LLM_PROVIDER env var)");
    let model_str =
        model.unwrap_or("(not set: will use profile default or provider plugin default_model)");
    format!(
        "Current channel configuration:\n  Provider: {}\n  Model:    {}",
        provider_str, model_str
    )
}

// ---------------------------------------------------------------------------
// NewCommand: parsed result for `/new`
// ---------------------------------------------------------------------------

/// Parsed `/new` command. Valid forms: `<prefix> [name]`, where `<prefix>` is
/// one of the command prefixes the platform plugin DECLARED (see
/// [`match_new_command`]).
#[derive(Debug, Clone)]
pub struct NewCommand {
    /// Optional channel name: `<prefix> <name>` creates/updates a channel keyed
    /// exactly `<name>`; absent -> the caller derives `{platform}-{first8}`.
    pub name: Option<String>,
}

/// Generic fallback prefixes for the `new` command, used when a platform
/// plugin declares no `commands.new` capability (today's non-Mattermost
/// behaviour: the Telegram Bot API style `/new`).
pub const DEFAULT_NEW_COMMAND_PREFIXES: &[&str] = &["/new"];

/// True when `text` is exactly `prefix`, or starts with `prefix` followed by
/// whitespace. Token-exact, so `/newsletter` is NOT `/new`.
fn matches_command_token(text: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    match text.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

/// Match `text` against the command `prefixes` DECLARED by a platform plugin
/// (capability `commands.<name>`). Returns the longest matching prefix, or
/// `None` when the text is not that command. Core only matches what the plugin
/// advertised: it never decides which platform owns which prefix.
pub fn match_command_prefix<'p>(
    text: &str,
    prefixes: impl IntoIterator<Item = &'p str>,
) -> Option<&'p str> {
    let trimmed = text.trim_start();
    prefixes
        .into_iter()
        .filter(|prefix| matches_command_token(trimmed, prefix))
        .max_by_key(|prefix| prefix.len())
}

/// The `new`-command prefix matched by `text` on a platform that declared
/// `declared` prefixes (`capabilities.commands.new`), falling back to
/// [`DEFAULT_NEW_COMMAND_PREFIXES`] when the plugin declared none.
pub fn match_new_command<'p>(text: &str, declared: Option<&'p [String]>) -> Option<&'p str> {
    match declared {
        Some(prefixes) if !prefixes.is_empty() => {
            match_command_prefix(text, prefixes.iter().map(String::as_str))
        }
        _ => match_command_prefix(text, DEFAULT_NEW_COMMAND_PREFIXES.iter().copied()),
    }
}

/// Parse a `/new` command text that already matched `prefix` (a prefix
/// declared by the platform plugin; see [`match_new_command`]). The optional
/// first argument is the channel name (`/new`, `/new mm-kanban`,
/// `$new mm-kanban`).
pub fn parse_new_command(input: &str, prefix: &str) -> AppResult<NewCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix(prefix).unwrap_or(trimmed).trim();
    let name = if rest.is_empty() {
        None
    } else {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() > 1 {
            err_msg!("Usage: /new [name]");
        }
        Some(rest.to_string())
    };
    Ok(NewCommand { name })
}

// ---------------------------------------------------------------------------
// ChannelCommand: parsed result for `/channel`
// ---------------------------------------------------------------------------

/// Parsed `/channel` command.
#[derive(Debug, Clone)]
pub enum ChannelCommand {
    /// Show current channel info.
    Show,
    /// List all available channels.
    List,
    /// Switch to a named channel.
    Switch(String),
}

/// Parse a `/channel` command text.
///
/// Valid forms:
///   `/channel`        → Show
///   `/channel list`   → List
///   `/channel <name>` → Switch
pub fn parse_channel_command(input: &str) -> AppResult<ChannelCommand> {
    let trimmed = input.trim();
    let rest = trimmed
        .strip_prefix("//channel")
        .or_else(|| trimmed.strip_prefix("/channel"))
        .or_else(|| trimmed.strip_prefix("$channel"))
        .unwrap_or(trimmed)
        .trim();
    if rest.is_empty() {
        return Ok(ChannelCommand::Show);
    }
    if rest == "list" {
        return Ok(ChannelCommand::List);
    }
    // Treat as a channel name
    Ok(ChannelCommand::Switch(rest.to_string()))
}

// ---------------------------------------------------------------------------
// ProfileCommand: parsed result for `/profile`
// ---------------------------------------------------------------------------

/// Parsed `/profile` command.
#[derive(Debug, Clone)]
pub enum ProfileCommand {
    /// Show current profile info.
    Show,
    /// Set the profile to a named one.
    Set(String),
    /// Reset profile to default.
    Reset,
}

/// Parse a `/profile` command text.
///
/// Valid forms:
///   `/profile`           → Show
///   `/profile <name>`    → Set
///   `/profile reset`     → Reset
pub fn parse_profile_command(input: &str) -> AppResult<ProfileCommand> {
    let trimmed = input.trim();
    let rest = trimmed
        .strip_prefix("//profile")
        .or_else(|| trimmed.strip_prefix("/profile"))
        .or_else(|| trimmed.strip_prefix("$profile"))
        .unwrap_or(trimmed)
        .trim();
    if rest.is_empty() {
        return Ok(ProfileCommand::Show);
    }
    if rest == "reset" {
        return Ok(ProfileCommand::Reset);
    }
    // Treat as profile name
    Ok(ProfileCommand::Set(rest.to_string()))
}

// ---------------------------------------------------------------------------
// Shared async handlers
// ---------------------------------------------------------------------------

/// Execute `/new` for an external platform: creates a channel with
/// resource_identifier = external_channel_id / platform resource identifier.
/// When `name` is provided (non-empty) it is used VERBATIM as the channel
/// key/name (e.g. `/new mm-kanban` -> key `mm-kanban`); otherwise the name
/// is derived as `{platform}-{first8}` (backwards compat for a bare command).
pub async fn handle_new_external(
    pool: &PgPool,
    platform: &str,
    resource_identifier: &str,
    name: Option<&str>,
) -> AppResult<Channel> {
    // Use the explicit name when given; otherwise derive from platform+resource
    let name = match name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => format!(
            "{}-{}",
            platform,
            resource_identifier.chars().take(8).collect::<String>()
        ),
    };
    // Create channel (ON CONFLICT will update updated_at but return existing)
    let channel = crate::db::types::create_channel(
        pool,
        crate::db::types::CreateChannelParams {
            name,
            platform: platform.to_string(),
            external_id: resource_identifier.to_string(),
            resource_identifier: resource_identifier.to_string(),
        },
    )
    .await?;
    Ok(channel)
}

/// Set the profile on a channel (yml `profile` field; channel_id = channel
/// name, the channels.yml key).
pub async fn handle_profile_set(
    _pool: &PgPool,
    channel_id: String,
    profile_name: &str,
) -> AppResult<()> {
    crate::channels_yaml::update_channel(&channel_id, |existing| {
        let mut d = existing
            .cloned()
            .ok_or_else(|| Error::Message(format!("Channel '{}' not found", channel_id)))?;
        d.profile = Some(profile_name.to_string());
        Ok(d)
    })?;
    Ok(())
}

/// List channels by platform (channels live in channels.yml now).
pub async fn handle_channel_list(pool: &PgPool, platform: &str) -> AppResult<Vec<Channel>> {
    let all = crate::db::channels::find_all_channels(pool).await?;
    Ok(all
        .into_iter()
        .filter(|c| {
            c.platform
                .as_deref()
                .map(|p| p == platform)
                .unwrap_or(false)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── /model tests ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_show() {
        let cmd = parse_model_command("//model").unwrap();
        assert_eq!(cmd.action, ModelAction::Show);

        let cmd = parse_model_command("  //model  ").unwrap();
        assert_eq!(cmd.action, ModelAction::Show);
    }

    #[test]
    fn test_parse_set_provider_only() {
        let cmd = parse_model_command("//model opencode-go").unwrap();
        assert_eq!(
            cmd.action,
            ModelAction::Set {
                provider: Some("opencode-go".into()),
                model: None,
            }
        );
    }

    #[test]
    fn test_parse_set_both() {
        let cmd = parse_model_command("//model opencode-go deepseek-v4-flash").unwrap();
        assert_eq!(
            cmd.action,
            ModelAction::Set {
                provider: Some("opencode-go".into()),
                model: Some("deepseek-v4-flash".into()),
            }
        );
    }

    #[test]
    fn test_parse_reset_both() {
        let cmd = parse_model_command("//model reset").unwrap();
        assert_eq!(
            cmd.action,
            ModelAction::Reset {
                provider: true,
                model: true,
            }
        );
    }

    #[test]
    fn test_parse_reset_provider() {
        let cmd = parse_model_command("//model reset provider").unwrap();
        assert_eq!(
            cmd.action,
            ModelAction::Reset {
                provider: true,
                model: false,
            }
        );
    }

    #[test]
    fn test_parse_reset_model() {
        let cmd = parse_model_command("//model reset model").unwrap();
        assert_eq!(
            cmd.action,
            ModelAction::Reset {
                provider: false,
                model: true,
            }
        );
    }

    #[test]
    fn test_parse_too_many_args() {
        let cmd = parse_model_command("//model a b c");
        assert!(cmd.is_err());
    }

    #[test]
    fn test_parse_bad_reset_target() {
        let cmd = parse_model_command("//model reset foo");
        assert!(cmd.is_err());
    }

    // ── /new tests ────────────────────────────────────────────────────────

    #[test]
    fn test_parse_new() {
        let cmd = parse_new_command("//new", "//new").unwrap();
        assert!(cmd.name.is_none());
    }

    #[test]
    fn test_parse_new_with_name() {
        let cmd = parse_new_command("//new mm-kanban", "//new").unwrap();
        assert_eq!(cmd.name.as_deref(), Some("mm-kanban"));

        let cmd = parse_new_command("$new mm-kanban", "$new").unwrap();
        assert_eq!(cmd.name.as_deref(), Some("mm-kanban"));

        let cmd = parse_new_command("  $new  mm-kanban  ", "$new").unwrap();
        assert_eq!(cmd.name.as_deref(), Some("mm-kanban"));
    }

    #[test]
    fn test_parse_new_too_many_args() {
        let cmd = parse_new_command("//new foo bar", "//new");
        assert!(cmd.is_err());
    }

    #[test]
    fn test_parse_new_whitespace() {
        let cmd = parse_new_command("  //new  ", "//new").unwrap();
        assert!(cmd.name.is_none());
    }

    /// A plugin that declares nothing gets the generic `/new` fallback.
    #[test]
    fn test_match_new_command_default_fallback() {
        assert_eq!(match_new_command("/new", None), Some("/new"));
        assert_eq!(match_new_command("/new telegram", None), Some("/new"));
        assert_eq!(match_new_command("  /new telegram", None), Some("/new"));
        // `$new` is only a command for a plugin that DECLARES it.
        assert_eq!(match_new_command("$new", None), None);
        assert_eq!(match_new_command("$new telegram", None), None);
        assert_eq!(match_new_command("/newsletter", None), None);
        assert_eq!(match_new_command("", None), None);
        // An empty declaration is the same as no declaration.
        let empty: Vec<String> = vec![];
        assert_eq!(match_new_command("/new", Some(&empty)), Some("/new"));
    }

    /// Mattermost declares its historical prefixes; telegram declares `/new`.
    #[test]
    fn test_match_new_command_declared_prefixes() {
        let mattermost: Vec<String> = ["/new", "$new", "//new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(match_new_command("$new", Some(&mattermost)), Some("$new"));
        assert_eq!(
            match_new_command("$new mm-kanban", Some(&mattermost)),
            Some("$new")
        );
        assert_eq!(
            match_new_command("  //new foo", Some(&mattermost)),
            Some("//new")
        );
        assert_eq!(match_new_command("/new", Some(&mattermost)), Some("/new"));

        let telegram: Vec<String> = vec!["/new".to_string()];
        assert_eq!(match_new_command("/new x", Some(&telegram)), Some("/new"));
        // `$new x` on a plugin declaring only `/new` is NOT a command.
        assert_eq!(match_new_command("$new x", Some(&telegram)), None);
    }

    /// A third, fake platform declares its own syntax; core honours it verbatim.
    #[test]
    fn test_match_new_command_third_fake_platform() {
        let fake: Vec<String> = vec!["!new".to_string()];
        assert_eq!(match_new_command("!new chat", Some(&fake)), Some("!new"));
        assert_eq!(match_new_command("!new", Some(&fake)), Some("!new"));
        // The declared set is the ONLY accepted syntax for that platform.
        assert_eq!(match_new_command("/new chat", Some(&fake)), None);
        assert_eq!(match_new_command("$new chat", Some(&fake)), None);
        // Token-exact: `!newsletter` is not `!new`.
        assert_eq!(match_new_command("!newsletter", Some(&fake)), None);
    }

    // ── /channel tests ───────────────────────────────────────────────────

    #[test]
    fn test_parse_channel_show() {
        let cmd = parse_channel_command("//channel").unwrap();
        assert!(matches!(cmd, ChannelCommand::Show));
    }

    #[test]
    fn test_parse_channel_list() {
        let cmd = parse_channel_command("//channel list").unwrap();
        assert!(matches!(cmd, ChannelCommand::List));
    }

    #[test]
    fn test_parse_channel_switch() {
        let cmd = parse_channel_command("//channel my-channel").unwrap();
        match cmd {
            ChannelCommand::Switch(name) => assert_eq!(name, "my-channel"),
            _ => panic!("Expected Switch"),
        }
    }

    // ── /profile tests ───────────────────────────────────────────────────

    #[test]
    fn test_parse_profile_show() {
        let cmd = parse_profile_command("//profile").unwrap();
        assert!(matches!(cmd, ProfileCommand::Show));
    }

    #[test]
    fn test_parse_profile_set() {
        let cmd = parse_profile_command("//profile default").unwrap();
        match cmd {
            ProfileCommand::Set(name) => assert_eq!(name, "default"),
            _ => panic!("Expected Set"),
        }
    }

    #[test]
    fn test_parse_profile_reset() {
        let cmd = parse_profile_command("//profile reset").unwrap();
        assert!(matches!(cmd, ProfileCommand::Reset));
    }
}
