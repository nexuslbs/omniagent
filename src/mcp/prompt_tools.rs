//! Built-in prompt MCP tools — generate_initial_prompt, compact_messages.
//!
//! These are the "memory prompt generator and condenser" — stateless tools that
//! build system/planning prompts and compact conversation messages.
//! No database, Qdrant, pgvector, or hindsight dependencies.

use crate::mcp::{AppContext, McpTool, McpToolResult};
use crate::prompt_builder::{self, MemoryStore};
use serde_json::Value;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Tool: generate_initial_prompt
// ---------------------------------------------------------------------------

fn generate_initial_prompt_tool() -> McpTool {
    McpTool {
        name: "generate_initial_prompt".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "generate_initial_prompt"),
        description:
            "Generate system prompt and planning prompt for a new conversation. \
             Builds the full system prompt with memory context, tool descriptions, \
             and platform hints. Also generates the planning prompt for task decomposition. \
             Returns JSON with 'system_prompt' and 'planning_prompt' fields."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "profile_name": {
                    "type": "string",
                    "description": "Profile name (default: default)"
                },
                "platform": {
                    "type": "string",
                    "description": "Platform identifier (e.g. 'telegram', 'mattermost')"
                },
                "system_message": {
                    "type": "string",
                    "description": "Optional system message override"
                },
                "user_message": {
                    "type": "string",
                    "description": "User message for planning prompt"
                },
                "tool_names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of available tool names"
                },
                "plan_iteration": {
                    "type": "integer",
                    "description": "Planning iteration (0 = first pass)"
                },
                "max_iterations": {
                    "type": "integer",
                    "description": "Max planning iterations"
                },
                "previous_plan": {
                    "type": "string",
                    "description": "Previous plan text for iterative refinement"
                },
                "use_json_plan": {
                    "type": "boolean",
                    "description": "Whether to use JSON plan format"
                }
            },
            "required": []
        }),
        server_name: None,
        handler: Arc::new(|args: Value, ctx: AppContext| {
            Box::pin(async move {
                let default_profile = crate::profile::default_profile_name();
                let profile_name = args["profile_name"].as_str().unwrap_or(&default_profile);
                let platform = args["platform"].as_str().unwrap_or("");
                let system_message = args["system_message"].as_str();
                let user_message = args["user_message"].as_str().unwrap_or("");
                let tool_names: Vec<String> = args["tool_names"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let plan_iteration = args["plan_iteration"].as_u64().unwrap_or(0) as u32;
                let max_iterations = args["max_iterations"].as_u64().unwrap_or(5) as u32;
                let previous_plan = args["previous_plan"].as_str();
                let use_json_plan = args["use_json_plan"].as_bool().unwrap_or(false);

                let base_path = format!("{}/profiles/{}", ctx.data_dir, profile_name);
                let mut memory_store = MemoryStore::new(&base_path);
                memory_store.load_from_disk();

                let system_prompt = prompt_builder::build_system_prompt(
                    &memory_store,
                    platform,
                    system_message,
                    profile_name,
                    &tool_names,
                );

                let planning_prompt = prompt_builder::build_planning_prompt(
                    &memory_store,
                    prompt_builder::PlanningPromptParams {
                        platform,
                        profile_name,
                        user_message,
                        plan_iteration,
                        max_iterations,
                        previous_plan,
                        use_json_plan,
                    },
                    &tool_names,
                );

                let result = serde_json::json!({
                    "system_prompt": system_prompt,
                    "planning_prompt": planning_prompt,
                });

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| "Failed to serialize result".to_string()),
                    is_error: false,
                })
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool: compact_messages
// ---------------------------------------------------------------------------

fn compact_messages_tool() -> McpTool {
    McpTool {
        name: "compact_messages".to_string(),
        full_name: crate::mcp::tool_qualify("builtin", "compact_messages"),
        description:
            "Compact old assistant messages in a conversation to save tokens. \
             Removes redundant assistant tool-call pairs from the middle of the \
             conversation while preserving system messages, the most recent messages, \
             and tool results. Returns the compacted message array."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "messages": {
                    "type": "array",
                    "description": "Array of ChatMessage objects to compact"
                },
                "keep_recent": {
                    "type": "integer",
                    "description": "Number of most recent messages to always keep (default: 3)"
                }
            },
            "required": ["messages"]
        }),
        server_name: None,
        handler: Arc::new(|args: Value, _ctx: AppContext| {
            Box::pin(async move {
                let messages_arr = match args["messages"].as_array() {
                    Some(arr) => arr,
                    None => {
                        return Ok(McpToolResult {
                            call_id: String::new(),
                            content: "Missing required argument: 'messages' (array of ChatMessage)"
                                .to_string(),
                            is_error: true,
                        })
                    }
                };

                let keep_recent = args["keep_recent"].as_u64().unwrap_or(3) as usize;

                let mut messages: Vec<crate::llm::ChatMessage> =
                    match serde_json::from_value(serde_json::Value::Array(messages_arr.clone())) {
                        Ok(msgs) => msgs,
                        Err(e) => {
                            return Ok(McpToolResult {
                                call_id: String::new(),
                                content: format!("Failed to parse messages: {}", e),
                                is_error: true,
                            })
                        }
                    };

                let before = messages.len();
                crate::agent::helpers::compact_old_assistant_messages(&mut messages, keep_recent);
                let after = messages.len();

                let result = serde_json::json!({
                    "messages": messages,
                    "was_compacted": before != after,
                    "before_count": before,
                    "after_count": after,
                });

                Ok(McpToolResult {
                    call_id: String::new(),
                    content: serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| "Failed to serialize result".to_string()),
                    is_error: false,
                })
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// Return all built-in prompt tools for registration in the MCP registry.
pub fn all_prompt_tools() -> Vec<McpTool> {
    vec![generate_initial_prompt_tool(), compact_messages_tool()]
}
