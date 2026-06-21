# Runtime Deadlock & Infinite Loop — Root Cause Analysis

## Overview

On June 21, 2026, two critical bugs were identified and fixed in the OmniAgent codebase:

1. **Infinite loop** in `resolve_env_var()` causing `/api/plugins` and `/settings` to hang
2. **Runtime-wide deadlock** from `handle.block_on()` in MCP tools causing all endpoints to hang
3. **Blocking file I/O** on the tokio async runtime causing intermittent hangs
4. **Hardcoded Mattermost SiteURL** preventing external access

---

## Bug #1: Infinite Loop in `resolve_env_var()`

### Symptoms
- `GET /api/plugins` hangs (curl timeout after 10s)
- `GET /settings` hangs
- All other endpoints work fine (health, check-db, check-list)

### Location
`src/plugin/mod.rs` → `fn resolve_env_var(value: &str) -> String`

### Root Cause
When a plugin manifest references an env var that is not set (e.g., `${TELEGRAM_BOT_TOKEN}`), the fallback logic produces the **exact same string** as the match, causing an infinite loop:

```rust
// Buggy code:
let replacement = std::env::var(var_name)
    .unwrap_or_else(|_| format!("${{{}}}", var_name));
// When var is missing: "${TELEGRAM_BOT_TOKEN}" → "${TELEGRAM_BOT_TOKEN}" — IDENTICAL!
result.replace_range(start..start + end + 1, &replacement);
// while loop: finds "${" again → INFINITE LOOP
```

### Detected Via
All plugins were enumerated via DB query. The `telegram` plugin had:
```
env: { "TELEGRAM_BOT_TOKEN": "${TELEGRAM_BOT_TOKEN}" }
```
And `TELEGRAM_BOT_TOKEN` was not set in the container environment.

### Fix
```rust
match std::env::var(var_name) {
    Ok(val) => result.replace_range(start..start + end + 1, &val),
    Err(_) => break,  // Var not set — stop to prevent infinite loop
}
```

**Don't put a `${VAR}` literal back if the var is not set** — break out of the loop immediately.

---

## Bug #2: Runtime-Wide Deadlock via `handle.block_on()`

### Symptoms
- After ~10-30s of uptime, **ALL** endpoints stop responding, including `/health`
- No panic or error in logs — the process is simply stuck
- Server appears dead but container is still running

### Location
15+ occurrences across 4 MCP tool files:
- `src/mcp/tools/cron.rs` — 4 uses
- `src/mcp/tools/kanban.rs` — 5 uses
- `src/mcp/tools/search.rs` — 2 uses
- `src/mcp/tools/plugin_manager.rs` — 3 uses

### Root Cause
The `McpTool` handler type is **synchronous** by design:
```rust
pub handler: Arc<dyn Fn(Value, AppContext) -> Result<McpToolResult> + Send + Sync>
```

Tools needing async DB operations used `handle.block_on()` to bridge sync→async:
```rust
let handle = tokio::runtime::Handle::current();
handle.block_on(async {
    plugin::list_plugins(&pool).await
})
```

`block_on()` blocks the **current thread** entirely. On a multi-threaded tokio runtime with only 2 workers, two concurrent `block_on` calls block both threads → **all tasks are starved**, including the HTTP server.

### Fix
Changed `McpRegistry::execute()` from synchronous to async, and offloaded tool execution to `tokio::task::spawn_blocking()`:

```rust
// Old (sync, deadlock-prone):
pub fn execute(&self, call: &McpToolCall, ctx: AppContext) -> Result<McpToolResult> {
    let tool = self.get(&call.name)?;
    (tool.handler)(call.arguments.clone(), ctx)
}

// New (async, safe):
pub async fn execute(&self, call: &McpToolCall, ctx: AppContext) -> Result<McpToolResult> {
    let tool = self.get(&call.name)?.clone();
    tokio::task::spawn_blocking(move || (tool.handler)(args, ctx)).await?
}
```

Blocking threads are **designed for blocking operations** — `block_on` on a blocking thread parks the thread safely and lets the tokio runtime use worker threads for async work.

---

## Bug #3: Blocking File I/O on Async Runtime

### Locations
| File | Line(s) | Operation |
|------|---------|-----------|
| `src/server/settings.rs` | 89, 120 | `std::fs::read_to_string`, `std::fs::write` |
| `src/agent/mod.rs` | 1001, 1006 | `std::fs::read_dir`, `std::fs::read_to_string` |
| `src/server/mod.rs` | 382, 387 | `std::fs::read_dir`, `std::fs::read_to_string` |

### Fix
Wrap in `tokio::task::spawn_blocking()`:
```rust
// Instead of:
let content = std::fs::read_to_string(&path);

// Use:
let env_path = path.clone();
tokio::task::spawn_blocking(move || std::fs::read_to_string(&env_path)).await
```

---

## Bug #4: Hardcoded Mattermost SiteURL

### Location
`docker-compose.mattermost.yml`

### Root Cause
```yaml
MM_SERVICESETTINGS_SITEURL: http://localhost:12340
```
Hardcoded to localhost instead of the actual external domain.

### Fix
```yaml
MM_SERVICESETTINGS_SITEURL: ${MATTERMOST_SITE_URL:-https://hermes-app-5.nexuslbs.org}
```
Set via `.env` file: `MATTERMOST_SITE_URL=https://hermes-app-5.nexuslbs.org`

---

## Detection & Debugging Methodology

The hang was isolated using a **diagnostic module** (`src/server/diagnostic.rs`) with endpoints that tested each layer individually:

| Endpoint | What It Tests | Result |
|----------|--------------|--------|
| `/api/plugins/ping` | Route registration, no state | ✅ pong |
| `/api/plugins/check-state` | State extraction | ✅ state ok |
| `/api/plugins/check-db` | DB pool health | ✅ db ok: 1 |
| `/api/plugins/check-list` | `plugin::list_plugins()` call | ✅ 12 rows |
| `/api/plugins/check-enrich` | enrich + json! construction | ❌ hung — infinite loop |

This proved the issue was in `enrich_plugin` (the step between `list_plugins` and JSON serialization), not in route registration, State extraction, or DB connectivity.

## Firm Rules Going Forward

1. **NO `handle.block_on()` from tokio worker threads** — use `spawn_blocking` or make the function async.
2. **NO `std::env::var()` with a fallback that can produce the same `${VAR}` string** — breaks out of the resolution loop.
3. **NO `std::fs::read_to_string()` directly in async tasks** — always wrap in `spawn_blocking`.
4. **ALL docker-compose environment-specific settings must use `${VAR_NAME:-default}`** pattern.
