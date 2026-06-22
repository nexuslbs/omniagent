//! Shared command handlers for `/model` and other channel commands.
//!
//! Provides a unified parsing + validation layer so that CLI, Telegram,
//! and external platform plugins all use the same logic.

use anyhow::Result;
use sqlx::PgPool;

use crate::plugin;

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
    Reset {
        provider: bool,
        model: bool,
    },
}

/// Parsed `/model` command.
#[derive(Debug, Clone)]
pub struct ModelCommand {
    pub action: ModelAction,
}

/// Parse a `/model` command text from any platform.
///
/// Valid forms:
///   `/model`                        → Show
///   `/model <provider>`             → Set provider, keep model
///   `/model <provider> <model>`     → Set both
///   `/model reset`                  → Reset both
///   `/model reset provider`         → Reset provider only
///   `/model reset model`            → Reset model only
pub fn parse_model_command(input: &str) -> Result<ModelCommand> {
    let input = input.trim();
    let rest = input.strip_prefix("/model").unwrap_or(input).trim();

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
                anyhow::bail!(
                    "Unknown reset target '{}'. Use: /model reset, /model reset provider, /model reset model",
                    target
                );
            }
        }
    }

    // `/model <provider>` or `/model <provider> <model>`
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
            anyhow::bail!(
                "Usage: /model [provider] [model] | /model reset [provider|model]"
            );
        }
    }
}

/// Validate that a provider name exists in the plugin_registry with plugin_type='provider'.
/// Returns Ok(()) if valid, Err with a message if not found.
pub async fn validate_provider(pool: &PgPool, provider_name: &str) -> Result<()> {
    let row = plugin::get_plugin_by_name(pool, provider_name).await?;
    match row {
        Some(r) if r.plugin_type == "provider" => Ok(()),
        Some(r) => anyhow::bail!(
            "'{}' exists but is not a provider plugin (type={})",
            provider_name,
            r.plugin_type
        ),
        None => anyhow::bail!(
            "Unknown provider '{}'. Register it as a provider plugin first.",
            provider_name
        ),
    }
}

/// Format a status line showing the current provider/model for a channel.
pub fn format_model_status(
    provider: Option<&str>,
    model: Option<&str>,
) -> String {
    let provider_str = provider.unwrap_or("(not set — will use profile default or LLM_PROVIDER env var)");
    let model_str = model.unwrap_or("(not set — will use profile default or LLM_MODEL env var)");
    format!(
        "Current channel configuration:\n  Provider: {}\n  Model:    {}",
        provider_str, model_str
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_show() {
        let cmd = parse_model_command("/model").unwrap();
        assert_eq!(cmd.action, ModelAction::Show);

        let cmd = parse_model_command("  /model  ").unwrap();
        assert_eq!(cmd.action, ModelAction::Show);
    }

    #[test]
    fn test_parse_set_provider_only() {
        let cmd = parse_model_command("/model opencode-go").unwrap();
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
        let cmd = parse_model_command("/model opencode-go deepseek-v4-flash").unwrap();
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
        let cmd = parse_model_command("/model reset").unwrap();
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
        let cmd = parse_model_command("/model reset provider").unwrap();
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
        let cmd = parse_model_command("/model reset model").unwrap();
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
        let cmd = parse_model_command("/model a b c");
        assert!(cmd.is_err());
    }

    #[test]
    fn test_parse_bad_reset_target() {
        let cmd = parse_model_command("/model reset foo");
        assert!(cmd.is_err());
    }
}
