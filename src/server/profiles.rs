//! Profiles API: list, update, create, and import profiles.
//!
//! Profile definitions AND runtime state live in
//! `{data_dir}/config/profiles.yml` — the single source of truth (the legacy
//! `profiles/<name>/config.json` stays on disk for backward compat but is
//! NOT read for resolution). The profile NAME is the yml key — the stable
//! identifier used everywhere (`threads.profile`, channels.yml `profile:`,
//! kanban boards/tasks `profile:`, dashboard profile selects).
//!
//! - `GET /profiles`        : list YAML-declared profiles (bare array, the
//!   dashboard `apiGet<ProfileData[]>` shape)
//! - `POST /profiles`       : create a profile (upsert into profiles.yml)
//! - `PATCH /profiles/{id}` : update fields (provider / model / plan /
//!   template / allowed_tools), persisted atomically to profiles.yml
//! - `POST /profiles/import`: import an external `profiles.yml`-structured
//!   document (raw YAML body OR JSON `{"yaml": "..."}`) and merge it into
//!   `{data_dir}/config/profiles.yml`
//!
//! IMPORT MERGE POLICY: imported entries OVERWRITE existing entries with the
//! same name (upsert semantics) — consistent with the channels import
//! precedent, where every imported channel is PATCHed/upserted into
//! channels.yml. The whole document is validated BEFORE the atomic save; on
//! any validation/parse error nothing is written. The response lists which
//! names were newly imported and which were updated.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{err_json, ok_json, AppState};
use crate::profiles_yaml::{validate_profile, ProfilesFile};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn profiles_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/profiles", get(list_profiles_handler))
        .route("/profiles", post(create_profile_handler))
        .route("/profiles/import", post(import_profiles_handler))
        .route("/profiles/{id}", patch(update_profile_handler))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Profile API view: the yml declaration plus the profile's filesystem
/// skills (`profiles/<name>/skills/*.md`) for the dashboard. Field names are
/// bare (`provider`/`model`/`plan`/`template`/`allowed_tools`).
#[derive(Debug, Clone, Serialize)]
pub struct ProfileEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

impl ProfileEntry {
    fn from_def(name: &str, def: &crate::profiles_yaml::ProfileDef, data_dir: &str) -> Self {
        Self {
            name: name.to_string(),
            provider: def.provider.clone(),
            model: def.model.clone(),
            plan: def.plan,
            template: def.template.clone(),
            allowed_tools: def.allowed_tools.clone(),
            skills: list_skills(data_dir, name),
        }
    }
}

/// Body of the create-profile endpoint (dashboard "+ Create Profile").
#[derive(Debug, Deserialize)]
struct CreateProfileRequest {
    name: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Body of the import endpoint. The external document may arrive as a JSON
/// `{"yaml": "..."}` payload (dashboard import modal) or as raw YAML text.
#[derive(Debug, Deserialize, Default)]
struct ImportRequest {
    #[serde(default)]
    yaml: Option<String>,
}

/// Fields a PATCH may update (bare names; empty string clears to None).
#[derive(Debug, Deserialize, Default)]
struct UpdateProfileRequest {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    plan: Option<bool>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `profiles/<name>/skills/*.md` on disk (profile FILES live in the profile
/// dir; the declaration lives in profiles.yml).
fn list_skills(data_dir: &str, name: &str) -> Vec<String> {
    let dir = std::path::Path::new(data_dir)
        .join("profiles")
        .join(name)
        .join("skills");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .filter(|s| s.ends_with(".md"))
        .collect();
    names.sort();
    names
}

/// Normalize an optional field from the API: empty/whitespace → None.
fn clean_opt(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Extract the YAML document from an import request body: a JSON
/// `{"yaml": "..."}` payload or the raw body itself (YAML text).
fn yaml_from_body(body: &str) -> Result<String, String> {
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        if let Ok(req) = serde_json::from_str::<ImportRequest>(trimmed) {
            if let Some(y) = req.yaml.filter(|s| !s.trim().is_empty()) {
                return Ok(y);
            }
        }
    }
    if trimmed.is_empty() {
        return Err(
            "empty import body: expected a profiles.yml-structured YAML document".to_string(),
        );
    }
    Ok(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /profiles — YAML-declared profiles, sorted by name. Returns a BARE
/// array (dashboard consumes it as `ProfileData[]`), mirroring GET /channels.
async fn list_profiles_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ProfileEntry>>, (StatusCode, Json<serde_json::Value>)> {
    let file = match crate::profiles_yaml::load_profiles_from(&state.data_dir) {
        Ok(f) => f,
        Err(e) => return Err(err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    };
    let mut entries: Vec<ProfileEntry> = file
        .profiles
        .iter()
        .map(|(name, def)| ProfileEntry::from_def(name, def, &state.data_dir))
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(entries))
}

/// POST /profiles — create (upsert) a profile from `{name, provider, model}`.
async fn create_profile_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateProfileRequest>,
) -> impl IntoResponse {
    let name = req.name.trim().to_string();
    let def = crate::profiles_yaml::ProfileDef {
        provider: clean_opt(req.provider),
        model: clean_opt(req.model),
        allowed_tools: Some(Vec::new()),
        ..Default::default()
    };
    if let Err(e) = validate_profile(&name, &def) {
        return err_json(StatusCode::BAD_REQUEST, &e);
    }
    match crate::profiles_yaml::update_profile_in(&state.data_dir, &name, |_existing| Ok(def)) {
        Ok(def) => ok_json(ProfileEntry::from_def(&name, &def, &state.data_dir)),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// PATCH /profiles/{id} — update one or more bare fields. Empty strings
/// clear provider/model/template to None (fall through the resolution chain).
async fn update_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateProfileRequest>,
) -> impl IntoResponse {
    let name = id.trim().to_string();
    if name.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "profile name must not be empty");
    }
    let result = crate::profiles_yaml::update_profile_in(&state.data_dir, &name, |existing| {
        let mut def = existing.cloned().unwrap_or_default();
        if req.provider.is_some() {
            def.provider = clean_opt(req.provider.clone());
        }
        if req.model.is_some() {
            def.model = clean_opt(req.model.clone());
        }
        if req.plan.is_some() {
            def.plan = req.plan;
        }
        if req.template.is_some() {
            def.template = clean_opt(req.template.clone());
        }
        if req.allowed_tools.is_some() {
            def.allowed_tools = req.allowed_tools.clone();
        }
        Ok(def)
    });
    match result {
        Ok(def) => ok_json(ProfileEntry::from_def(&name, &def, &state.data_dir)),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// POST /profiles/import — merge an external profiles.yml-structured
/// document into `{data_dir}/config/profiles.yml`.
///
/// Body: raw YAML text (`profiles:` top-level) OR JSON `{"yaml": "..."}`.
/// Merge policy: existing entries with the same name are OVERWRITTEN
/// (upsert — same as the channels import precedent); new names are added.
/// The whole document is validated BEFORE the atomic save; on error nothing
/// is written. Response: `{imported: [...], updated: [...]}`.
async fn import_profiles_handler(
    State(state): State<Arc<AppState>>,
    body: String,
) -> impl IntoResponse {
    let yaml_text = match yaml_from_body(&body) {
        Ok(y) => y,
        Err(e) => return err_json(StatusCode::BAD_REQUEST, &e),
    };
    let parsed: ProfilesFile = match serde_yaml::from_str(&yaml_text) {
        Ok(p) => p,
        Err(e) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                &format!("Failed to parse profiles YAML: {}", e),
            )
        }
    };
    if parsed.profiles.is_empty() {
        return err_json(
            StatusCode::BAD_REQUEST,
            "No profiles found: expected a top-level `profiles:` map",
        );
    }
    // Validate every entry BEFORE persisting anything.
    for (name, def) in &parsed.profiles {
        if let Err(e) = validate_profile(name, def) {
            return err_json(StatusCode::BAD_REQUEST, &e);
        }
    }
    match crate::profiles_yaml::merge_profiles_file_in(&state.data_dir, &parsed) {
        Ok((added, updated)) => {
            let mut message = format!("Imported {} profile(s)", added.len() + updated.len());
            if !added.is_empty() {
                message.push_str(&format!("; new: {}", added.join(", ")));
            }
            if !updated.is_empty() {
                message.push_str(&format!("; updated: {}", updated.join(", ")));
            }
            ok_json(serde_json::json!({
                "imported": added,
                "updated": updated,
                "message": message,
            }))
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_from_body_accepts_raw_yaml_and_json() {
        let raw = "profiles:\n  omni:\n    plan: false\n";
        // Raw YAML is returned trimmed (trailing whitespace/newline dropped).
        assert_eq!(yaml_from_body(raw).unwrap(), raw.trim());
        let json = format!(r#"{{"yaml": "{}"}}"#, raw.replace('\n', "\\n"));
        assert_eq!(yaml_from_body(&json).unwrap(), raw);
        assert!(yaml_from_body("").is_err());
        assert!(yaml_from_body("   ").is_err());
    }

    #[test]
    fn import_parse_and_validation() {
        // Valid document parses into a ProfilesFile with the expected entries.
        let yaml = r#"
profiles:
  omni:
    allowed_tools: []
  research:
    provider: opencode-go
    model: deepseek-v4-flash
    plan: true
"#;
        let parsed: ProfilesFile = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(parsed.profiles.len(), 2);
        for (name, def) in &parsed.profiles {
            validate_profile(name, def).expect("valid entry");
        }
        // An entry with an empty tool name fails validation (loud, pre-write).
        let bad = r#"
profiles:
  broken:
    allowed_tools:
      - ""
"#;
        let parsed_bad: ProfilesFile = serde_yaml::from_str(bad).expect("parse");
        let name = parsed_bad.profiles.keys().next().unwrap().clone();
        let def = &parsed_bad.profiles[&name];
        assert!(validate_profile(&name, def).is_err());
    }

    #[test]
    fn clean_opt_normalizes_empty() {
        assert_eq!(clean_opt(Some("  ".to_string())), None);
        assert_eq!(
            clean_opt(Some("opencode-go".to_string())).as_deref(),
            Some("opencode-go")
        );
        assert_eq!(clean_opt(None), None);
    }
}
