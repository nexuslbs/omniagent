# OmniAgent — AGENTS.md

## Plugin System Rules & Conventions

### Core Principle
Every action must be deterministic based on the **exact source** — never guess. The build strategy, YAML flags, and directory path are all derived from the source.

A plugin **can** exist at multiple sources simultaneously (e.g., a builtin crate in omniagent AND a bundled copy in omni-stack). The source field unambiguously identifies which one to act on.

**At most one source can be enabled per plugin name.** Enabling a different source for the same name automatically disables the previous one (overwrites the YAML entry).

### Source Definitions

| Source | Location | Build Method | Binary Check | YAML Flag |
|--------|---------|-------------|-------------|-----------|
| `built-in` | `/app/plugins/{type}/{name}/` | Workspace: `cargo build -p <pkg>` from `/app/Cargo.toml` | `get_bin_path(pkg_name)` | `builtin: true` |
| `bundled` | `{workspace_dir}/plugins/{type}/{name}/` | Standalone: `cargo build` from plugin's own `Cargo.toml` | `{dir}/target/release/{pkg_name}` | no builtin flag |
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/` | Standalone: `cargo build` from `.remote/{name}/Cargo.toml` | `{dir}/target/release/{pkg_name}` | `remote: {...}` |

### Builtin Plugin Rules (omniagent workspace members)

- **Builtin tools are disabled by default.** They must be explicitly added to the YAML file (tools.yml, platforms.yml, providers.yml) with `enabled: true` and `builtin: true` to activate.
- **When a tool is defined in YAML** (e.g., `enabled: true` without `builtin: true`), and a builtin with the same name exists on disk, the builtin is **ignored** — the one from omni-stack (bundled) or remote takes precedence.
- **When a builtin plugin has a YAML entry but `builtin` is false or absent**, it appears in the UI as **disabled** (`status: disabled`, source = `bundled`). The builtin source shows as **duplicated** (`is_duplicated: true`, `status: disabled`).
- **When a builtin plugin has NO YAML entry**, it appears in the UI as **disabled** (`is_duplicated: false`, `status: disabled`, source = `built-in`). Clicking Enable on it will create a YAML entry with `enabled: true, builtin: true`.
- **Builtin plugins** are workspace members in `/app/Cargo.toml`. They have `Cargo.toml` + `src/` + `mcp-config.json` but NOT `plugin.json`.
- **Disabling a builtin** via the dashboard sets `enabled: false` but preserves `builtin: true` in YAML.

### Bundled Plugin Rules (Omni-Stack)

- Bundled plugins live in `{workspace_dir}/plugins/{type}/{name}/`.
- They are considered "local/repo plugins" only if they have a `plugin.json` in the directory root. Lib-only crates (like `util`) with no `plugin.json` or `mcp-config.json` are skipped entirely.
- Most bundled plugins (fetch, filesystem, git, skills, docker-compose, test-rust-tool) are **self-contained**: they only depend on `mcp-server-util` and external crates.
- **`actions`** is an omni-stack-only plugin. It does NOT exist as a builtin in omniagent. Its source code only lives in omni-stack.
- Some omni-stack plugin directories contain **pre-compiled binaries only** (no Cargo.toml, no src/) — these are erroneous/leftover copies of builtin plugins.

### Erroneous Bundled Plugin Copies

The following directories in `/opt/workspace/omni-stack/plugins/mcp/` are **erroneous copies** of builtin plugins, containing only binaries (no source code):
- `cron`, `kanban`, `search`, `memory`, `metrics`, `query`, `plugin-manager`, `subtasks`, `hindsight`

These have `plugin.json` and a compiled binary but no `Cargo.toml` or `src/`. They show with `is_duplicated=true, has_source_code=false` in the UI. The actual source for these plugins is only in **omniagent** (`/app/plugins/mcp/<name>/`).

These will be removed from omni-stack in a future cleanup.

### Display Rules (Tools Page)

The `/api/plugins` response groups plugins by name and assigns a **primary source** based on YAML state:

1. YAML has `remote` → primary = `remote` source. Other sources = duplicated.
2. YAML has `builtin: true` → primary = `built-in` source. Other sources = duplicated.
3. YAML entry exists but no remote/builtin flag → primary = `bundled` source. Other sources = duplicated.
4. **No YAML entry** → primary = `built-in` source (so Install/Enable buttons are available).
5. Fallback → first source in discovery order.

Each non-primary source gets `is_duplicated: true` and its `status` is determined independently (no YAML entry → `disabled`, with YAML → mirrors the YAML entry). The frontend shows a yellow "duplicated" badge on duplicate entries.

### Install / Reinstall with Builtin Fallback

#### Fix: Binary-Only Bundled Copies No Longer Block Install

When the user tries to **Install** or **Reinstall** a plugin that is categorized as OmniStack (bundled) but the bundled directory contains only a pre-compiled binary (no Cargo.toml, no script entrypoint), the handler now **falls back** to the builtin source directory if it exists:

1. `detect_plugin_category_cross_type` determines the category (Builtin if `builtin: true` in YAML, OmniStack otherwise)
2. `get_plugin_dir_for_category` returns the source directory for the detected category
3. **Two fallback points** in both install/reinstall handlers:
   - If `get_plugin_dir_for_category` returns `None` (dir doesn't exist), try `/app/plugins/{type}/{name}/` before giving up with 404
   - If the source dir exists but has NO `Cargo.toml` AND no path-based entrypoint, try `/app/plugins/{type}/{name}/` before returning "no source code"
4. When falling back, the category is updated to `Builtin` so `compile_rust_crate` uses the workspace build strategy

This ensures that a plugin like `plugin-manager` with YAML `builtin: false` (or no builtin flag) that only has a binary copy in omni-stack can still be installed/reinstalled by compiling from the builtin source in omniagent.

#### Key Behaviors

**Install:**
- `POST /api/plugins/{name}/install` — detects source from YAML + disk state with Builtin fallback
- For builtin: workspace build from `/app/Cargo.toml`
- For bundled: standalone build from the plugin's Cargo.toml
- For remote: standalone build from `.remote/{name}/Cargo.toml`
- After compile: registers in YAML with `enabled: false`
- Compilation errors are **fatal** (return 500)
- Compilation runs in background (tokio::spawn) — returns immediately after YAML registration

**Reinstall:**
- `POST /api/plugins/{name}/reinstall` — same category detection as install with Builtin fallback
- Re-clones remote plugins from git, then recompiles
- Updates YAML entry (preserves enabled state)
- Compiles synchronously (not background)
- After compile, hot-reloads MCP server tools via `reload_tool_plugin` or `reload_platform_plugin`

**Uninstall:**
- `DELETE /api/plugins/{name}?mode=uninstall` — deletes from YAML
- For remote plugins: removes `.remote/` directory
- For non-remote: removes `target/` directories from both data_dir and workspace_dir

### Enable/Disable

- `POST /api/plugins/{name}/enable` — sets `enabled: true` in YAML
  - **REQUIRES JSON body** with `source` field: `{ "source": "built-in" }` or `{ "source": "bundled" }` or `{ "source": "remote" }`
  - `source: "built-in"` → sets `builtin: true` in YAML entry
  - `source: "bundled"` → sets `builtin: false`
  - `source: "remote"` → preserves any existing `remote` field
  - If plugin was not in YAML, the source determines the builtin flag
  - Hot-reloads MCP server for tool plugins (registers tools in shared registry)
- `POST /api/plugins/{name}/disable` — sets `enabled: false` in YAML
  - **REQUIRES JSON body** with `source` field
  - Source is validated but does NOT modify the `builtin` flag
  - Does NOT unset `builtin` flag
- Both preserve existing `builtin` and `remote` fields
- The `source` field is mandatory. Omitting it returns 422.
- Switching sources: To enable a different source for the same plugin name, call enable with the new source. The old source becomes disabled automatically because the YAML entry is overwritten.

### Toggle Request Flow (Frontend)

- The card carries `data-source` from the API response's `p.source` field
- On click, the toggle handler sends `{ source: pluginSource }` via `apiPost`
- All three pages (tools.ts, providers.ts, platforms.ts) use the same pattern
- If `data-source` is empty/missing, the toggle silently does nothing (`if (!pluginSource) return;`)
- After frontend rebuild: `npm run build:frontend` (dist is mounted read-only, no container restart needed)

### YAML Auto-Detection on Enable

When enabling a builtin plugin that has NO YAML entry yet, `set_entry()` calls `is_plugin_builtin()` which checks for:
- `/app/plugins/{type}/{name}/Cargo.toml` exists → builtin
- `/app/plugins/{type}/{name}/plugin.json` exists → builtin

If either exists, the YAML entry gets `builtin: true` automatically.

### Compiling Omni-Stack Plugins (Actions Special Case)

`compile_rust_crate(plugin_dir, name, source)` uses the `source` string to determine build strategy:

- **built-in**: Workspace build from `/app/Cargo.toml` — `cargo build --release -p <pkg_name>`
- **bundled/remote**: Standalone build — `cargo build --release` from the plugin's Cargo.toml

The actions plugin (`actions`) is an omni-stack-only plugin. Its source lives only in `/opt/workspace/omni-stack/plugins/mcp/actions/`. It does NOT exist as a builtin in omniagent. It compiles as a standalone crate connecting directly to Postgres via `sqlx`.

### Key API Response Fields

Each plugin entry in `/api/plugins` includes:
- `source`: `"built-in"` | `"bundled"` | `"remote"` | `"mcp_config"`
- `is_duplicated`: true when this source is NOT the YAML-configured primary
- `has_source_code`: true if `Cargo.toml` or script entrypoint exists
- `status`: `"enabled"` | `"disabled"` | `"error"`
- `needs_build`: true if Cargo.toml exists but no compiled binary found
- `is_script`: true if plugin has a script-based entrypoint with path (currently always false)

### No-Source and Binary-Only Plugins

- A plugin directory with only `plugin.json` (no Cargo.toml, no src/) is a **binary-only** plugin
- `has_source_code` = false → Install/Reinstall buttons are hidden or disabled in the UI
- The "no source" badge is displayed in yellow (`badge-warning`) with a tooltip explaining this
- These plugins can still be enabled/disabled if they have a working binary
- Binary-only entries in omni-stack (erroneous copies of builtins) are non-functional — they point to `mcp-server-<name>` which doesn't exist in the omni-stack path

### Key Files

| File | Purpose |
|------|---------|
| `src/server/plugins.rs` | Plugin API handlers, `compile_rust_crate(source)`, `category_to_source()`, `install_plugin_handler`, `reinstall_plugin_handler` |
| `src/plugins_yaml.rs` | YAML state, `pick_primary_source`, `build_plugin_detail`, `set_entry`, `is_plugin_builtin`, `list_plugins`, `get_plugin` |
| `src/plugin/installer.rs` | Filesystem install/uninstall/discover (`discover_plugins`, `install_from_git`, `install_from_url`) |
| `src/plugin/mod.rs` | PluginManifest, PluginType, PluginEntrypoint, DYNAMIC_ENUM_CACHE |
| `docker-compose.yml` (omni-stack) | Container volumes, WORKSPACE_DIR env var |
| `omni-stack/plugins/mcp/<name>/Cargo.toml` | Bundled plugin dependencies (path must resolve in container) |

### Pitfalls and Edge Cases

- **`util` crate is NOT a plugin.** The `discover_plugins` section D function skips directories that have only Cargo.toml without plugin.json or mcp-config.json. The `util` crate (a library helper) has neither, so it's correctly excluded from plugin discovery.
- **Binary-only bundled copies can block Install/Reinstall** if the YAML has no `builtin: true`. Fixed by the Builtin fallback in the install/reinstall handlers.
- **`actions` plugin has no builtin counterpart.** It lives only in omni-stack with full source code (Cargo.toml + src/ + plugin.json). Do NOT add a `/app/plugins/mcp/actions/` directory.
- **Cargo.toml dependency paths** for omni-stack plugin must resolve inside the container. The `actions` plugin is the only one that depends on `omniagent` crate, and its path is `../../../../omniagent` (relative to `/opt/workspace/omni-stack/plugins/mcp/actions/`).
- **Hot-reload** of MCP tools happens on enable and reinstall. Disabling removes the server's tools from the shared registry.
- **MCP registry cross-check**: after listing plugins, the handler checks if enabled MCP servers have registered tools. If not, status is set to `"error"` (the server failed to initialize).
