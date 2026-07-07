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

### Source Definitions

| Source | Location | Build Method | Binary Check |
|--------|---------|-------------|-------------|
| `built-in` | `/app/plugins/{type}/{name}/` | Workspace: `cargo build -p <pkg>` from `/app/Cargo.toml` | `get_bin_path(pkg_name)` |
| `bundled` | `{workspace_dir}/plugins/{type}/{name}/` | Standalone: `cargo build` from plugin's own `Cargo.toml` | `{dir}/target/release/{pkg_name}` |
| `remote` | `{data_dir}/plugins/{type}/.remote/{name}/` | Standalone: `cargo build` from `.remote/{name}/Cargo.toml` | `{dir}/target/release/{pkg_name}` |

### Builtin Plugin Rules

- **Builtin plugins are disabled by default.** They must be explicitly added to `plugins.yml` with `enabled: true` and `source: built-in`.
- **When a tool is defined in YAML** with `source: bundled` and a builtin with the same name exists, the builtin is ignored — the bundled source is the primary.
- **When a builtin plugin has a YAML entry but NO explicit `source` field**, it defaults to `built-in` but appears as disabled if enabled=false.
- **Builtin plugins** are workspace members in `/app/Cargo.toml`.

### Bundled Plugin Rules (Omni-Stack)

- Bundled plugins live in `{workspace_dir}/plugins/{type}/{name}/`.
- They are considered "local/repo plugins" only if they have a `plugin.json` in the directory root.

### Display Rules (Tools Page)

The `/api/plugins` response groups plugins by name and assigns a **primary source** based on YAML:

1. YAML entry with `source: remote` → primary = remote.
2. YAML entry with `source: built-in` → primary = built-in.
3. YAML entry with `source: bundled` → primary = bundled.
4. **No YAML entry** → primary = built-in (so Install/Enable buttons are available).
5. Fallback → first source in discovery order.

Each non-primary source gets `is_duplicated: true`.

### Install / Reinstall with Builtin Fallback

When Install/Reinstall is called and the categorized source directory has no Cargo.toml (only pre-compiled binary), the handler falls back to the builtin source.

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
