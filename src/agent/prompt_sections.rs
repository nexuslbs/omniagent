//! Scoped/ordered prompt sections (system prompt assembly).
//!
//! Task 9: the prompt plugin MAY return `sections: [{name, order, text}]` in
//! addition to (or instead of) the legacy flat `{system, memory, context,
//! user}` fields. The core assembles the system prompt by concatenating
//! sections in ascending `order`, with per-thread SCOPE SHADOWING (a thread
//! template's sections shadow channel sections, which shadow the plugin's
//! global sections) and `{{variable}}` interpolation from a variables
//! registry. Unknown variables and duplicate section names within one layer
//! fail loudly.
//!
//! Ordering convention (dsh pattern): identity -100, deployment/persona 0,
//! thread template 50, tool guidance 100-199, channel/platform 200+.

use crate::error::{AppResult, Error};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A single named, ordered prompt section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptSection {
    pub name: String,
    pub order: i64,
    pub text: String,
}

impl PromptSection {
    pub(crate) fn new(name: impl Into<String>, order: i64, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            order,
            text: text.into(),
        }
    }
}

/// YAML frontmatter of a thread template file. Only `prompt_sections` is
/// consumed; unknown keys are ignored so templates can keep other metadata.
#[derive(Debug, Default, Deserialize)]
struct TemplateFrontmatter {
    #[serde(default)]
    prompt_sections: Vec<PromptSection>,
}

/// Validate that a single layer has unique section names. Duplicate name in
/// one layer fails loudly (scope shadowing only applies ACROSS layers).
pub(crate) fn validate_layer(sections: &[PromptSection], layer: &str) -> AppResult<()> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(sections.len());
    for s in sections {
        if !seen.insert(s.name.as_str()) {
            return Err(Error::Message(format!(
                "duplicate prompt section name '{}' in {layer} layer (names must be unique per layer)",
                s.name
            )));
        }
    }
    Ok(())
}

/// Parse the optional `sections` array from the prompt plugin response.
///
/// Returns `Ok(None)` when `sections` is absent, null or an empty array
/// (backward compatible: the caller then renders the legacy flat fields
/// exactly as before). Errors loudly on a non-array value, malformed
/// entries (missing `name`/`order`/`text`) or duplicate names.
pub(crate) fn parse_plugin_sections(
    value: &serde_json::Value,
) -> AppResult<Option<Vec<PromptSection>>> {
    let sections = match value.get("sections") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let arr = sections.as_array().ok_or_else(|| {
        Error::Message("prompt plugin returned 'sections' but it is not an array".to_string())
    })?;
    if arr.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                Error::Message(format!(
                    "prompt section #{} is missing a string 'name' field",
                    i
                ))
            })?;
        let order = item.get("order").and_then(|v| v.as_i64()).ok_or_else(|| {
            Error::Message(format!(
                "prompt section '{name}' is missing an integer 'order' field"
            ))
        })?;
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                Error::Message(format!(
                    "prompt section '{name}' is missing a string 'text' field"
                ))
            })?;
        out.push(PromptSection { name, order, text });
    }
    validate_layer(&out, "plugin")?;
    Ok(Some(out))
}

/// Parse an optional YAML frontmatter block from a thread template file.
///
/// Returns `(sections, body)`: the template-scoped sections declared in the
/// frontmatter and the remaining body (with the frontmatter stripped). A
/// template without a frontmatter block returns `(vec![], content)`
/// unchanged — legacy templates render byte-identical.
///
/// Frontmatter format (convention used by the repo's memory files):
///
/// ```markdown
/// ---
/// prompt_sections:
///   - name: identity
///     order: -100
///     text: "..."
/// ---
/// template body...
/// ```
pub(crate) fn parse_template_frontmatter(content: &str) -> AppResult<(Vec<PromptSection>, &str)> {
    if !content.starts_with("---") {
        return Ok((Vec::new(), content));
    }
    // The frontmatter starts right after the opening `---` line.
    let after_open = &content[3..];
    let after_open = after_open
        .strip_prefix("\r\n")
        .or_else(|| after_open.strip_prefix('\n'));
    let Some(after_open) = after_open else {
        return Ok((Vec::new(), content)); // `---` at EOF: not a frontmatter
    };
    // Locate the closing `---` line (a line that is exactly `---`).
    let mut close_byte = None;
    let mut consumed = 0usize;
    for line in after_open.split_inclusive('\n') {
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line == "---" {
            close_byte = Some(consumed);
            break;
        }
        consumed += line.len();
        if !line.is_empty() {
            consumed += 1; // the '\n' carried by split_inclusive
        }
    }
    let Some(close) = close_byte else {
        // Unterminated `---` block: treat the whole file as body (the
        // frontmatter never closes, so there is nothing to strip).
        return Ok((Vec::new(), content));
    };
    let yaml = &after_open[..close];
    let after_close = &after_open[close + 3..]; // skip the closing `---`
    let after_close = after_close
        .strip_prefix("\r\n")
        .or_else(|| after_close.strip_prefix('\n'))
        .unwrap_or(after_close);
    let fm: TemplateFrontmatter = serde_yaml::from_str(yaml).map_err(|e| {
        Error::Message(format!(
            "failed to parse prompt_sections frontmatter in template: {e}"
        ))
    })?;
    validate_layer(&fm.prompt_sections, "template")?;
    Ok((fm.prompt_sections, after_close))
}

/// Interpolate `{{variable}}` placeholders in section text.
///
/// Unknown variables fail loudly. Unmatched braces (`{{` without a closing
/// `}}`) are left literal so ordinary prose containing braces is safe.
pub(crate) fn interpolate(text: &str, variables: &HashMap<String, String>) -> AppResult<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let var_name = after[..end].trim();
                if var_name.is_empty() {
                    return Err(Error::Message(
                        "empty '{{}}' placeholder in prompt section text".to_string(),
                    ));
                }
                let value = variables.get(var_name).ok_or_else(|| {
                    Error::Message(format!(
                        "unknown prompt variable '{{{{{var_name}}}}}' (known variables: {})",
                        known_variables(variables)
                    ))
                })?;
                out.push_str(value);
                rest = &after[end + 2..];
            }
            None => {
                // Unmatched opening brace: keep literally.
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn known_variables(variables: &HashMap<String, String>) -> String {
    if variables.is_empty() {
        "none".to_string()
    } else {
        let mut keys: Vec<&str> = variables.keys().map(|s| s.as_str()).collect();
        keys.sort();
        keys.join(", ")
    }
}

/// Assemble the system prompt from ordered, scoped layers.
///
/// `layers` are given from lowest to highest priority: the plugin's global
/// sections first, then channel-scoped sections, then template-scoped
/// sections. A section in a later layer SHADOWS a same-named section in an
/// earlier layer for this thread only (scope shadowing, per-thread design).
/// The winning sections are sorted by ascending `order`, interpolated, and
/// concatenated with `\n\n`. Empty rendered sections are dropped; an empty
/// result yields `""`.
pub(crate) fn assemble(
    layers: &[Vec<PromptSection>],
    variables: &HashMap<String, String>,
) -> AppResult<String> {
    // Merge: later layers shadow earlier ones with the same name.
    let mut merged: HashMap<String, PromptSection> = HashMap::new();
    for layer in layers {
        for section in layer {
            merged.insert(section.name.clone(), section.clone());
        }
    }
    let mut sections: Vec<PromptSection> = merged.into_values().collect();
    sections.sort_by_key(|s| s.order);
    let mut parts = Vec::with_capacity(sections.len());
    for s in &sections {
        let rendered = interpolate(&s.text, variables)?;
        if !rendered.trim().is_empty() {
            parts.push(rendered);
        }
    }
    Ok(parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> HashMap<String, String> {
        let mut v = HashMap::new();
        v.insert("profile_name".to_string(), "omni".to_string());
        v.insert("channel".to_string(), "dev".to_string());
        v.insert("thread_id".to_string(), "42".to_string());
        v.insert("platform".to_string(), "mattermost".to_string());
        v.insert("tools".to_string(), "read, write".to_string());
        v
    }

    // ── parse_plugin_sections ───────────────────────────────────────────────

    #[test]
    fn absent_sections_returns_none() {
        let value = serde_json::json!({ "system": "s", "user": "u" });
        assert!(parse_plugin_sections(&value).unwrap().is_none());
    }

    #[test]
    fn null_and_empty_sections_return_none() {
        assert!(
            parse_plugin_sections(&serde_json::json!({ "sections": null }))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_plugin_sections(&serde_json::json!({ "sections": [] }))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parses_valid_sections() {
        let value = serde_json::json!({
            "sections": [
                { "name": "identity", "order": -100, "text": "You are X" },
                { "name": "tool_guidance", "order": 100, "text": "Rules" },
            ],
            "user": "hi",
        });
        let sections = parse_plugin_sections(&value).unwrap().unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "identity");
        assert_eq!(sections[0].order, -100);
        assert_eq!(sections[1].text, "Rules");
    }

    #[test]
    fn non_array_sections_fails() {
        let err = parse_plugin_sections(&serde_json::json!({ "sections": "oops" })).unwrap_err();
        assert!(err.to_string().contains("not an array"));
    }

    #[test]
    fn malformed_entry_fails() {
        let err = parse_plugin_sections(&serde_json::json!({
            "sections": [{ "name": "identity" }]
        }))
        .unwrap_err();
        assert!(err.to_string().contains("order"));
    }

    #[test]
    fn duplicate_name_in_one_layer_fails() {
        let err = parse_plugin_sections(&serde_json::json!({
            "sections": [
                { "name": "identity", "order": -100, "text": "a" },
                { "name": "identity", "order": 0, "text": "b" },
            ]
        }))
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("duplicate prompt section name 'identity'"));
    }

    // ── interpolation ───────────────────────────────────────────────────────

    #[test]
    fn interpolates_known_variables() {
        let out = interpolate(
            "Hi {{profile_name}} on {{platform}} (thread {{thread_id}})",
            &vars(),
        )
        .unwrap();
        assert_eq!(out, "Hi omni on mattermost (thread 42)");
    }

    #[test]
    fn unknown_variable_fails_loudly() {
        let err = interpolate("Hello {{nope}}", &vars()).unwrap_err();
        assert!(err
            .to_string()
            .contains("unknown prompt variable '{{nope}}'"));
    }

    #[test]
    fn unmatched_braces_stay_literal() {
        let out = interpolate("literal {{ brace", &vars()).unwrap();
        assert_eq!(out, "literal {{ brace");
        let out2 = interpolate("no placeholders", &vars()).unwrap();
        assert_eq!(out2, "no placeholders");
    }

    // ── assembly: ordering, scoping, empty handling ─────────────────────────

    #[test]
    fn assembles_in_ascending_order() {
        let global = vec![
            PromptSection::new("tool_guidance", 100, "rules"),
            PromptSection::new("identity", -100, "you are X"),
            PromptSection::new("platform", 200, "platform note"),
        ];
        let out = assemble(&[global], &vars()).unwrap();
        let mut lines = out.split("\n\n");
        assert_eq!(lines.next(), Some("you are X"));
        assert_eq!(lines.next(), Some("rules"));
        assert_eq!(lines.next(), Some("platform note"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn template_shadows_global_same_name() {
        let global = vec![PromptSection::new("identity", -100, "global identity")];
        let template = vec![PromptSection::new("identity", -100, "template identity")];
        let out = assemble(&[global, template], &vars()).unwrap();
        assert_eq!(out, "template identity");
    }

    #[test]
    fn channel_shadows_global_but_template_shadows_channel() {
        let global = vec![PromptSection::new("persona", 0, "global persona")];
        let channel = vec![PromptSection::new("persona", 0, "channel persona")];
        let template = vec![PromptSection::new("persona", 0, "template persona")];
        // channel shadows global
        assert_eq!(
            assemble(&[global.clone(), channel.clone()], &vars()).unwrap(),
            "channel persona"
        );
        // template shadows channel
        assert_eq!(
            assemble(&[global, channel, template], &vars()).unwrap(),
            "template persona"
        );
    }

    #[test]
    fn shadowed_section_keeps_its_order() {
        // The shadowing section declares a DIFFERENT order: the winner's
        // order governs placement.
        let global = vec![PromptSection::new("identity", -100, "global identity")];
        let template = vec![PromptSection::new("identity", 500, "late identity")];
        let out = assemble(&[global, template], &vars()).unwrap();
        assert_eq!(out, "late identity");
    }

    #[test]
    fn empty_rendered_sections_are_dropped() {
        let global = vec![
            PromptSection::new("identity", -100, "keep me"),
            PromptSection::new("skip", 0, "  "),
        ];
        let out = assemble(&[global], &vars()).unwrap();
        assert_eq!(out, "keep me");
    }

    #[test]
    fn unknown_variable_during_assembly_fails() {
        let global = vec![PromptSection::new("identity", -100, "hi {{missing}}")];
        let err = assemble(&[global], &vars()).unwrap_err();
        assert!(err.to_string().contains("unknown prompt variable"));
    }

    #[test]
    fn empty_layers_render_empty() {
        assert_eq!(assemble(&[], &vars()).unwrap(), "");
        assert_eq!(assemble(&[Vec::new()], &vars()).unwrap(), "");
    }

    // ── template frontmatter ────────────────────────────────────────────────

    #[test]
    fn no_frontmatter_returns_body_unchanged() {
        let body = "just a template body";
        let (sections, out) = parse_template_frontmatter(body).unwrap();
        assert!(sections.is_empty());
        assert_eq!(out, body);
    }

    #[test]
    fn parses_frontmatter_and_strips_it() {
        let content = "---\nprompt_sections:\n  - name: identity\n    order: -100\n    text: \"scoped\"\n---\nbody here\n";
        let (sections, body) = parse_template_frontmatter(content).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "identity");
        assert_eq!(sections[0].order, -100);
        assert_eq!(sections[0].text, "scoped");
        assert_eq!(body, "body here\n");
    }

    #[test]
    fn duplicate_template_sections_fail() {
        let content = "---\nprompt_sections:\n  - name: a\n    order: 1\n    text: x\n  - name: a\n    order: 2\n    text: y\n---\nbody\n";
        let err = parse_template_frontmatter(content).unwrap_err();
        assert!(err
            .to_string()
            .contains("duplicate prompt section name 'a'"));
    }

    #[test]
    fn malformed_frontmatter_fails_loudly() {
        let content = "---\nprompt_sections: [unclosed\n---\nbody\n";
        let err = parse_template_frontmatter(content).unwrap_err();
        assert!(err.to_string().contains("frontmatter"));
    }

    #[test]
    fn unterminated_frontmatter_treated_as_body() {
        let content = "---\nprompt_sections:\n  - name: a\n    order: 1\n    text: x\nno closing";
        let (sections, body) = parse_template_frontmatter(content).unwrap();
        assert!(sections.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn frontmatter_with_other_keys_ignores_them() {
        let content = "---\ntitle: My Template\nprompt_sections:\n  - name: a\n    order: 1\n    text: x\n---\nbody\n";
        let (sections, body) = parse_template_frontmatter(content).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(body, "body\n");
    }
}
