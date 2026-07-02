use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use omniagent::{agent, db, mcp, platform, scheduler, server, vectorizer};
use omniagent::error::{AppResult, Error};

/// OmniAgent — autonomous agent system with Postgres, pgvector, MCP tools.

/// Read an environment variable with a fallback default value.
fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> AppResult<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    run_server().await
}

// ── Server mode (original) ──────────────────────────────────────────────────

async fn run_server() -> AppResult<()> {
    // Initialize tracing — JSON format for journald -> Vector -> Loki -> Grafana
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stdout)
        .init();

    tracing::info!("OmniAgent starting...");

    // Load base configuration
    let cfg = agent::AgentConfig::from_env()?;
    tracing::info!("Configuration loaded");

    // Initialize global config — shared Arc<RwLock<>> for hot-reload
    let shared_config = agent::config::init_global(cfg.clone());
    tracing::info!("Global config initialized");

    // Connect to PostgreSQL
    let pool = db::connect(&cfg.database_url).await?;
    tracing::info!("Connected to PostgreSQL");

    // Run migrations
    db::migrations::run(&pool).await.map_err(|e| Error::Message(format!("Migration failed: {}", e)))?;
    tracing::info!("Database migrations completed");

    // Determine data directory (default: /opt/data)
    let data_dir = env_or_default("OMNI_DIR", "/opt/data");
    tracing::info!("Data directory: {}", data_dir);

    // Determine workspace directory (default: /opt/workspace)
    let workspace_dir = env_or_default("WORKSPACE_DIR", "/opt/workspace");
    tracing::info!("Workspace directory: {}", workspace_dir);

    tracing::info!(
        "Agent config — provider: {}, max_tokens: {}, temperature: {}",
        cfg.llm_provider,
        cfg.max_tokens,
        cfg.temperature,
    );
    tracing::info!(
        "Iteration limits — no_plan: {}, simple_plan: {}, complex_plan: {}",
        cfg.max_iterations_no_plan,
        cfg.max_iterations_simple_plan,
        cfg.max_iterations_complex_plan,
    );

    // Create shared platform restart signals map (for hot-reload)
    let platform_restart_signals: Arc<Mutex<HashMap<String, (Arc<AtomicBool>, Arc<Notify>)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Create platform registry and register platforms
    let mut registry = platform::PlatformRegistry::new();

    // Load external platform plugins from config
    let external_plugins = platform::external::load_plugins_config(&data_dir);
    for plugin_config in &external_plugins {
        if !plugin_config.enabled {
            tracing::info!("Skipping disabled platform plugin: {}", plugin_config.name);
            continue;
        }
        tracing::info!(
            "Registering external platform plugin: {} (command: {} {})",
            plugin_config.name,
            plugin_config.command,
            plugin_config.args.join(" ")
        );
        let client = platform::external::client::ExternalPlatformClient::new(
            plugin_config.clone(),
            &data_dir,
            platform_restart_signals.clone(),
        )
        .await;
        registry.register(Box::new(client));
    }

    let platform_senders = registry.clone_senders();
    let _platform_handles = registry.start_all(pool.clone());

    // Create AppContext and MCP registry
    let readonly_pool = db::connect(&cfg.database_readonly_url).await?;
    let mut ctx = mcp::AppContext::new(
        pool.clone(),
        readonly_pool,
        &data_dir,
        &workspace_dir,
        Some(cfg.qdrant_url.clone()),
        platform_senders,
    );
    let mcp = mcp::default_registry(&mut ctx).await;
    let mcp_shared = Arc::new(RwLock::new(mcp));

    // Build the agent with shared mutable config
    let shared_config_for_agent = shared_config.clone();
    let agent = agent::Agent::new(pool.clone(), shared_config_for_agent, mcp_shared.clone(), ctx.clone());

    // ── STARTUP: Skip pending/processing messages BEFORE spawning any concurrent tasks ──

    // Shared cancellation tokens for /stop endpoint
    let cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let cancel_tokens_agent = cancel_tokens.clone();
    let cancel_tokens_server = cancel_tokens.clone();

    // Spawn the agent supervisor (parallel channel processing)
    let agent_handle = tokio::spawn(async move {
        agent.run(cancel_tokens_agent).await;
    });

    // Spawn HTTP server (health, /stop endpoint)
    let pool_server = pool.clone();
    let server_host = cfg.host.clone();
    let server_port = cfg.port;
    let data_dir_server = data_dir.clone();
    let mcp_for_server = mcp_shared.clone();
    let ctx_for_server = ctx.clone();
    let shared_config_for_server = shared_config.clone();
    let platform_restart_signals_for_server = platform_restart_signals.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::start_server(server::ServerConfig {
            pool: pool_server,
            host: server_host,
            port: server_port,
            cancel_tokens: cancel_tokens_server,
            data_dir: data_dir_server,
            workspace_dir,
            mcp_registry: mcp_for_server,
            app_context: ctx_for_server,
            shared_config: shared_config_for_server,
            platform_restart_signals: platform_restart_signals_for_server,
        })
        .await
        {
            tracing::error!("HTTP server error: {:?}", e);
        }
    });

    tracing::info!(
        "OmniAgent is ready! HTTP server on {}:{}",
        cfg.host,
        cfg.port
    );

    // Spawn old-message deletion task (daily cleanup)
    let pool_clean = pool.clone();
    let delete_after_days = cfg.delete_after_days;
    let cleanup_handle = tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(86400); // daily
        loop {
            tokio::time::sleep(interval).await;
            let before = chrono::Utc::now() - chrono::Duration::days(delete_after_days as i64);
            // Delete old messages
            match db::types::delete_old_messages(&pool_clean, before).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            "Deleted {} messages older than {} days",
                            count,
                            delete_after_days
                        );
                    }
                }
                Err(e) => tracing::error!("Failed to delete old messages: {:?}", e),
            }
            // Delete old summaries
            match db::types::delete_old_summaries(&pool_clean, before).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            "Deleted {} summaries older than {} days",
                            count,
                            delete_after_days
                        );
                    }
                }
                Err(e) => tracing::error!("Failed to delete old summaries: {:?}", e),
            }
        }
    });

    // Spawn vectorization workers if enabled
    let pool_vectorizer = pool.clone();
    let data_dir_for_vectorizer = data_dir.clone();
    let shared_config_for_vectorizer = shared_config.clone();
    let vectorizer_handle = tokio::spawn(async move {
        vectorizer::spawn_vectorizers(pool_vectorizer, shared_config_for_vectorizer, &data_dir_for_vectorizer).await;
    });

    // Spawn cron scheduler
    let cron_handle = scheduler::spawn(pool.clone(), data_dir.clone(), mcp_shared.clone(), ctx.clone());

    // Graceful shutdown
    tokio::select! {
        _ = agent_handle => {
            tracing::info!("Agent loop finished");
        }
        _ = server_handle => {
            tracing::info!("Server finished");
        }
        _ = cleanup_handle => {
            tracing::info!("Cleanup finished");
        }
        _ = vectorizer_handle => {
            tracing::info!("Vectorizer finished");
        }
        _ = cron_handle => {
            tracing::info!("Cron scheduler finished");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl+C, shutting down...");
        }
    }

    tracing::info!("OmniAgent shutdown complete");
    Ok(())
}
