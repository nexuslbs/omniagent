# OmniAgent — AGENTS.md

## Plugin System Rules & Conventions

### Core Principle
The **source** field in `plugins.yml` is authoritative — it determines which binary/source to use. No more `builtin: bool` or `remote: {...}` guessing.

A plugin **can** exist at multiple sources simultaneously (e.g., a builtin crate in omniagent AND a bundled copy in omni-stack). The `source` field unambiguously identifies which one to act on.

**At most one source can be enabled per plugin name.** Enabling a different source overwrites the YAML entry for that name.

### Configuration Files (omni-stack)

| File | Purpose |
|------|---------|
| `plugins.yml` | Unified config — replaces old tools.yml/platforms.yml/providers.yml |
| `remote.yml` | Remote plugin metadata (URL, path, ref) — versioned in git |

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

### Source Determination — HARD RULE: NO PRIORITY, NO FALLBACK

A plugin's **source** is determined **solely by its physical location on disk**. There is no priority order between built-in, bundled, and remote. Each stands independently:

| Source | Physical Location | Identified By |
|--------|------------------|---------------|
| `built-in` | `/app/plugins/{type}/{name}/` | `Cargo.toml` + `plugin.json` or `mcp-config.json` in workspace |
| `bundled` | `{data_dir}/plugins/{type}/{name}/` or `{workspace_dir}/plugins/{type}/{name}/` | `plugin.json` at root |
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/{path}/` | `plugin.json` at subpath + entry in `remote.yml` |

**The `source` field in `plugins.yml` is authoritative.** When a plugin has a YAML entry with `source: built-in`, only the built-in source is active. The bundled and remote sources for the same name still exist on disk but are marked `is_duplicated: true` and shown as disabled.

**When there is no YAML entry**, all sources are discovered and shown as disabled. The user can enable any source via the dashboard, which creates a YAML entry with that source.

**No function should guess or fall back between sources.** The `detect_plugin_category_cross_type()` function returns `None` when no YAML entry exists — it does NOT pick a source. Each caller (install handler, enable handler, etc.) has its own source-specific logic.

**MCP scanner (`discover_plugin_servers`) is source-aware:** It reads `plugins.yml` and only starts MCP servers for enabled plugins at their correct source location. It does NOT scan all directories blindly.

**Plugin discovery (`discover_plugins`) scans ALL directories:** Sections A-D scan every physical location so ALL discoverable plugins appear in the dashboard listing. Plugins not in `plugins.yml` default to `status: "disabled"`.
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/{path}/` | Standalone: `cargo build` from `.remote/{name}/{path}/Cargo.toml` | `{dir}/target/release/{pkg_name}` |

### Builtin Plugin Rules

- **Builtin plugins are disabled by default.** They must be explicitly added to `plugins.yml` with `enabled: true` and `source: built-in`.
- **If a tool/plugin is defined in YAML** with `source: bundled` or `source: remote` and a builtin with the same name exists, the builtin is ignored — the non-builtin source is the primary. The builtin still shows as an available source but marked as duplicated.
- **When a builtin plugin has a YAML entry but no explicit `source` field**, it defaults to `built-in` but appears as disabled if enabled=false.
- **Builtin plugins** are workspace members in `/app/Cargo.toml`.
- **Only plugins with `plugin.json` at directory root** are considered local/repo plugins. Directories without `plugin.json` (e.g., config-only dirs like `util`) should not appear as discoverable plugins.
- **Duplicated plugins in the tools page**: When a plugin exists both as builtin (in omniagent `/app/plugins/`) and bundled (in omni-stack `plugins/`), the non-primary source shows as "duplicated" in the dashboard. The omni-stack copy usually takes precedence unless the YAML explicitly sets `source: built-in`.

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

**Key behavior change (2026-07-07):** When there is no YAML entry, `pick_primary_source()` returns `None`, and `is_duplicated` is set to `group.sources.len() > 1` — meaning all sources in a multi-source group show as duplicated. This ensures the YAML-configured source is always the authority; without YAML, all sources are equal.

**Enabling a source** (via dashboard or API) creates a YAML entry with that `source`, making it primary and marking all others as duplicated.

### Plugin Display Rules (Dashboard)

- **Any Rust plugin needing build** (`source_code=true, !is_script, needs_build=true`): Show **Install** button (purple). This applies to built-in, bundled, and remote sources alike.
  - "Update" is only for non-compilable (script/no-source) remote plugins — they show **Remove + Update** buttons (since they need re-cloning, not compilation).
- **Installed Rust plugins** (`needs_build=false`): Show **Uninstall + Reinstall** buttons.
- **Non-remote Rust plugins needing build**: Show **Install** button (same as remote — no distinction).
- **Script/no-source plugins**: Show **Remove + Update** buttons (remote) or no build buttons (non-remote).
- **Duplicated entries** (non-primary source in a multi-source group): Show no action buttons (status indicator only).

### Plugin Discovery Rules

- `.remote/` directories contain remote plugin clones. Plugins inside `.remote/` with `plugin.json` at root are discovered as remote sources.
- Plugins cloned with a `path` sub-path (e.g., `path: tools/cron-echo`) are in a subdirectory within `.remote/{name}/{path}/`.
- Stale/old plugin directories in the workspace (non-.remote copies, mcp/ dirs, temp clones) should be cleaned up. They create false "bundled" or "duplicated" entries.
- The `remote.yml` must have entries that match the `.remote/` directory structure. Orphan `.remote/` directories (no remote.yml entry) are ignored.

### Install / Reinstall with Builtin Fallback

When Install/Reinstall is called and the categorized source directory has no Cargo.toml (only pre-compiled binary), the handler falls back to the builtin source.

### Git Install (install-git)

- **API**: `POST /api/plugins/install-git` — clones a plugin repo and persists to `remote.yml` only.
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
- `status: "not_found"` — red badge in dashboard
- `needs_download: true` — for remote plugins not yet cloned

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
- For non-remote plugins, uninstall removes the YAML entry and the compiled `target/` directory.

### Bundled Plugin Buttons (Dashboard)

- **Bundled plugins always show a Remove button** — there is no Update button (the code lives in the omni-stack repo, there's no external repository to update).
- The Remove button calls `DELETE /api/plugins/{name}` (remove mode), which removes the YAML entry and the compiled `target/` directory.
- Bundled plugins with `source: bundled` in `plugins.yml` can be removed via this endpoint. If the entry is not in `plugins.yml`, the Remove button still shows for discovery-purposes only (removes any stale YAML entry).
- The Install button for bundled plugins compiles synchronously, writes `enabled: true` to `plugins.yml`, and hot-reloads the MCP server — all in one synchronous API call. No more background compile.
