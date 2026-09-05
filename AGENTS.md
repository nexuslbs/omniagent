# OmniAgent: AGENTS.md

## Prompt Architecture: HARD RULE: ZERO PROMPT LOGIC IN OMINAGENT

### Core Principle
**The prompt plugin configured in settings is the SINGLE SOURCE OF TRUTH for ALL prompt generation.** Omniagent must never build, assemble, or generate prompts inline. No in-process prompt builder, no direct file reads for prompt assembly, no inline planning prompt strings.

### Contract: Prompt Plugin `generate` Tool Returns Parts

The `generate` tool MUST return a structured object with 5 fields: NOT a single concatenated string:

```json
{
  "system": "identity + tool guidance + profile hint + platform hint",
  "memory": "MEMORY.md content (read by the plugin from filesystem)",
  "soul": "system message override from settings",
  "context": "thread messages + summaries + skills + subtasks",
  "user": "the user's actual message"
}
```

| Field | Source | Purpose |
|-------|--------|---------|
| `system` | Plugin-defined | Stable identity, tool rules, profile/platform hints |
| `memory` | Plugin reads from disk | MEMORY.md content from profile |
| `soul` | Passed by omniagent | Optional system message override |
| `context` | Plugin queries DB | Thread history, summaries, skills, subtasks |
| `user` | Passed by omniagent | The user's message (or planning instruction) |

Omniagent is responsible ONLY for assembly: it receives these 5 parts and formats them into the message array for the LLM.

### Plan Resolution

**Plan mode** determines whether the agent runs a planning phase before the main execution loop. Planning creates a structured plan as a message (msg_type=`plan`), optionally generates subtasks from JSON plans, and guides the main loop.

#### Plan Resolution Priority

The plan boolean for a thread is resolved at creation time through a multi-level chain:

| Priority | Source | Description |
|----------|--------|-------------|
| 1 (highest) | `task_plan` | Explicit override from external client (platform plugins: mattermost, telegram) or cron/kanban scheduler. Passed as `ThreadCauseParams.task_plan`. |
| 2 | channel `plan` column | DB column on `channels` table. Set via `PATCH /api/channels/{id} {"plan": false}`. Accessed via `get_channel_plan()` function. |
| 3 (deprecated) | channel `metadata["plan"]` | Legacy JSON field for backward compatibility. Only used if the DB column is NULL. |
| 4 (fallback) | Prompt plugin decides | When neither task_plan nor channel plan is set, the prompt plugin decides at runtime. The builtin prompt plugin uses heuristic (content length, complexity). |

In code: `resolve_thread_plan()` in `db/threads.rs`:
```rust
// Priority: task_plan > channel_plan (column > metadata) > None (plugin decides)
let channel_plan = channel_plan_from_column.or(channel_plan_from_metadata);
resolve_thread_plan(channel_plan, task_plan)
// → Some(task_plan) if set
// → Some(channel_plan) if set
// → None (let plugin decide)
```

#### Prompt Plugin Interaction

The prompt plugin (`prompt_generate` tool) receives the resolved plan value as input and **may override it**:

1. Thread is created with `plan` from resolution chain
2. Executor calls `prompt_generate` with `"plan": thread.plan` in the arguments
3. Prompt plugin returns `{ "plan": true|false, ... }` in its response
4. If the response includes a `plan` field, the thread's plan is **updated in the DB**
5. `should_plan = thread.plan` determines whether the planning phase runs

The **builtin prompt plugin** behavior:
- If channel plan is explicitly set (`true` or `false`): respects it, returns the same value
- If channel plan is `None` (not set): decides based on content complexity, returns its decision
- The decision is persisted to the thread so subsequent checks use the resolved value

#### Configuration via API

Channel-level plan can be set via `PATCH /api/channels/{id}`:
```json
{"plan": false}
```

Global settings that affect planning behavior (via `PUT /settings`):
- `MAX_ITERATIONS_PLAN`: Max tool-call iterations when plan mode is active (default: 120)
- `MAX_ITERATIONS_NO_PLAN`: Max iterations without planning (default: 30)
- `PROMPT_GENERATE_TOOL`: MCP tool name for prompt generation (default: `"prompt_generate"`)

#### Mermaid Diagram

```mermaid
flowchart LR
    subgraph Thread_Creation["Thread Creation"]
        A[External Client] -->|task_plan: Some(false)| B[create_thread_with_cause]
        C[channels.plan column] -->|get_channel_plan| D{resolve_thread_plan}
        E[metadata['plan']] -->|deprecated fallback| D
        B --> D
        D -->|plan: false| F[Thread created with plan=false]
    end

    subgraph Execution["Execution"]
        F --> G[Executor: prompt_generate<br>receives plan=false]
        G --> H[Prompt plugin returns<br>{plan: false, ...}]
        H --> I[DB: UPDATE threads<br>SET plan = false]
        I --> J{should_plan = false}
        J -->|false| K[Skip planning phase]
        J -->|true| L[Run planning phase]
    end

    subgraph Plugin_Restart["Mattermost Platform Plugin"]
        M[WS reconnect] --> N[init_channel_cursor: finds<br>latest HUMAN post timestamp]
        N --> O[poll_channel: processes posts<br>with create_at > cursor]
        O -->|missed message found| B
    end
```

### Dashboard Prompt Preview

The `/prompt-preview/{channel_name}` endpoint MUST call the active prompt plugin via MCP `generate` to get the parts, then display them. It MUST NOT read MEMORY.md/USER.md directly or assemble prompts inline.

### No In-Process Fallback

If the prompt plugin's `generate` call fails, propagate the error. Do NOT fall back to in-process prompt building: no fallback exists.

### What This Eliminates

| What | Where it was | Status |
|------|-------------|--------|
| `src/prompt_builder.rs` | omniagent core | DELETED |
| `src/mcp/prompt_tools.rs` | omniagent MCP | DELETED |
| `src/agent/executor.rs` inline planning | Lines 487-523 | MUST BE REMOVED: use parts approach |
| `prompt_preview_handler` inline MEMORY.md read | `src/server/mod.rs` | MUST BE REMOVED: call MCP plugin |
| `build_thread_context` direct call | Both executor + preview | Can remain as a utility, but must NOT be the sole source of context: context comes from the plugin's generate tool |
| `prompt-tools` crate | Workspace member | DELETED (merged into plugin) |

### Plugin Discovery (`info` tool)

The prompt plugin SHOULD expose an `info` tool that returns:
```json
{
  "parts": ["system", "memory", "soul", "context", "user"],
  "capabilities": {"planning": false},
  "version": "1.0"
}
```

Omniagent calls `info` to discover what the plugin can provide.

## Plugin System Rules & Conventions

### Core Principle
The **source** field in `plugins.yml` is authoritative: it determines which binary/source to use. No more `builtin: bool` or `remote: {...}` guessing.

A plugin **can** exist at multiple sources simultaneously (e.g., a builtin crate in omniagent AND a bundled copy in omni-stack). The `source` field unambiguously identifies which one to act on.

**At most one source can be enabled per plugin name.** Enabling a different source overwrites the YAML entry for that name.

### Plugin Config: HARD RULE: Use Plugin Config, NOT Direct Env Vars

**Plugins MUST use their own plugin config (`config_schema` in `plugin.json`) for ALL configurable values.** Plugins may reference environment variables via `$env:VAR_NAME` as default values in `config_schema`, but the runtime value must come from the plugin's resolved config (which the plugin system provides via the `config` field).

**Correct pattern:**
```json
// plugin.json config_schema
"config_schema": [
  {
    "key": "MY_PARAM",
    "label": "My Parameter",
    "type": "string",
    "default": "$env:MY_ENV_VAR",
    "description": "..."
  }
]
```

The plugin reads from its resolved config at startup, not by calling `std::env::var("MY_PARAM")` directly. The plugin system resolves `$env:` references automatically.

**Incorrect: do NOT do this:**
```rust
// ❌ Plugin reads env var directly
let value = std::env::var("MY_PARAM").unwrap_or_default();
```

**Exception:** The core omniagent process (not a plugin) may still read env vars directly for settings that are global to the agent process. But plugins must use plugin config.

This rule applies to ALL plugin types: tools, platforms, and providers.

### Configuration Files (omni-stack)

| File | Purpose |
|------|---------|
| `plugins.yml` | Unified config: replaces old tools.yml/platforms.yml/providers.yml |
| `remote.yml` | Remote plugin metadata (URL, path, ref): versioned in git |

`plugins.yml` format:
```yaml
platforms:
  mattermost:
    enabled: true
    source: bundled
    config: { ... }
tools:
  cron:
    enabled: true
    source: built-in
    config: {}
  test-rust-tool:
    enabled: false
    source: remote
    config: {}
```

`remote.yml` format:
```yaml
tools:
  test-rust-tool:
    url: https://github.com/nexuslbs/omni-plugins.git
    path: tools/test-rust-tool
```

### Source Determination: HARD RULE: NO PRIORITY, NO FALLBACK

A plugin's **source** is determined **solely by its physical location on disk**. There is no priority order between built-in, bundled, and remote. Each stands independently:

| Source | Physical Location | Identified By |
|--------|------------------|---------------|
| `built-in` | `/app/plugins/{type}/{name}/` | `Cargo.toml` + `plugin.json` or `mcp-config.json` in workspace |
| `bundled` | `{data_dir}/plugins/{type}/{name}/` or `{workspace_dir}/plugins/{type}/{name}/` | `plugin.json` at root |
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/{path}/` | `plugin.json` at subpath + entry in `remote.yml` |

**The `source` field in `plugins.yml` is authoritative.** When a plugin has a YAML entry with `source: built-in`, only the built-in source is active. The bundled and remote sources for the same name still exist on disk but are marked `is_duplicated: true` and shown as disabled.

**When there is no YAML entry**, all sources are discovered and shown as disabled. The user can enable any source via the dashboard, which creates a YAML entry with that source.

**No function should guess or fall back between sources.** The `detect_plugin_category_cross_type()` function returns `None` when no YAML entry exists: it does NOT pick a source. Each caller (install handler, enable handler, etc.) has its own source-specific logic.

**MCP scanner (`discover_plugin_servers`) is source-aware:** It reads `plugins.yml` and only starts MCP servers for enabled plugins at their correct source location. It does NOT scan all directories blindly.

**Plugin discovery (`discover_plugins`) scans ALL directories:** Sections A-D scan every physical location so ALL discoverable plugins appear in the dashboard listing. Plugins not in `plugins.yml` default to `status: "disabled"`.
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/{path}/` | Standalone: `cargo build` from `.remote/{name}/{path}/Cargo.toml` | `{dir}/target/release/{pkg_name}` |

### Builtin Plugin Rules

- **Builtin plugins are disabled by default.** They must be explicitly added to `plugins.yml` with `enabled: true` and `source: built-in`.
- **If a tool/plugin is defined in YAML** with `source: bundled` or `source: remote` and a builtin with the same name exists, the builtin is ignored: the non-builtin source is the primary. The builtin still shows as an available source but marked as duplicated.
- **When a builtin plugin has a YAML entry but no explicit `source` field**, it defaults to `built-in` but appears as disabled if enabled=false.
- **Builtin plugins** are workspace members in `/app/Cargo.toml`.
- **Only plugins with `plugin.json` at directory root** are considered local/repo plugins. Directories without `plugin.json` (e.g., config-only dirs like `util`) should not appear as discoverable plugins.
| **Duplicated plugins in the tools page**: When a plugin exists both as builtin (in omniagent `/app/plugins/`) and bundled (in omni-stack `plugins/`), the non-primary source shows as "duplicated" in the dashboard. The omni-stack copy usually takes precedence unless the YAML explicitly sets `source: built-in`.
| **No hardcoded built-in list in frontend**: BUILT_IN_TOOLS was removed (2026-07-07). All tools come from the backend's `/api/plugins` endpoint. The frontend no longer hardcodes "actions" or any other plugin: the backend discovers everything.
| **`util` and similar config-only directories**: Directories without `plugin.json` at root are NOT discoverable as plugins. A dir like `util` (which only has Cargo.toml or config files, no plugin.json) should not appear in the /tools page unless explicitly defined in plugins.yml.

## Plugin Identity: [type + source + name] is the Composite Key

**A plugin's identity is `[type, source, name]`, NOT `name` alone.** Platforms, tools (MCP servers), and providers are entirely different things even when they share the same name. The name `test-python` can refer to a platform plugin, a tool plugin, and a provider plugin simultaneously - each is a distinct entity with its own configuration, lifecycle, and state.

**All plugin lookups MUST use the composite key `[type, source, name]`:**

```rust
// ✅ CORRECT - unambiguous, type-aware
plugins_yaml::get_plugin(data_dir, name, &PluginYamlType::Tool)

// ❌ WRONG - ambiguous, mixes types
// get_plugin(data_dir, name)  - no type parameter (DEPRECATED)
```

**API routes follow `/{type}/{source}/{name}/{action}` where type is required:**
| Route | Example |
|-------|---------|
| `/{type}/{source}/{name}/enable` | `/plugins/platforms/bundled/mattermost/enable` |
| `/{type}/{source}/{name}/disable` | `/plugins/tools/remote/test-rust-tool/disable` |
| `/{type}/{source}/{name}` | `/plugins/providers/built-in/noop` |

**Type parameter (`type` in URL, `pt` in code) MUST be one of:** `platforms`, `tools`, `providers`.

**No function should look up a plugin by name alone.** Every function that takes a plugin identifier MUST also receive either:
- A `PluginYamlType` enum (for Rust code)
- A `plugin_type` filter in API response data (for Python/frontend code)

Python/frontend code filtering plugin lists MUST include `plugin_type` in the filter:
```python
# ✅ CORRECT
next((p for p in plugins if p["name"] == name and p.get("plugin_type") == "platform"), None)

# ❌ WRONG - will find wrong plugin if types share a name
# next((p for p in plugins if p["name"] == name), None)
```

### Bundled Plugin Rules (Omni-Stack)

- Bundled plugins live in `{workspace_dir}/plugins/{type}/{name}/`.
- They are considered "local/repo plugins" only if they have a `plugin.json` in the directory root.

### Display Rules (Tools Page)

The `/api/plugins` response groups plugins by name and assigns a **primary source** based on YAML.
`is_duplicated` is determined by `pick_primary_source()` in `plugins_yaml.rs`:

1. **YAML entry exists** with `source: X` → source X is primary (`is_duplicated=false`). Other sources with same name get `is_duplicated=true`.
2. **YAML entry exists** but source not on disk → fallback to priority: built-in → bundled → remote.
3. **No YAML entry + 2+ sources** with same name → **no primary**. All sources get `is_duplicated=true`.
4. **No YAML entry + single source** → `is_duplicated=false` (no other source to conflict with).

**Key behavior change (2026-07-07):** When there is no YAML entry, `pick_primary_source()` returns `None`, and `is_duplicated` is set to `group.sources.len() > 1`: meaning all sources in a multi-source group show as duplicated. This ensures the YAML-configured source is always the authority; without YAML, all sources are equal.

**Enabling a source** (via dashboard or API) creates a YAML entry with that `source`, making it primary and marking all others as duplicated.

### Plugin Action Buttons (Dashboard: tools.ts)

Action buttons are determined by `renderActionButtons()` based on the plugin's source, build state, and type. The `is_duplicated` flag does NOT suppress buttons: duplicated sources with source code are still actionable.

**Remove button rule:** Remove (`plugin-delete-btn`) shows for non-builtin plugins when the plugin is NOT installed (needs_build=true) OR is a script plugin. For installed Rust plugins, use Uninstall instead.

| Scenario | `hasRemote` | `hasCompilableSource` | `needsBuild` | Buttons |
|----------|-------------|-----------------|---------------|---------|
| Remote script/no-source | ✅ | ❌ | N/A | **Remove + Update** |
| Remote Rust, not yet built | ✅ | ✅ | ✅ | **Remove + Install + Update** |
| Remote Rust, already built | ✅ | ✅ | ❌ | **Uninstall + Reinstall + Update** |
| Bundled script/no-source | ❌ | ❌ | N/A | **Remove** |
| Bundled Rust, not yet built | ❌ | ✅ | ✅ | **Install + Remove** |
| Bundled Rust, already built | ❌ | ✅ | ❌ | **Reinstall + Uninstall** |
| Built-in script/no-source | ❌ | ❌ | N/A | *(no buttons)* |
| Built-in Rust, not yet built | ❌ | ✅ | ✅ | *(no buttons)* |
| Built-in Rust, already built | ❌ | ✅ | ❌ | *(no buttons)* |

**Button actions:**
- **Remove** (`plugin-delete-btn`): Calls `DELETE /api/plugins/{name}`: removes YAML entry
- **Install** (`plugin-install-btn`): Calls `POST /api/plugins/{name}/install`: compiles + registers
- **Uninstall** (`plugin-remove-btn`): Calls `DELETE /api/plugins/{name}?mode=uninstall`: removes binary + disables
- **Reinstall** (`plugin-reinstall-btn`): Calls `POST /api/plugins/{name}/reinstall`: recompiles binary
- **Update** (`plugin-update-btn`): Calls `POST /api/plugins/{name}/download`: re-clones from git + recompiles (remote only)
- **Enable/Disable** (`plugin-toggle-btn`): Calls `POST /api/plugins/{name}/enable` or `/disable`

**Update vs Reinstall vs Install:**
- **Update** (remote only): re-clones from git repository (removes existing clone, fresh shallow clone), then recompiles if Rust
- **Reinstall**: recompiles the existing source code on disk (no git pull)
- **Install**: compiles from existing source and registers in YAML

### Plugin Display Rules (Dashboard: backend data)

### Plugin Discovery Rules

- `.remote/` directories contain remote plugin clones. Plugins inside `.remote/` with `plugin.json` at root are discovered as remote sources.
- Plugins cloned with a `path` sub-path (e.g., `path: tools/cron-echo`) are in a subdirectory within `.remote/{name}/{path}/`.
- Stale/old plugin directories in the workspace (non-.remote copies, mcp/ dirs, temp clones) should be cleaned up. They create false "bundled" or "duplicated" entries.
- The `remote.yml` must have entries that match the `.remote/` directory structure. Orphan `.remote/` directories (no remote.yml entry) are ignored.

### Install / Reinstall with Builtin Fallback

When Install/Reinstall is called and the categorized source directory has no Cargo.toml (only pre-compiled binary), the handler falls back to the builtin source.

### Shared Plugin Resolution (Install/Reinstall)

## Plugin Action Handlers: Type+Source from URL Path (HARD RULE)

### Core Principle
**Every plugin action handler that has `{type}/{source}/{name}` in the URL path MUST use those values from the path.** No guessing, no fallbacks to `plugins.yml`, `remote.yml`, or disk directories.

### Source Resolution Rules

| URL source | Behavior | Directory Construction |
|-----------|----------|----------------------|
| `remote` | Read `remote.yml` to get sub-path | `{data_dir}/plugins/{type_dir}/.remote/{name}/{sub_path}` |
| `bundled` | Construct directly | `{data_dir}/plugins/{type_dir}/{name}` |
| `built-in` | Disallowed for install/reinstall/delete/rename/download | Only enable/disable allowed (handled by `reject_builtin_operation()`) |
| any other | `BAD_REQUEST` error | |

**The `install-git` endpoint** (`POST /api/plugins/install-git`) has no type/source in URL: it always works for remote plugins.

### What Was Removed

| Function | Replaced By |
|----------|-------------|
| `detect_plugin_category()` | Direct source matching from URL path |
| `detect_plugin_category_cross_type()` | `PluginYamlType::from_type_str(&p_type)` |
| `get_plugin_dir_for_category()` | Direct path construction |
| `get_entry_with_type()` in download/rename | `get_remote_plugin(data_dir, &yaml_type, name)` |
| `load_remote_plugins()` type iteration in rename | `PluginYamlType::from_type_str(&p_type)` |

### Affected Endpoints

| Endpoint | Handler |
|----------|---------|
| `POST /api/plugins/{type}/{source}/{name}/install` | `install_plugin_handler` |
| `POST /api/plugins/{type}/{source}/{name}/reinstall` | `reinstall_plugin_handler` |
| `POST /api/plugins/{type}/{source}/{name}/download` | `download_plugin_handler` |
| `POST /api/plugins/{type}/{source}/{name}/rename` | `rename_plugin_handler` |
| `DELETE /api/plugins/{type}/{source}/{name}` | `delete_plugin_handler` (uninstall + remove modes) |
| `GET /api/plugins/{type}/{source}/{name}` | `get_plugin_handler` |
| `POST /api/plugins/{type}/{source}/{name}/enable` | `enable_plugin_handler` |
| `POST /api/plugins/{type}/{source}/{name}/disable` | `disable_plugin_handler` |

### The `resolve_plugin_for_compile()` Function

The shared preamble for Install and Reinstall now uses deterministic type+source from the URL path:

```rust
pub(crate) async fn resolve_plugin_for_compile(
    data_dir: &str,
    plugin_type: &str,      // from URL path: "tools"|"platforms"|"providers"
    source: &str,            // from URL path: "remote"|"bundled"|"built-in"
    name: &str,
    handler_name: &str,
) -> Result<ResolvedPlugin, ...>
```

The `resolve_plugin_for_compile()` function extracts the common preamble from both Install and Reinstall handlers. As of July 2026, it uses type+source from the URL path deterministically:

- Plugin type is parsed from the URL path via `PluginYamlType::from_type_str(plugin_type)`
- For `source = "remote"`: reads `remote.yml` to get the sub-path for directory resolution
- For `source = "bundled"`: constructs dir as `{data_dir}/plugins/{type_dir}/{name}`
- For any other source: returns `BAD_REQUEST`
- Verifies the plugin directory exists on disk; returns `NOT_FOUND` if not
- Returns `ResolvedPlugin` struct with `yaml_type`, `category`, `plugin_dir`

No `detect_plugin_category()`, no `get_plugin_dir_for_category()`, no `get_entry_with_type()` - all replaced by deterministic path construction from URL parameters.

## External Platform Plugin Client - Race Condition Prevention

### Core Problem: `tokio::sync::Notify` Stale Notification Bit

The external platform plugin runner (`src/platform/external/client.rs`) uses `tokio::sync::Notify` to signal restart/stop events from the API to the subprocess's inner event loop. This mechanism has a fundamental race condition:

**`tokio::sync::Notify` stores exactly one stale notification bit.** If `notify_one()` is called when no task is waiting on `notified()`, the notification is stored. The next `notified().await` resolves immediately, even if the event that produced it was already handled via a different mechanism (counter comparison).

This caused the mattermost subprocess (and would cause ANY external platform subprocess) to be killed immediately on spawn, preventing the WebSocket from ever establishing.

### Fix: Two-Pronged Defense

```rust
// FIX 1: Inner loop - ignore stale notifications when counters match
_ = self.restart_notify.notified() => {
    let current_restart = self.restart_count.load(Ordering::SeqCst);
    // If restart count hasn't changed since spawn, the notification
    // bit is stale - don't kill the subprocess
    if current_restart == last_restart_count {
        continue;  // ← KEY: skip break, keep subprocess alive
    }
    // Genuine new restart: break inner loop
    break;
}

// FIX 2: Before respawn - consume stale notification bit proactively
if current_restart != last_restart_count {
    last_restart_count = current_restart;
    // Consume the pending notification so the next spawn's
    // inner loop doesn't fire on it
    self.restart_notify.notified().await;  // ← safe: we know a notification is pending
    continue;
}
```

**FIX 1** (inner loop guard) is the primary defense: it prevents killing a healthy subprocess when a stale notification arrives.

**FIX 2** (pre-respawn consume) is the optimization: it prevents the stale notification from ever reaching the inner loop in the first place.

Both fixes apply to ALL external platform plugins (mattermost, telegram, etc.), not just the one that originally exhibited the bug.

### Additional Fragility Fixes

| Issue | Fix | Location |
|-------|-----|----------|
| `self.process.lock().unwrap()` panics on poisoned lock | Changed to `match self.process.lock() { Ok(guard) => ..., Err(e) => return Err(...) }` | Line 360 |
| Stderr from subprocess was discarded | Changed `stderr(Stdio::null())` to `stderr(Stdio::inherit())` | `spawn_plugin()` |

### Key Rules for Future Development

1. **`tokio::sync::Notify` is single-bit.** It stores at most one notification. Counter-based detection (via `AtomicU64`) is the reliable mechanism; `Notify` is only for waking the waiting task. Always validate the counter before acting on a notification.

2. **Consume notifications before respawn.** When a restart is detected via counter comparison, consume any pending `Notify` bit with `notified().await` before spawning the new subprocess. Otherwise the stale bit will fire in the new subprocess's event loop.

3. **Handle lock poisoning defensively.** `StdMutex` and `RwLock` can become poisoned if another task panics while holding the lock. Use `match lock() { Ok(g) => ..., Err(_) => fallback }` instead of `.unwrap()`.

4. **External platform plugin lifecycle is shared code.** All platforms (mattermost, telegram, etc.) share the same `ExternalPlatformClient` in `client.rs`. A bug fix for one platform fixes it for all. Do NOT add platform-specific hacks in `client.rs`.

The integration tests in `omni-stack/scripts/tests.py` were hardened:

- **Fixed broken regex** in `target_dir_exists()`: `\\\\s+` → `\\s+` (matched literal `\s` instead of whitespace: the function always returned False for remote plugins with indented YAML)
- **Added binary-absence check** after Uninstall: `assert_eq(binary_exists(name, plugin_type), False)`: this is the critical assertion that would have caught the subpath `target/` bug
- **Made functions type-aware**: `binary_exists()`, `target_dir_exists()`, `install_plugin()`, `uninstall_plugin()`, `add_remote_plugin()`, `test_rust_tool()` all accept a `plugin_type` parameter ("tools", "platforms", "providers")
- **Full lifecycle verification** for each operation:
  - Install: binary exists, needs_build=False, status=enabled, no background_compile
  - Uninstall: binary gone, target/ removed, .remote/ preserved, MCP tools deregistered, YAML has enabled=false
  - Remove: .remote/ preserved (source kept)
  - Download: remote.yml preserved
  - Enable/Disable: YAML content verified
  - Reinstall: binary still exists after recompile

### Git Install (install-git)

- **API**: `POST /api/plugins/install-git`: clones a plugin repo and persists to `remote.yml` only.
- Does NOT compile or register in `plugins.yml`. 
- The dashboard handles Install (compile + YAML entry), Enable, Remove as separate steps.
- Directory naming priority: explicit `name` → last segment of `path` → repo name from URL, sanitized with `sanitize_plugin_name()`.
- Clone destination: `{data_dir}/plugins/{type}/.remote/{name}/`

### Rename Plugin

- **API**: `POST /api/plugins/{name}/rename` with body `{ "new_name": "..." }`
- Updates all three locations atomically:
  1. Renames directory: `plugins/{type}/.remote/{old_name}/` → `plugins/{type}/.remote/{new_name}/`
  2. Updates `remote.yml` key: removes old key, adds new key with same URL/path/ref
  3. Updates `plugins.yml` key (if YAML entry exists): removes old key, inserts new key with same enabled/source/config
- Returns 404 if plugin not found in `remote.yml`
- Returns 409 if `new_name` already exists in `remote.yml` for the same type
- New name is sanitized with `sanitize_plugin_name()` before use

### Remote Plugin Store (remote.yml)

Remote plugin info is persisted in `{data_dir}/remote.yml` (root-level, replaces old `.remote/plugins.yml`).

**Key Behaviors:**
- **On git install**: Writes to `remote.yml` via `save_remote_plugin()`
- **On enable with remote source**: Reads from `remote.yml` for re-enabling
- **On delete**: Cleans up `remote.yml` via `remove_remote_plugin()`
- **Plugin listing**: Remote sources resolved via `get_remote_plugin()`

### "Not Found" Status

When a plugin exists in `plugins.yml` but has no source on disk, a synthetic "not found" entry is added:
- `status: "not_found"`: red badge in dashboard
- `needs_download: true`: for remote plugins not yet cloned

### API Type Change

- `plugin_type` in API responses uses `"tool"` instead of `"mcp"` (backward compat maintained via `from_type_str` mapping)
- Enable/disable endpoints require `{ source: "built-in" | "bundled" | "remote" }`

### Reinstall Behavior

- **Reinstall does NOT re-clone the git repository** for remote plugins. It only recompiles the existing source code in `.remote/<name>/`.
- To update from git (re-clone the latest version), use the **Download** endpoint (`POST /api/plugins/{name}/download`) instead.

### Uninstall Behavior

- **Uninstall does NOT remove the `.remote/` directory** for remote plugins. It only:
  1. Removes the compiled `target/` directory (`{data_dir}/plugins/{type}/.remote/{name}/target`)
  2. Sets `enabled: false` in `plugins.yml` (keeps the YAML entry and `.remote/` source code)
  3. **Stops the MCP server** via `clear_server_pools()` + `remove_server_config()` + `remove_by_server()`: without this, the MCP tools remain registered in `/mcp/tools` even though YAML says `enabled: false`
- For non-remote plugins, uninstall removes the YAML entry and the compiled `target/` directory, and also stops the MCP server.
- Same MCP server cleanup applies to the default **Remove** mode.

### Download Handler Must Preserve Enabled State

The `download_plugin_handler` (`POST /api/plugins/:name/download`) previously hardcoded `enabled: false` when rewriting the YAML entry after re-cloning from git. This caused every download to disable the plugin.

**Fix applied July 2026:** Reads the current `enabled` state from the existing YAML entry before writing:
```rust
let current_enabled = plugins_yaml::get_entry(data_dir, &yaml_type, &name)
    .ok().flatten()
    .map(|e| e.enabled)
    .unwrap_or(true);
```

### Bundled Plugin Buttons (Dashboard)

See "Plugin Action Buttons" table above for full rules. Key bundled specifics:
- **Bundled script/no-source**: Remove button only (runs directly, no compilation needed).
- **Bundled Rust, not yet installed**: Install + Remove.
- **Bundled Rust, installed**: Reinstall + Uninstall (no Remove: it's installed, use Uninstall instead).
- There is no Update button for bundled plugins (the code lives in the omni-stack repo, not an external git repo).
- The Remove button calls `DELETE /api/plugins/{name}` (remove mode), which removes the YAML entry and the compiled `target/` directory.
- The Install button for bundled plugins compiles synchronously, writes `enabled: true` to `plugins.yml`, and hot-reloads the MCP server: all in one synchronous API call. No more background compile.

### Remove API Behavior (DELETE /api/plugins/:name)

The Remove handler (`delete_plugin_handler`) follows strict source-based rules (rewritten August 2026).

**Core detection order:**
1. **YAML entry**: `plugins.yml` source field (built-in / bundled / remote)
2. **Disk state**: built-in on disk (`/app/plugins/`), bundled on disk (`workspace_dir/plugins/`), or remote in `remote.yml`

**Rules (applied in priority order):**

| Condition | Action | YAML Effect | Disk Effect |
|-----------|--------|-------------|-------------|
| Built-in on disk + no YAML entry | **Error** | None | None |
| Built-in on disk + YAML source=built-in | **Error** | None | None |
| YAML source=built-in + NOT on disk | Remove YAML entry | Entry deleted | None |
| YAML source=remote (or in remote.yml) | Remove all remote | YAML entry deleted (if source matches) | `.remote/` dir + remote.yml entry |
| YAML source=remote + bundled disk exists | Remove disk only | YAML entry PRESERVED (source mismatch) | Workspace dir removed only |
| YAML source=bundled (or disk as bundled) | Remove bundled | YAML entry deleted (if source matches) | Workspace dir + data dir removed |
| YAML source=bundled + remote in remote.yml | Remove disk only | YAML entry PRESERVED (source mismatch) | Workspace dir removed, `.remote/` preserved |
| YAML entry exists + no disk source | Remove YAML only | Entry deleted | None |
| No YAML + no disk | **No-op** (success) | None | None |

**Key behaviors:**
- **`remote.yml` is the single source of truth** for remote plugin detection. The `.remote/` directory contents are irrelevant: if a plugin name exists in `remote.yml` (loaded via `load_remote_plugins()`), it's treated as remote. No walking of `.remote/` directories needed.
- **Source mismatches preserve YAML** intentionally. If YAML says `source: bundled` but the plugin is listed in `remote.yml`, removing the plugin deletes the remote files (`remote.yml` entry + `.remote/` dir) but keeps the YAML entry intact. The YAML now correctly points to the bundled source (even if not yet present on disk).
- **Built-in plugins cannot be removed.** Attempting to remove a built-in plugin returns a 400 error: `"Cannot remove built-in plugin 'X'. Built-in plugins are part of the application and can only be disabled."`
- **MCP server cleanup** always runs when a `.remote/` directory or workspace plugin directory exists.
- **Provider and platform removal** works identically: the handler detects `yaml_type` from YAML entry or disk location.

**`list_plugins` filter change:** Any `enabled: false` YAML entry now suppresses ALL sources for that plugin name (removed source-matching requirement). This handles mismatched source types where YAML says `bundled` but disk source is `built-in`.

## Kanban Boards & Role-Based Workflows (recent architecture)

Kanban is driven by **boards** (`config/boards.yml`, optional - feature-gated on file presence) and **role-based workflows** (`config/workflows.yml`).

### workflows.yml

Each workflow defines the role lifecycle of a task. Roles run in sequence; each role creates a thread in its step:

```yaml
omniagent-dev:
  auto_approve: false          # true = skip review approval (executor-only workflows)
  review_on_fail: false        # true = testing failure routes to review, not back to executor
  clear_executions_on_review: true
  retries: 3                   # per-role default
  roles:
    executor: { template: dev-executor, mode: agent, action_id: null, plan_mode: on, retries: 9 }
    tester:    { template: dev-tester,    mode: agent, plan_mode: on }
    reviewer:  { template: dev-reviewer,  mode: agent, plan_mode: on }
```

- `mode: agent` runs a prompt template; `mode: action` runs a registered action by `action_id`.
- Task steps are recorded in `task_executions`; `clear_executions_on_review` wipes them when a task returns to review.
- Review verdicts via `POST /kanban/tasks/{id}/review` (approve / request changes). Reviewer rejects use `fail-thread` with `workflow_step="running"` to route the task back to a fresh executor thread.
- `auto_approve: true` + executor-only roles = self-contained dev-executor workflow (no tester/reviewer).

### boards.yml

- When `boards.yml` is present, task create/edit **requires** a valid board (API rejects missing/invalid board).
- Boards define defaults resolved **at load time** (task → board → channel → global settings): channel, workflow, plan, template, provider/model. Loaders return resolved data, never shallow values - do not re-resolve at execution time.
- The dispatcher enforces the **board gate** (per-board in-flight limits) and **channel-busy gate**, and **never dispatches archived tasks** (archived guard in the dispatch SQL).

## Event Hooks (`src/hooks.rs`)

Hooks are event-driven, fire-and-forget, and isolated from the triggering work (their failures never break the main flow):

- `thread_started` (fires after a thread is created, not for delegate messages)
- `thread_finished` (fires on every terminal transition)
- `new_message` (fires on message insert)

Hooks are delivered to the hooks channel (configured via `default_hook_channel` / channel `hooks` entries). See `config/tasks.yml` in omni-stack for the builtin hook task templates.

## Context Budgets (token-only, prompt plugin owns them)

- Budgets are **global settings** in `settings.yml`: `prompt_token_budget_soft` (100000) / `prompt_token_budget_hard` (500000). There are NO char budgets in core.
- The **prompt plugin's `compact-messages` tool owns compaction**: it receives soft/hard budget params at call time (resolved per provider/model; `chars/4` fallback when no tokenizer is available) and performs pruning (`prune_old_tool_results`) inside compaction.
- Core no longer has budget/prune logic - do NOT reintroduce char budgets or prune-elsewhere logic.
- `AgentConfig` fields are `token_budget_hard/soft` (read from those settings keys).

## models.yml Overrides

`config/models.yml` (in OMNI_DIR; absent = no behavior change):
- `providers.<name>.plugin: false` → plugin-less provider (no plugin code needed; builtin chat_completions/anthropic support).
- `providers.<name>.models` → replaces the plugin's `default_model.allowed_values` in selectors.
- Per-model `model_config.<model>` overrides take highest precedence. Budget precedence: `model_config.<model> > providers.<name> > global settings`.

## API Field-Name Parity (HARD RULE)

The HTTP API and YAML configs use the **same property names**: `channel`, `workflow`, `cron`, `board`, `provider`, `model` - NOT `channel_id`, `workflow_id`, `current_*`, or `schedule`. When adding/renaming API fields, keep YML parity; tests in omni-deployer `scripts/tests.py` (GROUP 39/46/47) assert this.

## Single-Instance Guard

`main.rs` acquires a **Postgres advisory lock** (`db::try_acquire_advisory_lock`, session-scoped) before startup cleanup. A second instance against the same database refuses to start rather than marking live threads skipped (zombie-executor bug). Do not remove or reorder this guard relative to `skip_on_startup`.

## Builtin `omniagent-api` Tool

The agent exposes its own HTTP API as the builtin MCP tool `omniagent-api` (internal self-fetch, no host/port config). It replaced the `kanban_*` / `cron_*` plugin tools. The `fetch` plugin gates unsafe HTTP methods via `allow_unsafe_methods` config.

## Verification Gates (before push)

```bash
cargo fmt
cargo check
cargo clippy -- -D warnings
cargo test --workspace --release
```

When a migration/query changes, regenerate the offline SQLx cache (`.sqlx/`) so `SQLX_OFFLINE` builds pass. Never commit scratch files (`*.patch`, `.task*`, `.push*`, `.smoke*`, `.g4x-*`, `_run_*.py`, `apply_*.py`) - scratch helper/driver scripts belong ONLY in `OMNI_DIR/data/scripts/` or `omni-stack/data/scripts/` (both gitignored, never versioned); never create them inside the repo tree.

    ## DB Write Guard: dev-built omniagent/migrations must NEVER write to the production DB (MANDATORY)

    Omniagent migrations are DECLARATIVE and auto-run at every startup
    (CREATE TABLE IF NOT EXISTS ...; there is no schema_migrations versioning).
    A dev-built binary pointed at the production postgres will silently create
    tables in the live DB before the feature is even committed.

    INCIDENT (2026-08-27/28): the kanban-tags feature executor ran its dev workflow
    (`cargo sqlx prepare` + live API verification) against the PRODUCTION omni-stack
    postgres from a dev container, creating `kanban_tags`/`task_tags` (+ a 'v1.0.0'
    tag row) in the prod DB before the feature was committed.

    RULES:
    1. **Dev verification (sqlx prepare, live API tests, migration application) runs
       against the omnidev stack DB ONLY** (project `omnidev`, its own postgres
       container `omnidev-postgres-1`, dev-only alias `omnidev-postgres`).
       NEVER point a dev binary/`db-migrations` at `omni-stack-postgres-1` or the
       omni-stack postgres IP (172.18.0.4:5432).
    2. The DB-write guard distinguishes BUILD MODE, not just DB host:
       - **Release-built images** (publish pipeline: `docker build --build-arg
         OMNIAGENT_BUILD_MODE=release`, baked as ENV in the image) AUTO-APPLY
         the idempotent declarative schema on container start against ANY
         database - no manual env vars, no operator step. Version upgrades
         just work.
       - **Dev-built binaries/images** (Dockerfile.dev, `cargo run`,
         `docker build` without the release arg -> default
         `OMNIAGENT_BUILD_MODE=dev`) refuse to auto-apply schema to any
         database whose host is NOT a known dev target
         (localhost/127.0.0.1/::1/`omnidev-postgres`). The bare `postgres`
         service name is deliberately NOT a dev host (the production
         omni-stack uses it).
    3. There is NO env-var override: only release-built images auto-apply schema
       against any database, and only a known dev host accepts a dev-built
       binary's schema writes. A dev-built binary pointed at a non-dev DB is
       refused - fix the DATABASE_URL, do not look for a bypass.
    4. The dev overlay (docker-compose.dev.yml) forces the dev stack onto its own
       postgres via the `omnidev-postgres` alias; a dev binary can never resolve
       to the omni-stack DB.
    
---

## Log Hygiene Rule (HARD, since 2026-09-05)

Events that can recur **per message / per iteration / per thread** (lifecycle anomalies, config discovery/refresh, polling loops) may ONLY log at `debug`/`trace` level, or be rate-limited. NEVER log them at `info` or `error`.

Background: a single 21h window produced 28,708 ERRORs ("Thread N has no cause message, skipping") and ~285,889 INFO lines (MCP `config::external` discovery) because per-event log sites used flooding levels. The journal drowned and real signals were lost.

Enforced by `tests/log_hygiene.rs` (source-scan guard tests): it fails the build if `mcp/external/config.rs` ever logs discovery at INFO or the no-cause thread skip in `src/agent/mod.rs` ever logs at ERROR again. When touching either site, keep the log at debug level.
