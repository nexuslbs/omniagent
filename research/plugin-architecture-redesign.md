# Plugin Architecture Redesign: From Global Statics to Actor Pattern

> Research conducted July 2026
> Root cause analysis of the fragile plugin system and a comprehensive migration plan

---

## 1. Executive Summary

The OmniAgent plugin system is fragile because it couples **three anti-patterns** that amplify each other:

1. **Global mutable static state** — 3+ `Lazy<std::sync::Mutex<HashMap<...>>>` instances and an `Arc<RwLock<McpRegistry>>` shared across all modules. Every operation (register, unregister, delete, enable, disable, reload) goes through a shared lock. Background tasks holding read locks block ALL mutations.

2. **Fragmented lifecycle management** — Plugin lifecycle events are scattered across 6+ files (`plugins_enable.rs`, `plugins_delete.rs`, `plugins_reload.rs`, `plugins_setup.rs`, `plugins.rs`, `client.rs`), each directly tapping the globals. No single authority manages plugin state transitions, so operations race with each other.

3. **Unmanaged MCP subprocesses** — MCP subprocesses are spawned per-channel via `McpClientPool`, stored in a global `HashMap<(server_name, channel_id), Arc<McpClientPool>>`. Pool clearing drops the `Arc`, killing the subprocess with SIGKILL. No graceful drain, no health checking, no restart supervision.

Network effects: A slow plugin, a hung subprocess, or a background task iterating tools under a read lock blocks **everything** — including API requests to DELETE/disable unrelated plugins.

---

## 2. Current Architecture (Problems in Detail)

### 2.1 Global Statics Map

```
src/mcp/external/client.rs
├── CLIENT_POOLS:        Lazy<std::sync::Mutex<HashMap<(String, i64), Arc<McpClientPool>>>>
├── SERVER_CONFIGS:      Lazy<std::sync::Mutex<HashMap<String, McpServerConfig>>>
├── CLIENT_REGISTRY:     Lazy<std::sync::Mutex<HashMap<String, Arc<dyn McpServerClient>>>>
└── (3 more inlined statics per server config)
```

These are initialized once, never cleaned up, and use **`std::sync::Mutex`** in an async Tokio runtime. Though lock durations are short (HashMap insert/remove), `std::sync::Mutex` blocks the **entire worker thread** when contended, unlike `tokio::sync::Mutex` which yields to other tasks.

### 2.2 Arc<RwLock<McpRegistry>> Lock Contention

```
AppState.tool_registry: Arc<tokio::sync::RwLock<McpRegistry>>
```

Every operation on tools passes through this single RwLock:

| Operation | Lock Type | Duration |
|-----------|-----------|----------|
| Tool execution (dispatch) | Read | Variable (entire tool call) |
| Tool execution (result collection) | Read | Variable (iterates results) |
| Scheduler tick | Read | Entire thread processing cycle |
| DELETE plugin | Write | Blocks until ALL readers finish |
| Enable/disable plugin | Write | Blocks until ALL readers finish |
| Reload plugin | Write | Blocks until ALL readers finish |

The write lock acquisition is the bottleneck. `remove_by_server()` is O(n) and completes in microseconds — but **waiting for readers** can take 25-30s when the scheduler or a long-running tool call holds a read lock.

### 2.3 Indiscriminate Subprocess Killing

`clear_server_pools(server_name)` drops the `Arc<McpClientPool>` from the global `CLIENT_POOLS` HashMap. When the `Arc` refcount drops to zero, `McpClientPool` is dropped, which drops each `AsyncChildProcess` handle, which closes stdin → the OS sends SIGPIPE/SIGKILL to the subprocess.

If the subprocess is mid-tool-call, the tool call fails silently. The executor sees a timeout or connection error. The next tool call spawns a new subprocess with the updated config.

**Problem areas:**
- `reload_tool_plugin()` clears pools BEFORE re-initializing — creates a window where the plugin is unavailable
- `plugins_delete.rs` clears pools in 7 different code paths, each with slightly different guard conditions
- `plugins_enable.rs` clears pools on both enable AND disable (disable should just clean up)
- No coordination: if two DELETE requests arrive concurrently, both call `clear_server_pools()` and both call `tool_registry.write().await` — the second write waits for the first, but the pool was already cleared twice

### 2.4 Platform Plugin Supervisor Complexity

The platform plugin lifecycle (`ExternalPlatformClient` in `client.rs`) uses:

- `AtomicU64` restart counter (migrated from `AtomicBool` due to lost restart signals)
- `tokio::sync::Notify` (had stale bit races — had to add guard checks before and inside the inner loop)
- A spawn loop with circuit breaker (3 max retries)
- A wait loop after subprocess exit (checks every 1s for pending restart)

This evolved through bug fixes rather than design. The result is a complex state machine with race conditions that were fixed one at a time:

1. AtomicBool → AtomicU64 (lost restart signals)
2. Added stale-notification check before inner loop (infinite restart loop)
3. Added stale-notification check inside select! handler (subprocess killed by stale Notify bit)
4. Added spawn-wait loop (restart signals lost after subprocess exit)

**A proper supervisor actor would handle all of this in ~50 lines of code.**

### 2.5 Config Delivery Fragmentation

Config flows through 4+ stages:

1. `plugin.json` → `config_schema` defaults (with `$env:` refs)
2. `plugins.yml` → user overrides (with `$env:` / `$secret:` refs)
3. `merge_yaml_config_into_env()` → prefixed keys into `config.env`
4. `apply_config_schema_defaults()` → non-prefixed keys into `config.env`
5. `resolve_config_value()` → resolves `$env:` refs
6. `build_configure_request()` → serializes to JSON-RPC `configure` message

Platform plugins have a completely separate path:

1. `plugin.json` → `config_schema` defaults
2. `load_plugins_config()` → loads from `plugins/platforms/` directories
3. `resolve_config_refs()` → resolves `$secret:` / `$env:` refs in `setup_env`
4. Sent as JSON-RPC `setup` message via `plugins_setup.rs`

Two parallel systems with different code paths, different resolution orders, and different edge cases.

---

## 3. Architectural Solution: PluginManager Actor Pattern

### 3.1 Core Idea

Replace all global statics and the `Arc<RwLock<McpRegistry>>` with a single **actor** — a `tokio::spawn` task that owns ALL plugin state and processes requests through typed `mpsc` channels.

```
┌──────────────────────────────────────────────────┐
│                  PluginManager Actor              │
│                                                    │
│  ┌──────────────┐  ┌──────────────────────────┐   │
│  │ McpRegistry   │  │  MCP Pool Manager        │   │
│  │ (owned, no    │  │  (owns Arc<McpClientPool> │   │
│  │  RwLock)      │  │   per (server, channel)) │   │
│  └──────────────┘  └──────────────────────────┘   │
│                                                    │
│  ┌──────────────────┐  ┌────────────────────────┐ │
│  │ Plugin Lifecycle   │  │ Platform Supervisor   │ │
│  │ State Machine      │  │ (one per platform)    │ │
│  └──────────────────┘  └────────────────────────┘ │
│                                                    │
│  mpsc::Receiver<PluginCommand>                      │
└──────────────────────────────────────────────────┘
         ▲
         │ send()
         │
  ┌──────┴──────┬──────────┬──────────┐
  │             │          │          │
Executor   Axum handlers  Scheduler  Cron
```

### 3.2 Message Types

```rust
enum PluginCommand {
    /// Register a new set of tools from an MCP server
    RegisterTools {
        server_name: String,
        tools: Vec<McpTool>,
        config: McpServerConfig,
        response: oneshot::Sender<Result<(), Error>>,
    },

    /// Remove all tools for a server and clean up pools
    UnregisterServer {
        server_name: String,
        response: oneshot::Sender<Result<(), Error>>,
    },

    /// Execute a tool on a specific server (for a specific channel)
    ExecuteTool {
        server_name: String,
        channel_id: i64,
        call: McpToolCall,
        context: ToolExecutionContext,
        response: oneshot::Sender<Result<McpToolResult, Error>>,
    },

    /// Snapshot all current tools (for executor dispatch)
    SnapshotTools {
        response: oneshot::Sender<McpRegistry>,
    },

    /// Enable a plugin (register tools, start subprocesses)
    EnablePlugin {
        name: String,
        plugin_type: PluginType,
        response: oneshot::Sender<Result<(), Error>>,
    },

    /// Disable a plugin (remove tools, stop subprocesses gracefully)
    DisablePlugin {
        name: String,
        response: oneshot::Sender<Result<(), Error>>,
    },

    /// Restart a platform plugin
    RestartPlatform {
        name: String,
        reason: String,
    },

    /// Graceful shutdown — drain all pools, stop all subprocesses
    Shutdown {
        response: oneshot::Sender<()>,
    },
}
```

### 3.3 Benefits Over Current Architecture

| Concern | Current | Actor Pattern |
|---------|---------|---------------|
| **Lock contention** | `RwLock` write blocks on readers | Zero contention — actor processes one command at a time |
| **Global state** | 3+ `Lazy<Mutex<HashMap>>` + `Arc<RwLock<McpRegistry>>` | Everything owned by one task. No statics. |
| **State snapshot** | Readers hold RwLock for entire operation | `SnapshotTools` clones registry in actor → returns via oneshot. Zero blocking. |
| **Subprocess lifecycle** | Arc drop → SIGKILL | Explicit `drain()` with timeout → SIGTERM → SIGKILL grace window |
| **Plugin isolation** | All plugins share globals | Each plugin gets isolated state within actor. One bad plugin can't poison others. |
| **Platform supervision** | Spawn loop + AtomicU64 + Notify race fixes | Single `tokio::select!` loop per platform. Clear states: Running, Draining, Stopped. |
| **Tool execution** | Locks pool via `get_or_create_pool` (std::sync::Mutex) + RwLock | Actor dispatches command → pool responds directly. No locks in hot path. |
| **Testability** | Hard to test — globals persist across tests | Actor can be constructed in tests. Full isolation. Deterministic. |
| **Concurrent mutations** | Two DELETE requests race on globals | Actor serializes all mutations. Sequential processing. |

### 3.4 Platform Supervisor

Each platform plugin gets its own lightweight supervisor within the actor:

```rust
struct PlatformSupervisor {
    name: String,
    subprocess: Option<AsyncChildProcess>,
    restart_signal: watch::Receiver<u64>,
    state: PlatformState,
}

enum PlatformState {
    Stopped,
    Starting,       // Spawning + initializing
    Running,        // Inner loop active
    Draining,       // Graceful shutdown in progress
}

impl PlatformSupervisor {
    async fn run(&mut self) {
        loop {
            self.spawn().await;  // Also sends initialize + configure
            tokio::select! {
                result = self.run_inner_loop() => {
                    match result {
                        Err(e) => tracing::warn!("Platform '{}' exited: {}", self.name, e),
                        Ok(()) => tracing::info!("Platform '{}' exited cleanly", self.name),
                    }
                }
                _ = self.restart_signal.changed() => {
                    // Restart requested during init — drain and respawn
                    self.drain().await;
                    continue;
                }
            }

            // Check if restart was requested
            if self.restart_signal.has_changed() {
                self.drain().await;
                continue;
            }
            break;  // No restart → stop permanently
        }
    }

    async fn drain(&mut self) {
        if let Some(proc) = self.subprocess.take() {
            // Graceful shutdown: close stdin → wait with timeout
            drop(proc.stdin);
            tokio::time::timeout(Duration::from_secs(5), proc.child.wait()).await.ok();
            // Force kill if still alive
            let _ = proc.child.start_kill();
        }
    }
}
```

### 3.5 Migration Strategy

The migration from global statics to actor pattern should happen in phases:

#### Phase 1: Extract PluginManager trait (low risk, no behavior change)

1. Create `src/agent/plugin_manager.rs` with a `PluginManager` trait
2. Implement `LegacyPluginManager` that wraps the existing statics
3. Wire it into `AppState` as `Arc<dyn PluginManager>`
4. All call sites go through the trait instead of directly accessing globals

This phase changes nothing about behavior — it just introduces the abstraction boundary. The trait also documents the actual surface area of the plugin system.

#### Phase 2: Implement ActorPluginManager (new code, new statics, parallel)

5. Build `ActorPluginManager` that implements the same trait but uses `mpsc` + actor task
6. It owns its own `McpRegistry` internally (no RwLock — direct field access in the actor loop)
7. It manages its own MCP pools internally (no global `CLIENT_POOLS`)
8. It manages its own server configs internally (no global `SERVER_CONFIGS`)
9. It manages its own platform restart signals internally (no global signals in AppState)
10. The actor is constructed in `main.rs` and injected into `AppState`

#### Phase 3: Swap implementation (switch over)

11. Change `AppState.plugin_manager` from `LegacyPluginManager` to `ActorPluginManager`
12. Remove `state.tool_registry`, `state.platform_restart_signals`, env vars
13. Remove global statics from `client.rs`: `CLIENT_POOLS`, `SERVER_CONFIGS`, `CLIENT_REGISTRY`
14. Remove `handle_remove_by_source` direct calls to `clear_server_pools()` — they go through the trait now

#### Phase 4: Clean up platform supervision (optional, reduce complexity)

15. Move platform spawn loop logic into `ActorPluginManager` as PlatformSupervisor
16. Remove `ExternalPlatformClient` spawn loop and restart counter complexity
17. Platform plugins become child actors managed by the PluginManager actor

---

## 4. Key Design Decisions

### 4.1 Snapshot vs. Reference

The executor currently holds a read lock on `McpRegistry` during tool execution and result collection. With the actor pattern, the executor requests a **snapshot** (clone) of the registry:

```rust
// Before (current):
let mcp_snapshot = state.tool_registry.read().await.clone();
// Lock held until snapshot drops

// After (proposed):
let mcp_snapshot = plugin_manager.snapshot_tools().await?;
// Zero contention — actor clones internally and sends back via oneshot
```

This adds a clone cost (HashMap + all tools) per snapshot, but:
- Snapshots are requested at most once per LLM turn (not per tool call)
- `McpRegistry` contains at most ~100-200 entries
- Clone of lightweight `McpTool` structs is negligible (microseconds)
- **Zero blocking** on the actor — the executor can process the snapshot while the actor handles other requests

If clone performance becomes a concern, use `Arc<HashMap<String, McpTool>>` and swap the entire map atomically on mutation.

### 4.2 Graceful Subprocess Management

Current behavior: `clear_server_pools()` drops the Arc → subprocess SIGKILL'd.

Proposed: The actor owns all `Arc<McpClientPool>` refs. When a pool needs clearing:

1. Remove pool from internal HashMap (prevents new tool calls from reaching it)
2. Send a `drain` signal to the pool
3. Pool stops accepting new requests
4. In-flight requests get a grace period (configurable, default 5s)
5. After grace period, kill remaining subprocesses
6. Return response to caller

This prevents tool call failures during plugin reload.

### 4.3 Zero Statics

The actor pattern eliminates ALL global statics:

| Current Static | Replacement |
|---------------|-------------|
| `CLIENT_POOLS` | `ActorPluginManager.pools: HashMap<(String, i64), Arc<McpClientPool>>` |
| `SERVER_CONFIGS` | `ActorPluginManager.configs: HashMap<String, McpServerConfig>` |
| `CLIENT_REGISTRY` | Removed — merged into actor |
| `AppState.tool_registry` | `ActorPluginManager.registry: McpRegistry` (owned, no RwLock) |
| `AppState.platform_restart_signals` | `ActorPluginManager.platforms: HashMap<String, PlatformSupervisor>` (owned, no Mutex) |

### 4.4 Config Delivery Simplification

The current dual-path config delivery (platforms vs MCP tools) should be unified:

- **All plugins** receive config through the same `configure` message format
- Platform plugins get an additional `setup` message (for bootstrapping channels/tokens)
- `plugin.json config_schema` is the single source of truth for ALL default config
- `plugins.yml` user overrides override config_schema defaults for both platform and MCP plugins
- `resolve_config_refs()` is called exactly once when the config is loaded, not spread across 3 layers
- No prefixed/non-prefixed key duality — use non-prefixed keys everywhere

---

## 5. Rust Plugin Best Practices

Research into Rust plugin system design patterns (acts-as, cargo-dylint, wasm-plugins, typemap, actor frameworks):

### 5.1 Recommended Patterns

1. **Actor Pattern (Erlang/OTP style)**: Single-ownership message-passing for lifecycle management. Avoids all lock contention by design. Each plugin has its own state, mutated only by its owning actor.

2. **Type-erased plugin trait**: `trait Plugin: Send + 'static` with methods like `async fn handle_config(&mut self, config: Value) -> Result<(), Error>` and `async fn execute_tool(&self, call: McpToolCall) -> Result<McpToolResult, Error>`. The plugin manager holds `Box<dyn Plugin>`.

3. **Graceful shutdown via Drop + drain**: Each plugin implements `fn drain(&mut self) -> DrainFuture` that closes connections, flushes buffers, and notifies peers. A global shutdown coordinator calls drain on all plugins with a configurable timeout.

4. **Health checks via heartbeat**: Actor processes a `HealthCheck { response: oneshot::Sender }` command. If the actor doesn't respond within 5s, it's considered stuck and replaced.

5. **No global static mutable state**: Use constructors with explicit dependency injection. Every component receives its dependencies at construction time through the `AppState` or `PluginManager` interface.

### 5.2 Anti-Patterns to Eliminate

| Anti-Pattern | Occurrence | Fix |
|--------------|-----------|-----|
| `Lazy<std::sync::Mutex<HashMap>>` for registry | 3+ locations | Actor-owned state |
| `Arc<RwLock<T>>` for hot-path data | 2 locations (tool_registry, config) | Actor snapshot pattern |
| `std::sync::Mutex` in async context | 4 locations | Replace with actor serialization |
| Static mutable state in test | All integration tests that rely on globals | Actor construction in test setup |
| `Notefy` for persistent flags | Platform restart (2 fix cycles) | `watch::channel` or dedicated actor |
| Unordered subprocess termination | `clear_server_pools` (7 call sites) | Graceful drain protocol |

---

## 6. Risk Assessment

### 6.1 Migration Risks

| Risk | Mitigation |
|------|-----------|
| Actor becomes bottleneck | Actor processes commands in microseconds. Heavy operations (spawning subprocesses) are delegated to `tokio::spawn` and reported back via oneshot. The actor loop is never blocked. |
| Message ordering | Messages are processed sequentially. Sequential consistency is correct for plugin lifecycle (you want serializable order for enable/disable/delete). |
| Clone costs for snapshot | `McpRegistry` is ~100-200 entries. Clone is sub-millisecond. If profiling shows otherwise, use `Arc<HashMap>` with atomic swap. |
| Backward compatibility | The trait interface matches current API surface. Axum handlers call `plugin_manager.register_tools()` instead of `tool_registry.write().await.register_all()`. Same effect. |

### 6.2 Benefits

- **Zero lock contention** on hot path (tool execution snapshot)
- **Deterministic plugin lifecycle** — no race between enable/disable/delete
- **Graceful subprocess management** — no mid-call kills
- **Observability** — actor can log every command for debugging
- **Testability** — mock `PluginManager` trait for unit tests
- **Isolation** — one plugin's hang can't block plugin management

---

## 7. Implementation Plan

### Phase 1 (1-2 days): Trait Extraction
```
Files: src/agent/plugin_manager.rs (NEW)
       src/server/plugins.rs (PATCH)
       src/server/plugins_enable.rs (PATCH)
       src/server/plugins_delete.rs (PATCH)
       src/server/plugins_reload.rs (PATCH)
       src/server/plugins_env.rs (PATCH)
       src/agent/executor.rs (PATCH)
       src/scheduler.rs (PATCH)
```
- Define `PluginManager` trait with ~10 methods
- Implement `LegacyPluginManager` wrapping existing statics
- Wire into AppState
- Verify all tests pass (102/106 → same failures)

### Phase 2 (3-5 days): Actor Implementation
```
Files: src/agent/plugin_manager.rs (MAJOR)
       src/mcp/external/client.rs (MAJOR refactor)
       src/server/mod.rs (PATCH — remove tool_registry from AppState)
       src/main.rs (PATCH)
```
- Implement `ActorPluginManager` with mpsc channel + tokio::spawn
- Move MCP pool management into actor
- Move server config management into actor
- Move platform restart signals into actor
- Remove global statics from client.rs

### Phase 3 (1-2 days): Swap + Cleanup
```
Files: src/main.rs (PATCH — swap implementations)
       src/mcp/external/client.rs (CLEANUP — remove statics)
       src/server/mod.rs (CLEANUP — remove old fields)
```
- Swap `AppState.plugin_manager` to `ActorPluginManager`
- Remove `state.tool_registry`, `state.platform_restart_signals`
- Remove `clear_server_pools()`, `register_server_config()`, `remove_server_config()`, `register_client()`
- Run full test suite

### Phase 4 (optional, 2-3 days): Platform Supervisor
```
Files: src/agent/plugin_manager.rs (EXPAND)
       src/platform/external/client.rs (REFACTOR or REMOVE)
```
- Move platform spawn loop logic into actor as PlatformSupervisor
- Replace AtomicU64 + Notify with watch::channel
- Remove stale Notify bit workarounds
- Simplify to 3 states: Stopped, Running, Draining

---

## 8. Verification

After migration, the following invariants must hold:

1. **No `Lazy<Mutex<...>>` statics in `src/mcp/external/client.rs`** (zero count)
2. **No `Arc<RwLock<McpRegistry>>` in AppState** (removed or replaced)
3. **No direct calls to `clear_server_pools()` from Axum handlers** (all go through trait)
4. **DELETE of any plugin completes in < 1s** (not 25s)
5. **All 106 plugin integration tests pass**
6. **Concurrent requests** (2 DELETE, 1 enable, 1 tool call) complete without errors
7. **Config update for one plugin does not affect other plugins' subprocesses**
