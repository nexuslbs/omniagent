//! Concurrency semantics of the plugin lifecycle coordinator
//! (`omniagent::plugin::lifecycle`).
//!
//! These cover the contract from the bulk-restart incident:
//!   * one in-flight operation per plugin (FIFO) and bounded parallelism across
//!     plugins, so a burst of 10 disable+enable pairs cannot become an
//!     unbounded subprocess storm;
//!   * N concurrent enable calls for one plugin start exactly ONE process;
//!   * a disable arriving while a restart is in flight ends with the plugin
//!     STOPPED;
//!   * a hung plugin is bounded by a timeout and reported as a per-plugin error
//!     instead of wedging the caller.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omniagent::plugin::lifecycle::{
    step_timeout_with, PluginLifecycle, MAX_CONCURRENT_LIFECYCLE_OPS,
};

/// Same-plugin operations never overlap and run in FIFO order.
#[tokio::test]
async fn same_plugin_ops_are_serialized_fifo() {
    let c = PluginLifecycle::new();
    let live = Arc::new(AtomicU64::new(0));
    let max_live = Arc::new(AtomicU64::new(0));

    let mut futs = Vec::new();
    for i in 0..5u64 {
        let live = live.clone();
        let max_live = max_live.clone();
        let c = &c;
        futs.push(async move {
            c.run("tool/alpha", async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                max_live.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(15)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                i
            })
            .await
        });
    }
    let order = futures::future::join_all(futs).await;

    assert_eq!(
        max_live.load(Ordering::SeqCst),
        1,
        "two lifecycle ops for the same plugin overlapped"
    );
    assert_eq!(order, vec![0, 1, 2, 3, 4], "per-plugin gate must be FIFO");
    assert_eq!(c.completed_ops(), 5);
}

/// Cross-plugin concurrency is bounded (no unbounded process storm).
#[tokio::test]
async fn cross_plugin_concurrency_is_bounded() {
    let c = PluginLifecycle::new();
    let live = Arc::new(AtomicU64::new(0));
    let max_live = Arc::new(AtomicU64::new(0));

    let mut futs = Vec::new();
    for i in 0..12u32 {
        let live = live.clone();
        let max_live = max_live.clone();
        let c = &c;
        let key = format!("tool/p{}", i);
        futs.push(async move {
            c.run(&key, async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                max_live.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            })
            .await
        });
    }
    futures::future::join_all(futs).await;

    let peak = max_live.load(Ordering::SeqCst);
    assert!(
        peak <= MAX_CONCURRENT_LIFECYCLE_OPS as u64,
        "peak concurrency {} exceeded the bound {}",
        peak,
        MAX_CONCURRENT_LIFECYCLE_OPS
    );
    assert!(peak > 1, "the bound must still allow some parallelism");
}

/// N concurrent enable calls for one plugin start exactly one process.
#[tokio::test]
async fn concurrent_enables_start_once() {
    let c = PluginLifecycle::new();
    let started = Arc::new(AtomicU64::new(0));
    let enabled = Arc::new(AtomicBool::new(false));

    let mut futs = Vec::new();
    for _ in 0..8 {
        let started = started.clone();
        let enabled = enabled.clone();
        let c = &c;
        futs.push(async move {
            c.run("tool/beta", async move {
                // Emulates the handler's idempotent "already enabled" check,
                // which now runs inside the per-plugin gate.
                if !enabled.swap(true, Ordering::SeqCst) {
                    started.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await
        });
    }
    futures::future::join_all(futs).await;

    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "concurrent enables for one plugin must start it once"
    );
}

/// A disable arriving while a restart is in flight ends with the plugin STOPPED
/// (the queued disable is applied after the restart finished).
#[tokio::test]
async fn disable_during_restart_ends_stopped() {
    let c = PluginLifecycle::new();
    let state = Arc::new(Mutex::new("stopped".to_string()));

    let restart = {
        let state = state.clone();
        let c = &c;
        async move {
            c.run("tool/gamma", async move {
                *state.lock().unwrap() = "running".to_string();
                tokio::time::sleep(Duration::from_millis(30)).await;
                *state.lock().unwrap() = "running".to_string();
            })
            .await
        }
    };
    let disable = {
        let state = state.clone();
        let c = &c;
        async move {
            c.run("tool/gamma", async move {
                *state.lock().unwrap() = "stopped".to_string();
            })
            .await
        }
    };
    futures::future::join(restart, disable).await;

    assert_eq!(*state.lock().unwrap(), "stopped");
}

/// A hung plugin step is bounded and reported as a per-plugin error, so the
/// caller (and therefore the API) never wedges on it.
#[tokio::test]
async fn hung_step_is_bounded() {
    let res = step_timeout_with(Duration::from_millis(40), "tool 'hung' MCP init", async {
        std::future::pending::<()>().await;
        Ok::<u32, String>(0)
    })
    .await;

    let err = res.expect_err("a hung step must time out");
    assert!(err.contains("timed out"), "unexpected error: {}", err);
    assert!(
        err.contains("tool 'hung' MCP init"),
        "unexpected error: {}",
        err
    );
}

/// A fast step passes through unchanged (no timeout side effects).
#[tokio::test]
async fn fast_step_is_forwarded() {
    let res = step_timeout_with(Duration::from_secs(5), "tool 'fast' MCP init", async {
        Ok::<u32, String>(7)
    })
    .await;
    assert_eq!(res.ok(), Some(7));
}
