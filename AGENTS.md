# OmniAgent — AGENTS.md

## Plugin System Rules & Conventions

### Core Principle
Every action must be deterministic based on the **exact source** — never guess. The build strategy, YAML flags, and directory path are all derived from the source.

A plugin **can** exist at multiple sources simultaneously (e.g., a builtin crate in omniagent AND a bundled copy in omni-stack). The source field unambiguously identifies which one to act on.

### Source Definitions

| Source | Location | Build Method | Binary Check | YAML Flag |
|--------|---------|-------------|-------------|-----------|
| `built-in` | `/app/plugins/{type}/{name}/` | Workspace: `cargo build -p <pkg>` from `/app/Cargo.toml` | `get_bin_path(pkg_name)` | `builtin: true` |
| `bundled` | `{workspace_dir}/plugins/{type}/{name}/` | Standalone: `cargo build` from plugin's own `Cargo.toml` | `{dir}/target/release/{pkg_name}` | no builtin flag |
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/` | Standalone: `cargo build` from `.remote/{name}/Cargo.toml` | `{dir}/target/release/{pkg_name}` | `remote: {...}` |

### Category Detection Priority (`detect_plugin_category`)

1. YAML has `remote` → `PluginCategory::Remote`
2. YAML has `builtin: true` → `PluginCategory::Builtin`
3. Disk has Cargo.toml at `/app/plugins/{type}/{name}/` → `PluginCategory::Builtin`
4. Disk has `.remote/` directory → `PluginCategory::Remote`
5. Default → `PluginCategory::OmniStack`

### Primary Source Priority (`pick_primary_source`)

1. YAML has `remote` → prefer `remote` source
2. YAML has `builtin: true` → prefer `built-in` source
3. YAML entry exists but no remote/builtin → prefer `bundled` source
4. **No YAML entry** → prefer `built-in` source (so Install/Enable buttons are visible)
5. Fallback → first source in discovery order

### Builtin Plugin Rules

- **Builtin tools are disabled by default.** They must be explicitly added to the YAML file (tools.yml, platforms.yml, providers.yml) with `enabled: true` and `builtin: true` to activate.
- **When a tool is defined in YAML** (e.g., `enabled: true` without `builtin: true`), and a builtin with the same name exists on disk, the builtin is **ignored** for the YAML-configured entry. The one from omni-stack (bundled) or remote takes precedence.
- **When a builtin plugin has NO YAML entry**, it appears in the UI as **disabled** (`is_duplicated: false`, `status: disabled`, source = `built-in`). Clicking Enable on it will create a YAML entry with `enabled: true, builtin: true`.
- **Builtin plugins live only in omniagent** (`/app/plugins/{type}/{name}/`). They are workspace members in `/app/Cargo.toml`. They have `Cargo.toml` + `src/` + `mcp-config.json` but NOT `plugin.json`.

### Bundled Plugin Rules (Omni-Stack)

- Bundled plugins live in `{workspace_dir}/plugins/{type}/{name}/` (mounted at `/opt/workspace/omni-stack/`).
- They have their own `Cargo.toml` + `src/` + `plugin.json` + `mcp-config.json`.
- Most bundled plugins (fetch, filesystem, git, skills, docker-compose, test-rust-tool) are **self-contained**: they only depend on `mcp-server-util` and external crates.
- **`actions`** is special: it depends on `omniagent` crate for database access. Its Cargo.toml dependency path **must** be `../../../../omniagent` to resolve inside the Docker container.
- Some omni-stack plugin directories contain **pre-compiled binaries only** (no Cargo.toml, no src/) — these are erroneous/leftover copies of builtin plugins (kanban, memory, hindsight, etc.). They show as `is_duplicated=true, status=disabled, has_source_code=false` in the UI and are non-functional.

### Key Behaviors

#### Install
- `POST /api/plugins/{name}/install` — detects source from YAML + disk state
- For builtin: workspace build from `/app/Cargo.toml`
- For bundled: standalone build from the plugin's Cargo.toml
- For remote: standalone build from `.remote/{name}/Cargo.toml`
- After compile: registers in YAML with `enabled: false`
- Compilation errors are **fatal** (return 500)

#### Reinstall
- `POST /api/plugins/{name}/reinstall` — same category detection as install
- Re-clones remote plugins, then recompiles
- Updates YAML entry (preserves enabled state)

#### Enable/Disable
- `POST /api/plugins/{name}/enable` — sets `enabled: true` in YAML
  - If plugin was not in YAML, auto-detects `builtin: true` from disk
  - Hot-reloads MCP server for tool plugins (registers tools in shared registry)
- `POST /api/plugins/{name}/disable` — sets `enabled: false` in YAML
  - Does NOT unset `builtin` flag
- Both preserve existing `builtin` and `remote` fields

### YAML Auto-Detection on Enable

When enabling a builtin plugin that has NO YAML entry yet, `set_entry()` calls `is_plugin_builtin()` which checks for:
- `/app/plugins/{type}/{name}/Cargo.toml` exists → builtin
- `/app/plugins/{type}/{name}/plugin.json` exists → builtin

If either exists, the YAML entry gets `builtin: true` automatically.

### Warning: Erroneous Bundled Plugins

The following plugin directories in omni-stack (`/opt/workspace/omni-stack/plugins/mcp/`) were **erroneously added** by a previous agent and contain only binaries (no source code):
- `cron`, `kanban`, `search`, `memory`, `metrics`, `query`, `plugin-manager`, `subtasks`, `hindsight`

These are DEAD entries. They have `plugin.json` but no `Cargo.toml` or `src/`. They MUST NOT be installed (no source to compile). The actual source for these plugins is in **omniagent** (`/app/plugins/mcp/<name>/`) as workspace-built builtins.

These omni-stack copies will be removed later. For now, they appear as `is_duplicated=true, status=disabled, has_source_code=false` in the UI.

### Compiling Omni-Stack Plugins (Actions Special Case)

`compile_rust_crate(plugin_dir, name, source)` uses the `source` string to determine build strategy:

- **built-in**: Workspace build from `/app/Cargo.toml` — `cargo build --release -p <pkg_name>`
- **bundled/remote**: Standalone build — `cargo build --release` from the plugin's Cargo.toml

The actions plugin (`actions`) depends on the `omniagent` crate (for database access). Its `Cargo.toml` has:
```toml
omniagent = { path = "../../../../omniagent" }
```
This resolves to `/opt/workspace/omniagent/Cargo.toml` inside the container (4 levels up from actions directory: `/opt/workspace/omni-stack/plugins/mcp/actions/` → `/opt/workspace/` → `omniagent/`).

This is the ONLY plugin in omni-stack that depends on `omniagent`. All others are self-contained.

### Key API Response Fields

Each plugin entry in `/api/plugins` includes:
- `source`: `"built-in"` | `"bundled"` | `"mcp_config"`
- `is_duplicated`: true when this source is NOT the YAML-configured primary
- `has_source_code`: true if `Cargo.toml` or script entrypoint exists
- `status`: `"enabled"` | `"disabled"`

### Key Files

| File | Purpose |
|------|---------|
| `src/server/plugins.rs` | Plugin API handlers, `compile_rust_crate(source)`, `category_to_source()` |
| `src/plugins_yaml.rs` | YAML state, `pick_primary_source`, `build_plugin_detail`, `set_entry`, `is_plugin_builtin` |
| `src/plugin/installer.rs` | Filesystem install/uninstall/discover |
| `docker-compose.yml` (omni-stack) | Container volumes, WORKSPACE_DIR env var |
| `omni-stack/plugins/mcp/<name>/Cargo.toml` | Bundled plugin dependencies (path must resolve in container) |
