use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod agent;
mod config;
mod db;
mod llm;
mod mcp;
mod models;
mod platform;
mod profile;
mod prompt_builder;
mod server;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!("OmniAgent starting...");

    // Load base configuration
    let cfg = config::Config::from_env()?;
    tracing::info!("Configuration loaded");

    // Connect to PostgreSQL
    let pool = db::connect(&cfg.database_url).await?;
    tracing::info!("Connected to PostgreSQL");

    // Run migrations
    db::migrations::run(&pool).await?;
    tracing::info!("Database migrations completed");

    // Determine data directory (default: /opt/data)
    let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());
    tracing::info!("Data directory: {}", data_dir);

    // Build agent config from environment
    let agent_cfg = agent::AgentConfig::from_env()?;
    tracing::info!(
        "Agent config — model: {}, provider: {}, max_tokens: {}, temperature: {}, max_iterations: {}",
        agent_cfg.llm_model,
        agent_cfg.llm_provider,
        agent_cfg.max_tokens,
        agent_cfg.temperature,
        agent_cfg.max_iterations,
    );

    // Create AppContext and MCP registry
    let ctx = mcp::AppContext::new(pool.clone(), &data_dir, Some(cfg.qdrant_url.clone()));
    let mcp = mcp::default_registry(&ctx);

    // Build the agent with MCP context
    let agent = agent::Agent::new(pool.clone(), agent_cfg, mcp, ctx);

    // Shared cancellation tokens for /stop endpoint
    let cancel_tokens: Arc<Mutex<HashMap<i64, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let cancel_tokens_agent = cancel_tokens.clone();
    let cancel_tokens_server = cancel_tokens.clone();

    // Spawn the agent supervisor (parallel channel processing)
    let agent_handle = tokio::spawn(async move {
        agent.run(cancel_tokens_agent).await;
    });

    // Create platform registry and register built-in platforms
    let mut registry = platform::PlatformRegistry::new();
    registry.register(Box::new(platform::TelegramPlatform::new()));
    let _platform_handles = registry.start_all(pool.clone());

    // Spawn HTTP server (health, /stop endpoint)
    let pool_server = pool.clone();
    let server_host = cfg.host.clone();
    let server_port = cfg.port;
    let server_handle = tokio::spawn(async move {
        server::start_server(pool_server, server_host, server_port, cancel_tokens_server).await;
    });

    tracing::info!(
        "OmniAgent is ready! HTTP server on {}:{}",
        cfg.host,
        cfg.port
    );

    // Spawn old-message deletion task (daily cleanup)
    let pool_clean = pool.clone();
    let delete_after_days = std::env::var("DELETE_AFTER_DAYS")
        .unwrap_or_else(|_| "30".to_string())
        .parse::<u32>()
        .unwrap_or(30);
    let cleanup_handle = tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(86400); // daily
        loop {
            tokio::time::sleep(interval).await;
            let before = chrono::Utc::now() - chrono::Duration::days(delete_after_days as i64);
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
        }
    });

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
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl+C, shutting down...");
        }
    }

    tracing::info!("OmniAgent shutdown complete");
    Ok(())
}
