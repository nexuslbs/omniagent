//! Bounded, single-flight coordination for plugin lifecycle operations.
//!
//! Bulk lifecycle bursts (e.g. the dashboard restarting 10 plugins, i.e. 10
//! disable + enable pairs, or two operators clicking at the same time) used to
//! run completely unbounded:
//!
//!   * every HTTP request spawned/killed subprocesses in parallel, and every
//!     handler did its own `plugins.yml` read-modify-write, so concurrent calls
//!     lost entries and collided on the shared `.tmp` staging file
//!     (`Failed to rename .../plugins.yml.tmp`, HTTP 500);
//!   * the MCP handshake ran inside the plugin-manager actor loop, so every
//!     other API call that needs the tool registry (GET /api/tools, prompt
//!     building, the executor snapshot) waited behind the restart.
//!
//! This module is the single authority for lifecycle ordering:
//!
//!   * [`PluginLifecycle::run`] serializes every operation that targets the SAME
//!     plugin (per-plugin FIFO gate). N concurrent enable calls for one plugin
//!     therefore perform exactly ONE start (the queue plus the handler's
//!     idempotent "already enabled" check), and a disable arriving while a
//!     restart is in flight is applied after it, so the plugin always ends
//!     STOPPED;
//!   * the number of lifecycle operations running concurrently ACROSS plugins is
//!     bounded by a semaphore, so a burst of 10 restarts cannot turn into an
//!     unbounded subprocess storm;
//!   * no lock used by read paths is held while an operation awaits, so
//!     GET /api/plugins, GET /api/tools and registry snapshots stay responsive
//!     while restarts are in flight.
//!
//! The coordinator is plugin-agnostic: a gate key is just `<type>/<name>`. No
//! plugin, platform, provider or tool name is special-cased anywhere in core.
//!
//! The concurrency semantics are covered by
//! `tests/plugin_lifecycle_concurrency.rs`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

/// Maximum number of plugin lifecycle operations (enable / disable / restart)
/// allowed to run concurrently across all plugins.
///
/// Chosen to overlap subprocess startup latency (spawn + MCP handshake) without
/// letting a 10-plugin burst become 10 simultaneous process spawns.
pub const MAX_CONCURRENT_LIFECYCLE_OPS: usize = 4;

/// Upper bound for a single slow lifecycle step (subprocess spawn + handshake).
///
/// A plugin that never answers must not wedge the API or hold its lifecycle
/// gate forever: the step is aborted and reported as a per-plugin failure while
/// every other plugin keeps working.
pub const LIFECYCLE_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Process-wide lifecycle coordinator (one per agent process).
pub struct PluginLifecycle {
    /// One async mutex per plugin key: FIFO queue of the operations targeting
    /// that plugin. The std mutex only guards the map lookup (never held across
    /// an await).
    gates: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Global bound on concurrently running lifecycle operations.
    permits: Arc<Semaphore>,
    completed_ops: AtomicU64,
    timed_out_ops: AtomicU64,
}

impl PluginLifecycle {
    /// Create an isolated coordinator (used by the tests; the runtime uses
    /// [`PluginLifecycle::global`]).
    pub fn new() -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_LIFECYCLE_OPS)),
            completed_ops: AtomicU64::new(0),
            timed_out_ops: AtomicU64::new(0),
        }
    }

    /// The process-wide coordinator used by the plugin HTTP handlers.
    pub fn global() -> &'static PluginLifecycle {
        static GLOBAL: OnceLock<PluginLifecycle> = OnceLock::new();
        GLOBAL.get_or_init(PluginLifecycle::new)
    }

    /// Run `op` as a lifecycle operation for `key` (`<type>/<name>`).
    ///
    /// Guarantees:
    ///   * only one operation per key runs at a time, in FIFO order, so the LAST
    ///     operation for a plugin decides its end state;
    ///   * at most [`MAX_CONCURRENT_LIFECYCLE_OPS`] operations run at a time;
    ///   * no shared/global lock is held across the awaits inside `op`.
    pub async fn run<F, T>(&self, key: &str, op: F) -> T
    where
        F: Future<Output = T>,
    {
        // Per-plugin first: callers for the same plugin queue here (FIFO), so a
        // disable issued during a restart is applied after the restart and the
        // plugin ends STOPPED.
        let gate = self.gate_for(key);
        let _plugin_guard = gate.lock_owned().await;
        // Then the global bound: never more than N subprocess operations at once.
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("plugin lifecycle semaphore is never closed");
        let out = op.await;
        self.completed_ops.fetch_add(1, Ordering::Relaxed);
        out
    }

    /// Number of lifecycle operations that finished (observability/tests).
    pub fn completed_ops(&self) -> u64 {
        self.completed_ops.load(Ordering::Relaxed)
    }

    /// Number of lifecycle steps aborted by a timeout (observability/tests).
    pub fn timed_out_ops(&self) -> u64 {
        self.timed_out_ops.load(Ordering::Relaxed)
    }

    /// The (stable) gate for one plugin key.
    fn gate_for(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

impl Default for PluginLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

/// Await a slow lifecycle step with the default bounded timeout.
///
/// `what` names the step (e.g. `"tool 'web' MCP init"`) and shows up in the
/// per-plugin error message when the step times out.
pub async fn step_timeout<F, T>(what: &str, fut: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    step_timeout_with(LIFECYCLE_STEP_TIMEOUT, what, fut).await
}

/// [`step_timeout`] with an explicit timeout (tests use a short one).
pub async fn step_timeout_with<F, T>(timeout: Duration, what: &str, fut: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => {
            PluginLifecycle::global()
                .timed_out_ops
                .fetch_add(1, Ordering::Relaxed);
            Err(format!(
                "{} timed out after {}s (plugin unresponsive; step aborted, other plugins unaffected)",
                what,
                timeout.as_secs_f32()
            ))
        }
    }
}
