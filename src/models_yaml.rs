//! Provider/model overrides via a pure definition file: `{data_dir}/config/models.yml`.
//!
//! This file lets an operator override provider definitions and per-model settings
//! WITHOUT writing a plugin or any custom code. Semantics (user spec, 2026-08-19):
//!
//! ```yaml
//! providers:
//!   deepseek:
//!     plugin: true                      # true/name -> use the provider plugin (plugins.yml)
//!     models: ["deepseek-v4-flash", "deepseek-v4-pro"]  # replaces default_model.allowed_values
//!   my_provider_01:
//!     plugin: false                     # no plugin: builtin chat_completions/anthropic support
//!     api_mode: "chat_completions"
//!     supports_reasoning: true
//!     default_base_url: "http://noop-provider:9090/v1"
//!     refresh_url: "https://api.deepseek.com/v1/models"
//!     default_model: "test-model-1"
//!     api_key: "$secret:MY_SECRET"      # $env: and $secret: refs supported
//!     models: ["my_model_01", "my_model_02", "my_model_03"]
//!     model_config:
//!       my_model_02:
//!         api_mode: "anthropic"
//!         supports_reasoning: false
//!         token_budget_soft: 200000
//!         token_budget_hard: 1000000
//!         max_tokens: 32000
//!         max_tokens_on_truncation: 128000
//! ```
//!
//! Precedence (per user spec): for EACH of soft/hard token budget INDEPENDENTLY —
//!   1. `model_config.<model>` budget;
//!   2. else provider-level budget (`providers.<name>` soft/hard);
//!   3. else GLOBAL settings budget (`prompt_token_budget_soft` default 100000 /
//!      `prompt_token_budget_hard` default 500000 from settings.yml).
//!
//! `max_tokens` / `max_tokens_on_truncation` follow the same chain (model > provider > settings).
//!
//! omniagent RESOLVES the effective values and passes them to the prompt plugin's
//! compact-messages tool as `soft_budget`/`hard_budget` params. The prompt plugin
//! stays AGNOSTIC of models.yml — it only ever sees resolved budgets as params.
//!
//! Absent/empty models.yml -> zero behavior change. Malformed -> load errors so
//! startup can fail loud with a clear message.

use crate::error::{AppResult, Error, ErrorContext};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Canonical path to models.yml: `{data_dir}/config/models.yml`.
pub fn models_path(data_dir: &str) -> PathBuf {
    crate::config_path::config_path(data_dir, "models.yml")
}

/// Top-level models.yml content.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderOverride>,
}

/// `plugin:` flag: `true`/a provider-plugin name -> use the plugin (plugins.yml);
/// `false` -> builtin chat_completions/anthropic support only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PluginFlag {
    Bool(bool),
    Name(String),
}

impl Default for PluginFlag {
    fn default() -> Self {
        PluginFlag::Bool(true)
    }
}

impl PluginFlag {
    /// True when the entry is plugin-backed (explicit `true` or a plugin name).
    pub fn is_true(&self) -> bool {
        match self {
            PluginFlag::Bool(b) => *b,
            PluginFlag::Name(_) => true,
        }
    }

    /// Optional explicit plugin name (when `plugin: <name>`).
    pub fn plugin_name(&self) -> Option<&str> {
        match self {
            PluginFlag::Name(n) => Some(n),
            _ => None,
        }
    }
}

/// Skip serializing the default `plugin: true` (keeps files concise; `false`/name
/// are always written).
fn is_default_plugin(flag: &PluginFlag) -> bool {
    matches!(flag, PluginFlag::Bool(true))
}

/// A single provider override/definition in models.yml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOverride {
    /// `true`/name -> use the provider plugin (plugins.yml); `false` -> builtin support.
    /// Same name as an existing provider plugin OVERRIDES it (plugin still used
    /// behind the scenes for transport; selectors + defs come from models.yml).
    #[serde(default, skip_serializing_if = "is_default_plugin")]
    pub plugin: PluginFlag,
    /// Replaces `default_model.allowed_values` in selectors (the models list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// Overrides the plugin manifest api_mode ("chat_completions" | "anthropic_messages").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<String>,
    /// Overrides the plugin manifest supports_reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Overrides the plugin manifest default_base_url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_base_url: Option<String>,
    /// Models-list refresh endpoint used by the dashboard refresh button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Overrides the plugin manifest default_model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// API key for this provider. Supports `$env:VAR` and `$secret:NAME` refs
    /// (same expansion path as plugins.yml api_key). Overrides the plugin config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Provider-level token budget (soft). Precedence: model_config > provider > settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget_soft: Option<usize>,
    /// Provider-level token budget (hard). Precedence: model_config > provider > settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget_hard: Option<usize>,
    /// Provider-level max output tokens. Precedence: model_config > provider > settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Provider-level escalated output budget on truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_on_truncation: Option<u32>,
    /// Per-model overrides (highest precedence for every resolved value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_config: Option<BTreeMap<String, ModelConfig>>,
}

/// Per-model settings in models.yml (`model_config.<model>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Per-model API mode override ("chat_completions" | "anthropic_messages").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_mode: Option<String>,
    /// Per-model reasoning support override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// Per-model soft token budget (reduction target passed to compact-messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget_soft: Option<usize>,
    /// Per-model hard token budget (compaction trigger threshold).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget_hard: Option<usize>,
    /// Per-model max output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Per-model escalated output budget on truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_on_truncation: Option<u32>,
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

/// Load models.yml. Absent/empty -> `Ok(ModelsFile::default())`.
/// Malformed -> `Err` with a clear message (callers may fail startup).
pub fn load_models_file(data_dir: &str) -> AppResult<ModelsFile> {
    let path = models_path(data_dir);
    if !path.exists() {
        return Ok(ModelsFile::default());
    }
    let content =
        std::fs::read_to_string(&path).ctx(format!("Failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(ModelsFile::default());
    }
    let file: ModelsFile = serde_yaml::from_str(&content).ctx(format!(
        "Failed to parse {} (provider/model override definitions): {}",
        path.display(),
        content.lines().next().unwrap_or("")
    ))?;
    // Light semantic validation: provider names + model entries must be non-empty.
    for (name, ov) in &file.providers {
        if name.trim().is_empty() {
            return Err(Error::Message(format!(
                "Invalid {}: provider name must not be empty",
                path.display()
            )));
        }
        if let Some(models) = &ov.models {
            for m in models {
                if m.trim().is_empty() {
                    return Err(Error::Message(format!(
                        "Invalid {}: provider '{}' has an empty model entry",
                        path.display(),
                        name
                    )));
                }
            }
        }
        if let Some(mc) = &ov.model_config {
            for (model, cfg) in mc {
                if model.trim().is_empty() {
                    return Err(Error::Message(format!(
                        "Invalid {}: provider '{}' has an empty model_config key",
                        path.display(),
                        name
                    )));
                }
                validate_budget(name, model, "token_budget_soft", cfg.token_budget_soft)?;
                validate_budget(name, model, "token_budget_hard", cfg.token_budget_hard)?;
            }
        }
        validate_budget(name, name, "token_budget_soft", ov.token_budget_soft)?;
        validate_budget(name, name, "token_budget_hard", ov.token_budget_hard)?;
    }
    Ok(file)
}

fn validate_budget(
    provider: &str,
    model: &str,
    field: &str,
    value: Option<usize>,
) -> AppResult<()> {
    if let Some(v) = value {
        if v == 0 {
            return Err(Error::Message(format!(
                "Invalid models.yml: provider '{}' model '{}' {} must be > 0 (got {})",
                provider, model, field, v
            )));
        }
    }
    Ok(())
}

/// Atomic save of models.yml (.tmp -> fsync -> rename). Creates config/ if needed.
pub fn save_models_file(data_dir: &str, file: &ModelsFile) -> AppResult<()> {
    crate::config_path::ensure_config_dir(data_dir);
    let path = models_path(data_dir);
    let tmp_path = path.with_extension("yml.tmp");
    let yaml = serde_yaml::to_string(file).ctx("Failed to serialize models.yml")?;
    {
        let mut f = std::fs::File::create(&tmp_path)
            .ctx(format!("Failed to create {}", tmp_path.display()))?;
        std::io::Write::write_all(&mut f, yaml.as_bytes())
            .ctx(format!("Failed to write {}", tmp_path.display()))?;
        f.sync_all()
            .ctx(format!("Failed to sync {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, &path).ctx(format!(
        "Failed to rename {} to {}",
        tmp_path.display(),
        path.display()
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

/// The `models` list for a provider defined in models.yml (None when absent).
pub fn models_for_provider(data_dir: &str, provider: &str) -> Option<Vec<String>> {
    load_models_file(data_dir)
        .ok()?
        .providers
        .get(provider)?
        .models
        .clone()
}

/// Whether the provider is defined in models.yml as plugin-less (`plugin: false`).
pub fn is_plugin_less(data_dir: &str, provider: &str) -> bool {
    load_models_file(data_dir)
        .ok()
        .and_then(|f| f.providers.get(provider).cloned())
        .map(|ov| !ov.plugin.is_true())
        .unwrap_or(false)
}

/// Refresh URL for a provider: models.yml `refresh_url` first, else the plugin
/// manifest's `default_model` config_schema refresh_url (fallback, None allowed).
pub fn models_refresh_url(data_dir: &str, provider: &str) -> Option<String> {
    load_models_file(data_dir)
        .ok()?
        .providers
        .get(provider)?
        .refresh_url
        .clone()
}

/// Raw (unresolved) api_key from models.yml for a provider.
pub fn models_api_key_raw(data_dir: &str, provider: &str) -> Option<String> {
    load_models_file(data_dir)
        .ok()?
        .providers
        .get(provider)?
        .api_key
        .clone()
}

/// Resolve a provider's models.yml api_key (`$env:`/`$secret:` expansion).
pub async fn resolve_models_api_key(
    data_dir: &str,
    provider: &str,
    pool: &sqlx::PgPool,
) -> Option<String> {
    let raw = models_api_key_raw(data_dir, provider)?;
    Some(crate::plugins_yaml::resolve_config_ref_value(&raw, pool).await)
}

/// Upsert the `models` list for a provider in models.yml (refresh-flow contract):
/// - entry ABSENT  -> create with `plugin: true` + `models: [fetched]`;
/// - entry PRESENT -> update ONLY `models`, every other field byte-identical.
///
/// Returns the saved file.
pub fn upsert_provider_models(
    data_dir: &str,
    provider: &str,
    models: Vec<String>,
) -> AppResult<ModelsFile> {
    let mut file = load_models_file(data_dir).unwrap_or_default();
    let entry = file
        .providers
        .entry(provider.to_string())
        .or_insert_with(|| ProviderOverride {
            plugin: PluginFlag::Bool(true),
            ..Default::default()
        });
    entry.models = Some(models);
    save_models_file(data_dir, &file)?;
    Ok(file)
}

// ---------------------------------------------------------------------------
// Precedence resolution (model > provider > plugin/core defaults)
// ---------------------------------------------------------------------------

/// Global (settings.yml / AgentConfig) fallback values for the resolution chain.
#[derive(Debug, Clone)]
pub struct ModelGlobalDefaults {
    pub token_budget_soft: usize,
    pub token_budget_hard: usize,
    pub max_tokens: Option<u32>,
    pub max_tokens_on_truncation: Option<u32>,
}

/// The fully-resolved per-thread (provider+model) effective configuration.
#[derive(Debug, Clone)]
pub struct EffectiveModelConfig {
    pub api_mode: String,
    pub supports_reasoning: bool,
    pub token_budget_soft: usize,
    pub token_budget_hard: usize,
    pub max_tokens: Option<u32>,
    pub max_tokens_on_truncation: Option<u32>,
}

/// Exact per-model api_mode override from models.yml (model_config), if any.
pub fn resolve_model_api_mode(data_dir: &str, provider: &str, model: &str) -> Option<String> {
    load_models_file(data_dir)
        .ok()?
        .providers
        .get(provider)?
        .model_config
        .as_ref()?
        .get(model)?
        .api_mode
        .clone()
}

/// Resolve the effective per-thread config for (provider, model).
///
/// Precedence for EACH value INDEPENDENTLY (user spec 2026-08-19):
///   1. model_config.<model>  (models.yml)
///   2. providers.<name>      (models.yml provider-level)
///   3. plugin manifest / GLOBAL settings (defaults)
pub fn resolve_effective(
    data_dir: &str,
    provider: &str,
    model: &str,
    defaults: &ModelGlobalDefaults,
) -> EffectiveModelConfig {
    let file = load_models_file(data_dir).unwrap_or_default();
    let ov = file.providers.get(provider);
    let mc = ov
        .and_then(|p| p.model_config.as_ref())
        .and_then(|m| m.get(model));

    // Plugin-level fallbacks come from the merged metadata (PROVIDER_METADATA
    // already includes models.yml provider-level overrides).
    let meta_api_mode = crate::llm::resolve_provider_api_mode(provider);
    let meta_supports_reasoning = crate::llm::PROVIDER_METADATA
        .read()
        .get(provider)
        .map(|m| m.supports_reasoning)
        .unwrap_or(false);

    EffectiveModelConfig {
        api_mode: mc
            .and_then(|c| c.api_mode.clone())
            .or_else(|| ov.and_then(|p| p.api_mode.clone()))
            .unwrap_or(meta_api_mode),
        supports_reasoning: mc
            .and_then(|c| c.supports_reasoning)
            .or(ov.and_then(|p| p.supports_reasoning))
            .unwrap_or(meta_supports_reasoning),
        token_budget_soft: mc
            .and_then(|c| c.token_budget_soft)
            .or(ov.and_then(|p| p.token_budget_soft))
            .unwrap_or(defaults.token_budget_soft),
        token_budget_hard: mc
            .and_then(|c| c.token_budget_hard)
            .or(ov.and_then(|p| p.token_budget_hard))
            .unwrap_or(defaults.token_budget_hard),
        max_tokens: mc
            .and_then(|c| c.max_tokens)
            .or(ov.and_then(|p| p.max_tokens))
            .or(defaults.max_tokens),
        max_tokens_on_truncation: mc
            .and_then(|c| c.max_tokens_on_truncation)
            .or(ov.and_then(|p| p.max_tokens_on_truncation))
            .or(defaults.max_tokens_on_truncation),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registry overlay + validation
// ---------------------------------------------------------------------------

/// Merge models.yml provider overrides into the provider metadata map.
///
/// - provider-level fields (api_mode, supports_reasoning, default_base_url,
///   default_model) override the plugin manifest values;
/// - plugin-less providers (`plugin: false`) are added as builtin
///   chat_completions/anthropic definitions so they appear in provider
///   selects and can be used on threads.
pub fn apply_provider_overrides(
    data_dir: &str,
    map: &mut std::collections::HashMap<String, crate::llm::ProviderMetadata>,
) {
    let Ok(file) = load_models_file(data_dir) else {
        return;
    };
    for (name, ov) in &file.providers {
        let meta = map
            .entry(name.clone())
            .or_insert_with(|| crate::llm::ProviderMetadata {
                name: name.clone(),
                default_base_url: ov.default_base_url.clone().unwrap_or_default(),
                api_mode: ov
                    .api_mode
                    .clone()
                    .unwrap_or_else(|| "chat_completions".to_string()),
                api_modes: std::collections::HashMap::new(),
                default_model: ov.default_model.clone().unwrap_or_default(),
                supports_reasoning: ov.supports_reasoning.unwrap_or(false),
            });
        if let Some(v) = &ov.api_mode {
            meta.api_mode = v.clone();
        }
        if let Some(v) = ov.supports_reasoning {
            meta.supports_reasoning = v;
        }
        if let Some(v) = &ov.default_base_url {
            meta.default_base_url = v.clone();
        }
        if let Some(v) = &ov.default_model {
            meta.default_model = v.clone();
        }
    }
}

/// Semantic validation of a models.yml document (provider names, model
/// entries, budgets > 0). Used by PUT /api/models BEFORE the atomic write.
pub fn validate_models_file(file: &ModelsFile) -> AppResult<()> {
    for (name, ov) in &file.providers {
        if name.trim().is_empty() {
            return Err(Error::Message(
                "models.yml: provider name must not be empty".into(),
            ));
        }
        if let Some(models) = &ov.models {
            for m in models {
                if m.trim().is_empty() {
                    return Err(Error::Message(format!(
                        "models.yml: provider '{}' has an empty model entry",
                        name
                    )));
                }
            }
        }
        if let Some(mc) = &ov.model_config {
            for (model, cfg) in mc {
                if model.trim().is_empty() {
                    return Err(Error::Message(format!(
                        "models.yml: provider '{}' has an empty model_config key",
                        name
                    )));
                }
                validate_budget(name, model, "token_budget_soft", cfg.token_budget_soft)?;
                validate_budget(name, model, "token_budget_hard", cfg.token_budget_hard)?;
            }
        }
        validate_budget(name, name, "token_budget_soft", ov.token_budget_soft)?;
        validate_budget(name, name, "token_budget_hard", ov.token_budget_hard)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tmp dir")
    }

    fn write_models(dir: &tempfile::TempDir, content: &str) {
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let mut f = std::fs::File::create(config.join("models.yml")).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    const SAMPLE: &str = r#"
providers:
  deepseek:
    plugin: true
    models: ["deepseek-v4-flash", "deepseek-v4-pro"]
  my_provider_01:
    plugin: false
    api_mode: "chat_completions"
    supports_reasoning: true
    default_base_url: "http://noop-provider:9090/v1"
    refresh_url: "https://api.deepseek.com/v1/models"
    default_model: "test-model-1"
    api_key: "$secret:MY_SECRET"
    token_budget_soft: 150000
    token_budget_hard: 600000
    max_tokens: 16000
    models: ["my_model_01", "my_model_02", "my_model_03"]
    model_config:
      my_model_02:
        api_mode: "anthropic"
        supports_reasoning: false
        token_budget_soft: 200000
        token_budget_hard: 1000000
        max_tokens: 32000
        max_tokens_on_truncation: 128000
"#;

    #[test]
    fn parse_sample() {
        let dir = tmp_dir();
        write_models(&dir, SAMPLE);
        let file = load_models_file(dir.path().to_str().unwrap()).expect("parse ok");
        assert_eq!(file.providers.len(), 2);
        let ds = &file.providers["deepseek"];
        assert!(ds.plugin.is_true());
        assert_eq!(
            ds.models.as_ref().unwrap(),
            &vec![
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-pro".to_string()
            ]
        );
        let mp = &file.providers["my_provider_01"];
        assert!(!mp.plugin.is_true());
        assert_eq!(mp.api_mode.as_deref(), Some("chat_completions"));
        assert_eq!(mp.api_key.as_deref(), Some("$secret:MY_SECRET"));
        let m02 = &mp.model_config.as_ref().unwrap()["my_model_02"];
        assert_eq!(m02.token_budget_soft, Some(200000));
        assert_eq!(m02.max_tokens_on_truncation, Some(128000));
    }

    #[test]
    fn absent_file_is_empty() {
        let dir = tmp_dir();
        let file = load_models_file(dir.path().to_str().unwrap()).expect("absent ok");
        assert!(file.providers.is_empty());
    }

    #[test]
    fn malformed_file_errors() {
        let dir = tmp_dir();
        write_models(&dir, "providers:\n  bad:\n    models: [unterminated\n");
        let err = load_models_file(dir.path().to_str().unwrap());
        assert!(err.is_err(), "malformed yaml must error");
    }

    #[test]
    fn plugin_less_detection() {
        let dir = tmp_dir();
        write_models(&dir, SAMPLE);
        let d = dir.path().to_str().unwrap();
        assert!(!is_plugin_less(d, "deepseek"));
        assert!(is_plugin_less(d, "my_provider_01"));
        assert!(!is_plugin_less(d, "unknown"));
    }

    #[test]
    fn precedence_model_over_provider_over_defaults() {
        let dir = tmp_dir();
        write_models(&dir, SAMPLE);
        let d = dir.path().to_str().unwrap();
        let defaults = ModelGlobalDefaults {
            token_budget_soft: 100000,
            token_budget_hard: 500000,
            max_tokens: Some(8192),
            max_tokens_on_truncation: None,
        };
        // my_model_02: model_config wins for all values.
        let eff = resolve_effective(d, "my_provider_01", "my_model_02", &defaults);
        assert_eq!(eff.api_mode, "anthropic");
        assert!(!eff.supports_reasoning);
        assert_eq!(eff.token_budget_soft, 200000);
        assert_eq!(eff.token_budget_hard, 1000000);
        assert_eq!(eff.max_tokens, Some(32000));
        assert_eq!(eff.max_tokens_on_truncation, Some(128000));
        // my_model_01: provider-level wins (no model_config entry).
        let eff = resolve_effective(d, "my_provider_01", "my_model_01", &defaults);
        assert_eq!(eff.api_mode, "chat_completions");
        assert!(eff.supports_reasoning);
        assert_eq!(eff.token_budget_soft, 150000);
        assert_eq!(eff.token_budget_hard, 600000);
        assert_eq!(eff.max_tokens, Some(16000));
        assert_eq!(eff.max_tokens_on_truncation, None);
        // deepseek: no budgets in models.yml -> global defaults.
        let eff = resolve_effective(d, "deepseek", "deepseek-v4-flash", &defaults);
        assert_eq!(eff.token_budget_soft, 100000);
        assert_eq!(eff.token_budget_hard, 500000);
        assert_eq!(eff.max_tokens, Some(8192));
        // unknown provider: everything from defaults.
        let eff = resolve_effective(d, "nope", "m", &defaults);
        assert_eq!(eff.token_budget_soft, 100000);
        assert_eq!(eff.max_tokens, Some(8192));
    }

    #[test]
    fn model_api_mode_override() {
        let dir = tmp_dir();
        write_models(&dir, SAMPLE);
        let d = dir.path().to_str().unwrap();
        assert_eq!(
            resolve_model_api_mode(d, "my_provider_01", "my_model_02").as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            resolve_model_api_mode(d, "my_provider_01", "my_model_01"),
            None
        );
    }

    #[test]
    fn save_roundtrip_and_upsert_preserves_fields() {
        let dir = tmp_dir();
        write_models(&dir, SAMPLE);
        let d = dir.path().to_str().unwrap();

        // Upsert on an EXISTING entry updates ONLY models.
        let file =
            upsert_provider_models(d, "my_provider_01", vec!["a".into(), "b".into()]).unwrap();
        let saved = load_models_file(d).unwrap();
        let mp = &saved.providers["my_provider_01"];
        assert_eq!(
            mp.models.as_ref().unwrap(),
            &vec!["a".to_string(), "b".to_string()]
        );
        assert!(!mp.plugin.is_true(), "plugin flag must be untouched");
        assert_eq!(mp.api_key.as_deref(), Some("$secret:MY_SECRET"));
        assert_eq!(mp.api_mode.as_deref(), Some("chat_completions"));
        assert!(mp.model_config.is_some(), "model_config must be untouched");
        assert_eq!(file.providers.len(), 2);

        // Upsert on an ABSENT entry creates plugin:true + models.
        upsert_provider_models(d, "brand_new", vec!["x".into()]).unwrap();
        let saved = load_models_file(d).unwrap();
        let bn = &saved.providers["brand_new"];
        assert!(bn.plugin.is_true());
        assert_eq!(bn.models.as_ref().unwrap(), &vec!["x".to_string()]);
        // Existing entries still intact.
        assert!(saved.providers.contains_key("deepseek"));
        assert!(saved.providers.contains_key("my_provider_01"));
    }

    #[test]
    fn empty_models_file_is_valid() {
        let dir = tmp_dir();
        write_models(&dir, "");
        let file = load_models_file(dir.path().to_str().unwrap()).unwrap();
        assert!(file.providers.is_empty());
    }
}
