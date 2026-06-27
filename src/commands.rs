//! Shared command handlers for `/model` and other channel commands.
//!
//! Provides a unified parsing + validation layer so that CLI, Telegram,
//! and external platform plugins all use the same logic.

use crate::error::{AppResult, Error};
use crate::err_msg;
use sql_forge::sql_forge;
use sqlx::PgPool;

use crate::db::types::Channel;
use crate::plugins_yaml;

// ---------------------------------------------------------------------------
// ModelCommand — parsed result
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
        provider.unwrap_or("(not set — will use profile default or LLM_PROVIDER env var)");
    let model_str =
        model.unwrap_or("(not set — will use profile default or provider plugin default_model)");
    format!(
        "Current channel configuration:\n  Provider: {}\n  Model:    {}",
        provider_str, model_str
    )
}

// ---------------------------------------------------------------------------
// NewCommand — parsed result for `/new`
// ---------------------------------------------------------------------------

/// Parsed `/new` command (no arguments).
#[derive(Debug, Clone)]
pub struct NewCommand;

#[allow(dead_code)]
/// Parse a `/new` command text. Valid form: `/new` (no arguments).
pub fn parse_new_command(input: &str) -> AppResult<NewCommand> {
    let trimmed = input.trim();
    let rest = trimmed
        .strip_prefix("//new")
        .or_else(|| trimmed.strip_prefix("/new"))
        .or_else(|| trimmed.strip_prefix("$new"))
        .unwrap_or(trimmed)
        .trim();
    if !rest.is_empty() {
        err_msg!("Usage: /new (no arguments)");
    }
    Ok(NewCommand)
}

// ---------------------------------------------------------------------------
// ChannelCommand — parsed result for `/channel`
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
// ProfileCommand — parsed result for `/profile`
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
pub async fn handle_new_external(
    pool: &PgPool,
    platform: &str,
    resource_identifier: &str,
) -> AppResult<Channel> {
    // Generate a name based on platform and resource
    let name = format!(
        "{}-{}",
        platform,
        resource_identifier.chars().take(8).collect::<String>()
    );
    // Create channel (ON CONFLICT will update updated_at but return existing)
    let channel = crate::db::types::create_channel(
        pool,
        crate::db::types::CreateChannelParams {
            name,
            platform: platform.to_string(),
            external_id: resource_identifier.to_string(),
            cause: "user".to_string(),
            resource_identifier: resource_identifier.to_string(),
        },
    )
    .await?;
    Ok(channel)
}

/// Set the profile on a channel by updating `current_profile`.
pub async fn handle_profile_set(pool: &PgPool, channel_id: i64, profile_name: &str) -> AppResult<()> {
    sql_forge!(
        "UPDATE channels SET current_profile = :profile_name, updated_at = NOW() WHERE id = :channel_id",
        ( :profile_name = profile_name, :channel_id = channel_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// List channels by platform.
pub async fn handle_channel_list(pool: &PgPool, platform: &str) -> AppResult<Vec<Channel>> {
    let rows: Vec<crate::db::types::ChannelDb> = sql_forge!(
        crate::db::types::ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(template, '') AS "template",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE platform = :platform
        ORDER BY name ASC
        "#,
        ( :platform = platform )
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| r.try_into().map_err(|e| Error::Message(format!("Channel conversion failed: {}", e))))
        .collect()
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
        let cmd = parse_new_command("//new").unwrap();
        assert!(matches!(cmd, NewCommand));
    }

    #[test]
    fn test_parse_new_with_args() {
        let cmd = parse_new_command("//new foo");
        assert!(cmd.is_err());
    }

    #[test]
    fn test_parse_new_whitespace() {
        let cmd = parse_new_command("  //new  ").unwrap();
        assert!(matches!(cmd, NewCommand));
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
