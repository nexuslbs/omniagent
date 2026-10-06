//! Argument-shape repair for "bridge" tool calls.
//!
//! A bridge plugin (an external tool server) exposes exactly ONE tool - the
//! MCP server literally names it `tool` - whose DECLARED input schema is the
//! two-key envelope
//!
//! ```json
//! {"tool": "<inner tool name>", "params": {<inner tool args>}}
//! ```
//!
//! The plugin POSTs that envelope verbatim to the bridge's HTTP surface
//! (`POST /api/tool/call`), which unwraps it and runs the inner tool.
//!
//! Some models answer that envelope by REPEATING it inside `params`:
//!
//! ```json
//! {"tool": "agent_run",
//!  "params": {"tool": "agent_run", "params": {"role": "...", "objective": "..."}}}
//! ```
//!
//! The bridge then delivers `{tool, params}` to the inner tool as if it were
//! the inner tool's OWN params and the run fails with
//! "objective: missing required parameter; tool/params: unknown parameter"
//! (reported in thread 3908; reproduced verbatim through
//! `POST /mcp/execute` on the dev stack). The identical payload works over a
//! plain `curl` against the bridge HTTP surface, which is why the defect looked
//! like an omniagent -> plugin argument-shape bug.
//!
//! The repair collapses the redundant outer level so the bridge receives the
//! INNER envelope. It is deliberately narrow:
//!
//! * it only fires for tools whose DECLARED schema is the two-key envelope
//!   (so `core__call_and_wait`, which declares `tool` + `params` + `timeout`,
//!   is never rewritten);
//! * both the outer object and the nested `params` object must have EXACTLY the
//!   two envelope keys, and the nested `tool` must be the same non-empty string
//!   as the outer one - the observed shape of the model's self-duplication;
//! * the nested `params` must be an object.
//!
//! Anything else is returned untouched (no guessing), and every repair is
//! logged loudly by the caller.

use serde_json::{json, Value};

/// True when a tool's declared input schema IS the bridge envelope:
/// exactly the properties `tool` (string) and `params` (object), with `tool`
/// as the single required field.
pub fn is_bridge_envelope_schema(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    let Some(props) = obj.get("properties").and_then(|p| p.as_object()) else {
        return false;
    };
    if props.len() != 2 {
        return false;
    }
    let tool_type = props
        .get("tool")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str());
    let params_type = props
        .get("params")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str());
    if tool_type != Some("string") || params_type != Some("object") {
        return false;
    }
    match obj.get("required").and_then(|r| r.as_array()) {
        Some(req) => req.len() == 1 && req[0].as_str() == Some("tool"),
        None => false,
    }
}

/// Collapse a SELF-NESTED bridge envelope, returning the repaired arguments.
///
/// `None` means "not this shape": the caller must pass the arguments through
/// unchanged. The repair only applies to a tool whose declared schema is the
/// bridge envelope (see [`is_bridge_envelope_schema`]) and whose arguments
/// repeat that same envelope one level too deep.
pub fn repair_self_nested_envelope(schema: &Value, arguments: &Value) -> Option<Value> {
    if !is_bridge_envelope_schema(schema) {
        return None;
    }
    let outer = arguments.as_object()?;
    if outer.len() != 2 {
        return None;
    }
    let outer_tool = outer.get("tool")?.as_str()?.trim();
    if outer_tool.is_empty() {
        return None;
    }
    let inner = outer.get("params")?.as_object()?;
    if inner.len() != 2 {
        return None;
    }
    let inner_tool = inner.get("tool")?.as_str()?.trim();
    if inner_tool.is_empty() || inner_tool != outer_tool {
        return None;
    }
    let inner_params = inner.get("params")?;
    if !inner_params.is_object() {
        return None;
    }
    // The outer envelope merely repeated itself: the real dispatch is the
    // INNER envelope (same tool name, the inner tool's own params).
    Some(json!({ "tool": inner_tool, "params": inner_params }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema the external bridge MCP servers declare.
    fn bridge_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool": {"type": "string", "description": "bridge tool/command name"},
                "params": {"type": "object", "additionalProperties": true, "default": {}}
            },
            "required": ["tool"]
        })
    }

    /// The exact arguments of the thread-3908 repro (double-wrapped).
    fn thread_3908_arguments() -> Value {
        json!({
            "params": {
                "tool": "agent_run",
                "params": {
                    "role": "developer",
                    "objective": "Run the shell command sleep 300 and then report DONE.",
                    "timeoutSecs": 600
                }
            },
            "tool": "agent_run"
        })
    }

    #[test]
    fn repairs_the_thread_3908_self_nested_envelope() {
        let repaired = repair_self_nested_envelope(&bridge_schema(), &thread_3908_arguments())
            .expect("the self-nested envelope must be repaired");
        assert_eq!(
            repaired,
            json!({
                "tool": "agent_run",
                "params": {
                    "role": "developer",
                    "objective": "Run the shell command sleep 300 and then report DONE.",
                    "timeoutSecs": 600
                }
            })
        );
    }

    /// The correct (single) envelope is the shape the bridge expects: never
    /// rewritten, so the working `curl` body stays intact.
    #[test]
    fn leaves_the_correct_single_envelope_untouched() {
        let correct = json!({
            "tool": "agent_run",
            "params": {"role": "developer", "objective": "Reply OK", "timeoutSecs": 60}
        });
        assert!(repair_self_nested_envelope(&bridge_schema(), &correct).is_none());
    }

    /// An inner tool that legitimately takes `tool` + `params` of its own but
    /// with a DIFFERENT name is not a self-duplication: untouched.
    #[test]
    fn does_not_repair_a_different_inner_tool_name() {
        let nested = json!({
            "tool": "agent_run",
            "params": {"tool": "other_tool", "params": {"x": 1}}
        });
        assert!(repair_self_nested_envelope(&bridge_schema(), &nested).is_none());
    }

    /// Extra keys mean the object is not the envelope shape: untouched.
    #[test]
    fn does_not_repair_objects_with_extra_keys() {
        let with_timeout = json!({
            "tool": "agent_run",
            "params": {"tool": "agent_run", "params": {"objective": "x"}},
            "timeout": 30
        });
        assert!(repair_self_nested_envelope(&bridge_schema(), &with_timeout).is_none());

        let inner_extra = json!({
            "tool": "agent_run",
            "params": {"tool": "agent_run", "params": {"objective": "x"}, "id": 3}
        });
        assert!(repair_self_nested_envelope(&bridge_schema(), &inner_extra).is_none());
    }

    /// A `params` that is not an object is not the envelope shape.
    #[test]
    fn does_not_repair_non_object_params() {
        let nested = json!({"tool": "agent_run", "params": {"tool": "agent_run", "params": "x"}});
        assert!(repair_self_nested_envelope(&bridge_schema(), &nested).is_none());
        let stringy = json!({"tool": "agent_run", "params": "agent_run"});
        assert!(repair_self_nested_envelope(&bridge_schema(), &stringy).is_none());
    }

    /// The gate is the DECLARED schema, never the tool name: a tool that
    /// declares anything else (e.g. `core__call_and_wait` with
    /// `tool` + `params` + `timeout`) can never be rewritten.
    #[test]
    fn non_bridge_schemas_are_never_rewritten() {
        let call_and_wait_schema = json!({
            "type": "object",
            "properties": {
                "tool": {"type": "string"},
                "params": {"type": "object"},
                "timeout": {"type": "integer"}
            },
            "required": ["tool"]
        });
        // A LEGITIMATE `core__call_and_wait` into a bridge tool.
        let legit = json!({
            "tool": "external_agent__tool",
            "params": {"tool": "agent_run", "params": {"objective": "x"}}
        });
        assert!(repair_self_nested_envelope(&call_and_wait_schema, &legit).is_none());

        let no_required = json!({
            "type": "object",
            "properties": {"tool": {"type": "string"}, "params": {"type": "object"}}
        });
        assert!(!is_bridge_envelope_schema(&no_required));
        assert!(repair_self_nested_envelope(&no_required, &thread_3908_arguments()).is_none());

        let wrong_types = json!({
            "type": "object",
            "properties": {"tool": {"type": "string"}, "params": {"type": "string"}},
            "required": ["tool"]
        });
        assert!(!is_bridge_envelope_schema(&wrong_types));
    }
}
