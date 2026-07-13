# OmniAgent Rust Backend: Code Quality Analysis

Found **50+ issues** across 7 categories. All are internal refactoring opportunities (no external API/behavior changes needed).

---

## 1. DEAD CODE: `#[expect(dead_code)]` / `#[allow(dead_code)]` (~47 instances)

**Entire structs/functions that are never used:**

| Location | What's Dead | Lines |
|---|---|---|
| `src/config.rs` | `socket_addr()` method | 97-101 |
| `src/mcp/mod.rs` | `McpToolCall.id`, `McpToolResult.call_id`, `McpToolResult.is_error` fields | 37, 46, 49 |
| `src/mcp/mod.rs` | `to_openai_tools_all()` method | 171-186 |
| `src/agent/mod.rs` | `AgentConfig.summarize_after_days` field | 53-54 |
| `src/llm/mod.rs` | `LLMConfig.max_tokens`, `LLMConfig.temperature` fields | 218-221 |
| `src/llm/mod.rs` | Response struct fields (`OpenAiResponse.id`, `OpenAiChoice.finish_reason`, `OpenAiChoice.index`, `AnthropicContentBlock.signature`) | 397-465 |
| `src/platform/mod.rs` | `Platform::send_response()` trait method | 33-34 |
| `src/platform/telegram.rs` | `delete_message()`, `get_updates()`, `inbound_polling_loop()`, `inbound_ws_loop()` | 117, 144, 450, 707 |
| `src/db/types.rs` | `ChannelStopDb` struct | 198-221 |
| `src/db/types.rs` | `SummaryDb.channel_id`, `SummaryDb.created_at` fields | 230, 234 |
| `src/db/types.rs` | `SubscriptionDb.created_at` field | 248-249 |
| `src/db/types.rs` | `set_thread_pending()` function | 488-498 |
| `src/models/profile.rs` | `ProfileRow` struct: **entire file is dead** | 7-34 |
| `src/models/profile.rs` | `ProfileNew` struct: **entire file is dead** | 24-34 |
| `src/models/mod.rs` | `#[expect(unused_imports)]` on `ProfileNew`, `ProfileRow` exports | 13-16 |
| `src/models/thread.rs` | Some field (line 26) | 26 |
| `src/models/channel.rs` | Some field (line 50) | 50 |
| `src/profile/mod.rs` | `resolve_model()`, `resolve_provider()`, `default_profile`, `data_dir`, `default()` | 128-151, 214-218 |
| `src/main.rs` | `get_max_sequence()` function | 898-901 |
| `src/main.rs` | Helper structs in CLI (`LastThread`, `MessageContentOnly`, `ResponseMsg`) | 815, 942, 948 |
| `src/context_builder.rs` | **Entire file-level `#![allow(dead_code)]`** | 15 |
| `src/context_builder.rs` | `classify_query()` deprecated function | 332-340 |
| `src/mcp/tools/kanban.rs` | `KanbanTaskRow.display_id` field | 13 |
| `src/mcp/external/client.rs` | 6+ dead code instances | 33, 89, 101, 124, 251, 254, 497, 500 |
| `src/mcp/external/protocol.rs` | 1 instance | 87 |

**Refactoring suggestion**: Remove or consolidate. The `models/profile.rs` file is entirely unused: it's a DB-backed profile model while the actual profile system uses filesystem-backed `Profile` in `profile/mod.rs`. Similarly, `config.rs:socket_addr()` is never called.

---

## 2. DUPLICATED CODE

### 2a. Two number-formatting functions: same logic
- `src/main.rs:1578`: `fn format_num(n: i64) -> String`
- `src/prompt_builder.rs:157`: `fn format_thousands(n: usize) -> String`

Both insert thousands separators with nearly identical implementations. Extract into a shared utility.

### 2b. Complexity classification duplicated across modules
- `src/db/types.rs:425`: `classify_complexity_for_planning()`: returns planning mode string
- `src/context_builder.rs:294`: `classify_complexity()`: returns `Complexity` enum
- `src/context_builder.rs:332`: `classify_query()`: deprecated wrapper

All three classify messages by length + keywords with slightly different keyword lists and thresholds. Unify into one shared function.

### 2c. OMNI_DIR / WORKSPACE_DIR read 4× in main.rs
Lines 70, 101, 108, and 112 all read the same env vars with the same fallback. Lines 108-112 are redundant with 70-75 and 101-105.

### 2d. Vectorization config has duplicated fields
`src/config.rs:13-26`: Messages and wiki vectorization configs are fully duplicated (6 fields each: method, api_url, interval_secs, protocol, api_key, api_model). Extract into a `VectorizerConfig` struct and have two instances.

### 2e. LLM_API_KEY fallback pattern duplicated 4×
The same pattern `{PROVIDER}_API_KEY` → `LLM_API_KEY` fallback appears in:
1. `agent/mod.rs:97-109`: `AgentConfig::from_env()`
2. `llm/mod.rs:247-255`: `LLMConfig::from_env()`
3. `server/mod.rs:483-495`: prompt preview handler
4. `plugin/mod.rs:376-377, 502-506`: `enrich_plugin()` and `refresh_plugin_models()`

Extract into a shared `resolve_api_key(provider_name: &str) -> String` function.

### 2f. MCP tool handler `block_in_place` pattern repeated 7× in kanban.rs
Each tool handler (create, list, update, delete, add_dependency, remove_dependency, etc.) uses the same boilerplate:
```rust
tokio::task::block_in_place(|| {
    let handle = tokio::runtime::Handle::current();
    handle.block_on(async { ... })
})
```

### 2g. Channel SELECT query duplication
The same ~12-column SELECT with timestamp formatting is repeated in 6+ functions in `db/types.rs` (`find_all_channels`, `get_channel_by_name`, `get_channel_by_platform_name`, etc.). Extract into a SQL view or helper macro.

---

## 3. OVERLY LONG FUNCTIONS

| Function | Lines | Issues |
|---|---|---|
| `main.rs:run_cli()` | ~380 | Contains inline `/new` transaction logic, polling, channel management: split into helper functions |
| `main.rs:run_server()` | ~200 | Startup orchestration: could separate into smaller builder steps |
| `main.rs:poll_for_response()` | ~160 | CLI polling loop: tightly coupled to print formatting |
| `scheduler.rs:tick()` | ~280 | Both action and agentic cron modes inline |
| `agent/mod.rs:channel_handler()` inner block | ~150 | Should split thread processing from supervision |
| `config.rs:from_env()` | ~65 | All sequential env var reads: repetitive |
| `agent/mod.rs:from_env()` | ~60 | Same pattern |
| `context_builder.rs:build_thread_context()` | ~180 | RRF fusion, hindsight recall, retrieval all inline |
| `mcp/tools/kanban.rs:update_kanban_task_tool()` handler | ~100 | Individual UPDATE per field |

---

## 4. ANTI-PATTERNS

### 4a. `block_in_place` + `block_on` in sync tool handlers
MCP tool handlers are `sync` closures but call async DB code. They use `tokio::task::block_in_place` → `Handle::block_on` which is risky (can cause worker thread starvation under load). Make handlers async or use a different pattern.

### 4b. Sequential per-field UPDATEs in kanban.rs:update_kanban_task_tool()
Lines 285-358: Each field update issues a separate SQL UPDATE. This is N+1 queries per update and not atomic. Build a single UPDATE with conditional SET clauses.

### 4c. No shared module-level constants for env var names
Env var names like `"LLM_API_KEY"`, `"OMNI_DIR"`, `"WORKSPACE_DIR"`, `"PLANNING_MODE"` are hardcoded as string literals in 15+ locations across the codebase. Define as `const` or `static` in a central config module.

### 4d. `format_thousands` uses manual string reversal
The `format_num`/`format_thousands` functions use `chars().rev()` to insert thousands separators. Use a well-known crate instead of reinventing this.

---

## 5. ERROR HANDLING ISSUES

| Location | Issue | Severity |
|---|---|---|
| `llm/mod.rs:494` | `.expect("Failed to build reqwest Client")`: panics if TLS backend missing | Medium |
| `profile/mod.rs:218` | `.expect("Default profile must exist")`: panics if default not found | Medium |
| `plugin/installer.rs:123,127` | `parent().unwrap()` on paths: could panic if path has no parent | Medium |
| `main.rs:316,454` | `.unwrap()` on `SystemTime::now().duration_since(UNIX_EPOCH)`: could panic if system clock is before epoch | Low |
| `main.rs:99` | `.unwrap()` on `SystemTime::now().duration_since(UNIX_EPOCH)` in CLI mode | Low |

---

## 6. HARDCODED ENV VAR NAMES (not centralized as constants)

The following env var names are string literals scattered across multiple files with no shared constants:

- `LLM_API_KEY`: 5+ files
- `LLM_MODEL`, `LLM_PROVIDER`, `LLM_BASE_URL`: 4+ files
- `LLM_MAX_TOKENS`, `LLM_TEMPERATURE`: 2 files
- `OMNI_DIR`, `WORKSPACE_DIR`: 3+ files
- `PLANNING_MODE`: 2 files
- `MAX_ITERATIONS_*`: 1 file
- All `VECTORIZE_*` and `WIKI_VECTORIZATION_*` vars in `config.rs`
- `HINDSIGHT_URL`, `HINDSIGHT_BANK`: 2 files

---

## 7. OTHER FINDINGS

### 7a. `context_builder.rs` has `#![allow(dead_code)]` at module level (line 15)
This blanket-allows dead code for the entire file, hiding unused functions like `classify_query()` and possibly the `ContextBuilder` struct itself if it's not called from the changed path.

### 7b. `default_base_url` resolution duplicated
`llm/mod.rs:239-245` and `server/mod.rs:476-482` both have the same match on provider names for default base URLs. This should reference `PROVIDER_METADATA` or be extracted.

### 7c. `db/types:resolve_thread_planning_mode()` and `resolve_thread_planning_mode_with_content()`
These two functions are nearly identical: one just falls through to `classify_complexity_for_planning()` at the end while the other returns `"prompt_only"`. Could be unified with a parameter.

### 7d. Unused `HashMap::with_capacity`
Import of `std::collections::HashMap` with `std::collections::HashMap::new()` used in places where `HashMap::with_capacity()` would be more efficient (known sizes in `context_builder.rs` RRF fusion section).

---

## Summary

| Category | Count | Impact |
|---|---|---|
| Dead code (suppressed warnings) | ~47 | Code bloat, confusion |
| Duplicated logic | ~12 blocks | Maintenance burden |
| Overly long functions | ~9 functions | Difficult to test/review |
| Error handling (`.expect`/`.unwrap`) | ~5 production | Potential panics |
| Hardcoded env var names | ~30+ string literals | Brittle config |
| Anti-patterns | ~4 patterns | Performance/risk |

**Top 3 highest-value refactoring targets:**
1. Extract shared `resolve_api_key()` function: eliminates 4 copies of same fallback logic
2. Remove unused `models/profile.rs` (entire file is dead code) + consolidate dead structs
3. Extract number formatting + complexity classification into shared utility functions
