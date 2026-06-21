use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod agent;
mod config;
mod context_builder;
mod db;
mod llm;
mod mcp;
mod models;
mod platform;
mod plugin;
mod profile;
mod prompt_builder;
mod relevance;
mod scheduler;
mod server;
mod subtask;
mod vectorizer;

use crate::platform::OutboundSender;

/// OmniAgent — autonomous agent system with Postgres, pgvector, MCP tools.
#[derive(Parser, Debug)]
#[command(name = "omniagent", about = "OmniAgent — autonomous agent system")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Run the full server (default when no subcommand is given)
    Server,
    /// Interactive CLI client — sends messages to an agent channel
    Cli {
        /// Channel name for the CLI session
        #[arg(long, default_value = "cli")]
        channel: String,

        /// Profile to use (default profile's model/provider if omitted)
        #[arg(long, default_value = "default")]
        profile: String,

        /// Model override (use profile model if omitted)
        #[arg(long)]
        model: Option<String>,

        /// Provider override (use profile provider if omitted)
        #[arg(long)]
        provider: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Determine data directory (default: /opt/data)
    let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());

    match cli.command.unwrap_or(Command::Server) {
        Command::Server => run_server().await,
        Command::Cli { channel, profile, model, provider } => run_cli(channel, profile, model, provider, &data_dir).await,
    }
}

// ── Server mode (original) ──────────────────────────────────────────────────

async fn run_server() -> Result<()> {
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

    // Sync plugins from disk after migrations
    let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    if let Err(e) = plugin::sync_plugins_from_disk(&pool, &data_dir).await {
        tracing::warn!("Plugin sync failed (non-fatal): {:?}", e);
    }

    // Determine data directory (default: /opt/data)
    let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());
    tracing::info!("Data directory: {}", data_dir);

    // Determine workspace directory (default: /opt/workspace)
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_else(|_| "/opt/workspace".to_string());
    tracing::info!("Workspace directory: {}", workspace_dir);

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

    // Create platform registry and register platforms
    let mut registry = platform::PlatformRegistry::new();

    // Built-in Telegram platform (kept for backward compatibility)
    registry.register(Box::new(crate::platform::telegram::TelegramPlatform::new()));

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
        let client = platform::external::client::ExternalPlatformClient::new(plugin_config.clone());
        registry.register(Box::new(client));
    }

    let platform_senders = registry.clone_senders();
    let _platform_handles = registry.start_all(pool.clone());

    // Create AppContext and MCP registry
    let readonly_pool = db::connect(&cfg.database_readonly_url).await?;
    let ctx = mcp::AppContext::new(
        pool.clone(),
        readonly_pool,
        &data_dir,
        &workspace_dir,
        Some(cfg.qdrant_url.clone()),
        platform_senders,
    );
    let mcp = mcp::default_registry(&ctx);

    // Build the agent with MCP context
    let agent = agent::Agent::new(pool.clone(), agent_cfg.clone(), mcp.clone(), ctx.clone());

    // ── STARTUP: Skip pending/processing messages BEFORE spawning any concurrent tasks ──
    if let Err(e) = agent::skip_on_startup(&pool).await {
        tracing::error!(
            "Failed to skip pending/processing messages on startup: {:?}",
            e
        );
    }

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
    let server_handle = tokio::spawn(async move {
        server::start_server(
            pool_server,
            server_host,
            server_port,
            cancel_tokens_server,
            data_dir_server,
            mcp,
            ctx,
        )
        .await;
    });

    tracing::info!(
        "OmniAgent is ready! HTTP server on {}:{}",
        cfg.host,
        cfg.port
    );

    // Spawn old-message deletion task (daily cleanup)
    let pool_clean = pool.clone();
    let delete_after_days = agent_cfg.delete_after_days;
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
    let vectorizer_handle = tokio::spawn(async move {
        vectorizer::spawn_vectorizers(pool_vectorizer, &cfg, &data_dir_for_vectorizer).await;
    });

    // Spawn cron scheduler
    let cron_handle = scheduler::spawn(pool.clone(), data_dir.clone());

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

// ── CLI mode ────────────────────────────────────────────────────────────────

use sqlx::PgPool;
use std::io::{self, BufRead, Write};

/// Run the interactive CLI client.
/// Connects to Postgres, creates/finds a CLI channel, reads stdin, sends
/// messages as pending, polls for agent responses, and prints them.
async fn run_cli(channel_name: String, profile_name: String, model: Option<String>, provider: Option<String>, data_dir: &str) -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")?;
    let pool = db::connect(&database_url).await?;

    // Find or create the CLI channel
    let channel = ensure_cli_channel(&pool, &channel_name, &profile_name, model.as_deref(), provider.as_deref()).await?;
    let mut current_channel_id = channel.id;
    let mut current_channel_name = channel.name.clone();

    // Generate a unique session ID for this CLI process
    let mut session_id = format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // Claim this channel for our session (will notify old session if reassigned)
    crate::platform::claim_channel(&pool, current_channel_id, &session_id, "cli", None).await?;

    // Resolve provider+model for stamping on all seq-0 messages in this session
    // Order: channel.current_provider → profile provider → env → default
    let profile_registry_cfg = crate::profile::ProfileRegistry::new(data_dir);
    let cli_prof = profile_registry_cfg.get(&profile_name).cloned().unwrap_or_else(|| {
        crate::profile::Profile::default("default")
    });
    let resolved_provider: String = channel.current_provider.clone()
        .or_else(|| cli_prof.provider.clone())
        .or_else(|| Some(std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "opencode-go".to_string())))
        .unwrap_or_else(|| "opencode-go".to_string());
    let resolved_model: String = channel.current_model.clone()
        .or_else(|| cli_prof.model.clone())
        .or_else(|| Some(std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string())))
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());

    println!("\n┌─────────────────────────────────────────────────────────┐");
    println!("│  OmniAgent CLI — channel: {}, profile: {}  │", current_channel_name, channel.current_profile);
    println!("│  Type your messages. /exit to quit. /new for new channel  │");
    println!("│  /channel to create or claim a channel                   │");
    println!("│  /subscribe <name> to receive summaries from a channel    │");
    println!("│  /unsubscribe <name> to stop receiving summaries          │");
    println!("│  /subscriptions to list your current subscriptions        │");
    println!("│  /usage to show token usage stats per channel              │");
    println!("└─────────────────────────────────────────────────────────┘\n");

    // Get or create the current thread
    let mut thread_id = get_or_create_thread(&pool, current_channel_id, &profile_name, &resolved_provider, &resolved_model).await?;
    // Mark the /start thread as a system thread (terminal, never processed by executor)
    db::types::set_thread_system(&pool, thread_id).await?;
    let _ = get_next_sequence(&pool, current_channel_id, thread_id).await?;

    // Initialize cursor-based polling tracker — skip existing messages
    let mut last_seen_id: i64 = get_max_message_id(&pool, current_channel_id).await?;
    let mut last_seen_summary_id: i64 = 0;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    let mut session_has_channel = true;

    loop {
        print!("> ");
        io::stdout().flush()?;
        line.clear();

        if reader.read_line(&mut line)? == 0 {
            // EOF
            break;
        }

        let input = line.trim().to_string();
        if input.is_empty() {
            continue;
        }

        match input.as_str() {
            "/exit" | "/quit" => {
                println!("Goodbye.");
                break;
            }
            "/channel" => {
                match handle_channel_command(
                    &pool,
                    &mut reader,
                    &session_id,
                )
                .await?
                {
                    Some((new_id, new_name)) => {
                        current_channel_id = new_id;
                        current_channel_name = new_name;
                        session_has_channel = true;
                        // Start a fresh thread on the new channel
                        thread_id = get_or_create_thread(
                            &pool,
                            current_channel_id,
                            &profile_name,
                            &resolved_provider,
                            &resolved_model,
                        )
                        .await?;
                        let _ = get_next_sequence(&pool, current_channel_id, thread_id).await?;
                        last_seen_id = get_max_message_id(&pool, current_channel_id).await?;
                        last_seen_summary_id = 0;
                        println!(
                            "Switched to channel '{}' (id={}).",
                            current_channel_name, current_channel_id
                        );
                    }
                    None => {
                        // User cancelled
                    }
                }
                continue;
            }
            "/new" if !session_has_channel => {
                eprintln!(
                    "\n[ERR_SESSION_NO_CHANNEL] This session no longer has a channel. \
                     Use /channel to claim one.\n"
                );
                continue;
            }
            "/new" => {
                // Transactional /new: atomically un-claim the old channel,
                // create a new channel with the existing session_id as
                // resource_identifier, and set profile/model — all in one
                // transaction so the session never has no channel.
                // session_id stays unchanged — session keeps owning the new channel.
                use std::time::{SystemTime, UNIX_EPOCH};
                use sql_forge::sql_forge;
                let unique_suffix = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let auto_name = format!("cli-new-{}-{}", std::process::id(), unique_suffix);

                // Begin a single SQL transaction
                let mut tx = pool.begin().await?;

                // 1. Un-claim the current channel (set resource_identifier to
                //    the channel name so it's available for the next session,
                //    while avoiding NOT NULL on external_id and the UNIQUE
                //    constraint on (platform, resource_identifier))
                sql_forge!(
                    r#"
                    UPDATE channels
                    SET resource_identifier = name,
                        external_id = name,
                        updated_at = NOW()
                    WHERE id = :id
                    "#,
                    ( :id = current_channel_id )
                )
                .execute(&mut *tx)
                .await?;

                // 2. Create a new channel with the existing session_id as resource_identifier
                #[derive(Debug, sqlx::FromRow, Clone)]
                struct NewChannelRow {
                    id: i64,
                    name: String,
                }

                let new_channel_row: NewChannelRow = sql_forge!(
                    NewChannelRow,
                    r#"
                    INSERT INTO channels (name, platform, external_id, cause, resource_identifier)
                    VALUES (:name, 'cli', :name, 'user', :session_id)
                    RETURNING id, name
                    "#,
                    ( :name = &auto_name, :session_id = &session_id )
                )
                .fetch_one(&mut *tx)
                .await?;

                // 3. Set profile/model on the new channel
                sql_forge!(
                    r#"
                    UPDATE channels
                    SET current_profile = :profile,
                        current_model = NULLIF(:model, '')::text,
                        current_provider = NULLIF(:provider, '')::text,
                        updated_at = NOW()
                    WHERE id = :id
                    "#,
                    ( :profile = &profile_name,
                      :model = &resolved_model,
                      :provider = &resolved_provider,
                      :id = new_channel_row.id )
                )
                .execute(&mut *tx)
                .await?;

                // Commit — session always has a channel
                tx.commit().await?;

                current_channel_id = new_channel_row.id;
                current_channel_name = auto_name.clone();
                session_has_channel = true;
                // session_id unchanged — session keeps owning the new channel
                last_seen_id = get_max_message_id(&pool, current_channel_id).await?;
                last_seen_summary_id = 0;

                // Start a fresh thread on the new channel
                thread_id = get_or_create_thread(
                    &pool,
                    current_channel_id,
                    &profile_name,
                    &resolved_provider,
                    &resolved_model,
                ).await?;
                let _ = get_next_sequence(&pool, current_channel_id, thread_id).await?;

                println!(
                    "Created and claimed new channel '{}' (id={}).",
                    auto_name, new_channel_row.id
                );
                continue;
            }
            cmd if cmd.starts_with("/subscribe ") => {
                let name = cmd.trim_start_matches("/subscribe ").trim().to_string();
                if name.is_empty() {
                    println!("Usage: /subscribe <channel_name>");
                    continue;
                }
                match db::types::get_channel_by_name(&pool, &name).await? {
                    Some(ch) => {
                        let sub_id = db::types::add_subscription(
                            &pool, ch.id, "cli", &session_id,
                        ).await?;
                        println!(
                            "Subscribed to summaries from channel '{}' (sub_id={}).",
                            ch.name, sub_id
                        );
                    }
                    None => {
                        println!("Channel '{}' not found.", name);
                    }
                }
                continue;
            }
            cmd if cmd.starts_with("/unsubscribe ") => {
                let name = cmd.trim_start_matches("/unsubscribe ").trim().to_string();
                if name.is_empty() {
                    println!("Usage: /unsubscribe <channel_name>");
                    continue;
                }
                match db::types::get_channel_by_name(&pool, &name).await? {
                    Some(ch) => {
                        let removed = db::types::remove_subscription(
                            &pool, ch.id, "cli", &session_id,
                        ).await?;
                        if removed {
                            println!(
                                "Unsubscribed from summaries from channel '{}'.",
                                ch.name
                            );
                        } else {
                            println!(
                                "Not currently subscribed to channel '{}'.",
                                ch.name
                            );
                        }
                    }
                    None => {
                        println!("Channel '{}' not found.", name);
                    }
                }
                continue;
            }
            "/usage" => {
                handle_usage_command(&pool).await?;
                continue;
            }
            cmd if cmd.starts_with("/action") => {
                handle_action_command(&pool, cmd).await?;
                continue;
            }
            "/subscriptions" => {
                let subs = db::types::get_subscriptions_for_subscriber(
                    &pool, "cli", &session_id,
                ).await?;
                if subs.is_empty() {
                    println!("You are not subscribed to any channels.");
                } else {
                    println!("Your subscriptions:");
                    for sub in &subs {
                        let ch_name = db::types::find_channel_by_id(&pool, sub.channel_id)
                            .await?
                            .map(|c| c.name)
                            .unwrap_or_else(|| format!("id={}", sub.channel_id));
                        println!("  - {} (channel_id={})", ch_name, sub.channel_id);
                    }
                }
                continue;
            }
            _ => {}
        }

        // Check if this session still owns the channel
        if session_has_channel {
            if let Some(ch) = db::types::find_channel_by_id(&pool, current_channel_id).await? {
                if ch.resource_identifier.as_deref() != Some(&session_id) {
                    session_has_channel = false;
                }
            }
        }

        if !session_has_channel {
            eprintln!(
                "\n[ERR_SESSION_NO_CHANNEL] This session no longer has a channel. \
                 Use /channel to claim one.\n"
            );
            continue;
        }

        // Insert user message as pending (it will be picked up by the executor)
        // For the CLI, we create a new thread for each user message
        let thread = db::types::create_thread(
            &pool,
            "user",
            current_channel_id,
            &profile_name,
            Some(&resolved_provider),
            Some(&resolved_model),
            None,
            None,
        ).await?;

        let msg = models::MessageNew {
            thread_id: thread.id,
            role: "cause".to_string(),
            content: input.clone(),
            thread_sequence: 0,
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "message".to_string(),
            msg_subtype: None,
            processing_time_ms: None,
            token_usage: None,
        };
        db::types::create_cause_and_set_pending(&pool, &msg).await?;

        // Poll for agent responses using cursor-based polling
        (last_seen_id, last_seen_summary_id) = poll_for_response(
            &pool, current_channel_id, last_seen_id, last_seen_summary_id, &session_id,
        ).await?;
    }

    Ok(())
}

/// Find or create a CLI channel.
async fn ensure_cli_channel(
    pool: &PgPool,
    channel_name: &str,
    profile_name: &str,
    model: Option<&str>,
    provider: Option<&str>,
) -> Result<models::Channel> {
    use sql_forge::sql_forge;

    // Try to find existing CLI channel by name
    let existing = sql_forge!(
        crate::db::types::ChannelDb,
        r#"
        SELECT
            id, name,
            COALESCE(platform, '') AS "platform",
            resource_identifier,
            COALESCE(external_id, '') AS "external_id",
            cause,
            current_profile, current_model, current_provider,
            readonly,
            COALESCE(closed, false) as "closed",
            '{}'::text AS "metadata",
            COALESCE(TO_CHAR(created_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "created_at",
            COALESCE(TO_CHAR(updated_at, 'YYYY-MM-DD"T"HH24' || CHR(58) || 'MI' || CHR(58) || 'SS.US"Z"'), '') AS "updated_at"
        FROM channels
        WHERE name = :name AND platform = 'cli'
        "#,
        ( :name = channel_name )
    )
    .fetch_optional(pool)
    .await?;

    if let Some(channel_db) = existing {
        // Update profile/model/provider if changed
        if channel_db.current_profile != profile_name
            || model.is_none_or(|m| channel_db.current_model.as_deref() != Some(m))
            || provider.is_none_or(|p| channel_db.current_provider.as_deref() != Some(p))
        {
            sql_forge!(
                r#"
                UPDATE channels
                SET current_profile = :profile,
                    current_model = :model,
                    current_provider = :provider,
                    updated_at = NOW()
                WHERE id = :id
                "#,
                ( :id = channel_db.id, :profile = profile_name, :model = model.unwrap_or(""), :provider = provider.unwrap_or("") )
            )
            .execute(pool)
            .await?;
        }
        let channel = models::Channel {
            id: channel_db.id,
            name: channel_db.name.clone(),
            platform: Some("cli".to_string()),
            resource_identifier: Some(channel_db.name.clone()),
            external_id: Some(channel_db.name),
            cause: "user".to_string(),
            current_profile: profile_name.to_string(),
            current_model: model.map(|m| m.to_string()),
            current_provider: provider.map(|p| p.to_string()),
            readonly: false,
            closed: false,
            metadata: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        return Ok(channel);
    }

    // Create new channel
    let new_channel = db::types::create_channel(pool, channel_name, "cli", channel_name, "user", channel_name).await?;
    // Update profile/model/provider
    sql_forge!(
        r#"
        UPDATE channels
        SET current_profile = :profile,
            current_model = :model,
            current_provider = :provider
        WHERE id = :id
        "#,
        ( :id = new_channel.id, :profile = profile_name, :model = model.unwrap_or(""), :provider = provider.unwrap_or("") )
    )
    .execute(pool)
    .await?;

    Ok(models::Channel {
        id: new_channel.id,
        name: channel_name.to_string(),
        platform: Some("cli".to_string()),
        resource_identifier: Some(channel_name.to_string()),
        external_id: Some(channel_name.to_string()),
        cause: "user".to_string(),
        current_profile: profile_name.to_string(),
        current_model: model.map(|m| m.to_string()),
        current_provider: provider.map(|p| p.to_string()),
        readonly: false,
        closed: false,
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

/// Get or create a thread for the CLI session.
async fn get_or_create_thread(pool: &PgPool, channel_id: i64, profile_name: &str, resolved_provider: &str, resolved_model: &str) -> Result<i64> {
    use sql_forge::sql_forge;
    use crate::db::types as queries;

    // Find the latest completed thread for this channel
    #[derive(Debug, sqlx::FromRow)]
    #[allow(dead_code)]
    struct LastThread {
        thread_id: Option<i64>,
        id: i64,
    }

    let latest: Option<LastThread> = sql_forge!(
        LastThread,
        r#"
        SELECT m.thread_id, m.id FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE t.channel_id = :channel_id
          AND m.thread_sequence = 0
          AND t.status = 'completed'
        ORDER BY m.id DESC
        LIMIT 1
        "#,
        ( :channel_id = channel_id )
    )
    .fetch_optional(pool)
    .await?;

    if let Some(row) = latest {
        if let Some(tid) = row.thread_id {
            return Ok(tid);
        }
    }

    // Create a new thread
    let thread = queries::create_thread(
        pool,
        "user",
        channel_id,
        profile_name,
        Some(resolved_provider),
        Some(resolved_model),
        None,
        None,
    ).await?;

    let root_msg = models::MessageNew {
        thread_id: thread.id,
        role: "cause".to_string(),
        content: "/start".to_string(),
        thread_sequence: 0,
        external_id: None,
        metadata: serde_json::json!({"cli_start": true}),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "message".to_string(),
        msg_subtype: None,
        processing_time_ms: None,
        token_usage: None,
    };
    let saved = queries::create_message(pool, &root_msg).await?;
    Ok(saved.thread_id)
}

/// Get the next thread_sequence for inserting a new user message.
async fn get_next_sequence(pool: &PgPool, channel_id: i64, thread_id: i64) -> Result<i32> {
    use sql_forge::sql_forge;

    #[derive(Debug, sqlx::FromRow)]
    struct MaxSeq {
        max_seq: Option<i32>,
    }

    let row: MaxSeq = sql_forge!(
        MaxSeq,
        "SELECT MAX(m.thread_sequence) AS \"max_seq\" FROM messages m JOIN threads t ON t.id = m.thread_id WHERE t.channel_id = :channel_id AND m.thread_id = :thread_id",
        ( :channel_id = channel_id, :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(row.max_seq.unwrap_or(0) + 1)
}

/// Get the maximum thread_sequence in a thread.
#[allow(dead_code)]
async fn get_max_sequence(pool: &PgPool, channel_id: i64, thread_id: i64) -> Result<i32> {
    get_next_sequence(pool, channel_id, thread_id).await.map(|n| n - 1)
}

/// Get the maximum message id for a given channel.
/// Used to initialize the polling cursor so the first poll skips existing messages.
async fn get_max_message_id(pool: &PgPool, channel_id: i64) -> Result<i64> {
    use sql_forge::sql_forge;

    #[derive(Debug, sqlx::FromRow)]
    struct MaxId {
        max_id: Option<i64>,
    }

    let row: MaxId = sql_forge!(
        MaxId,
        "SELECT MAX(m.id) AS \"max_id\" FROM messages m JOIN threads t ON t.id = m.thread_id WHERE t.channel_id = :channel_id",
        ( :channel_id = channel_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(row.max_id.unwrap_or(0))
}

/// Poll for agent responses after inserting a user message.
/// Uses cursor-based polling by message id (not thread_sequence).
/// Returns the latest message id processed as the cursor for the next call.
/// Polls every 500ms and prints messages as they arrive (all at once when
/// the thread completes, filtered by t.status = 'completed').
/// Before each cycle, checks that this session still owns the channel.
/// Also fetches and displays summaries from subscribed channels.
async fn poll_for_response(
    pool: &PgPool,
    channel_id: i64,
    last_seen_id: i64,
    last_seen_summary_id: i64,
    session_id: &str,
) -> Result<(i64, i64)> {
    use sql_forge::sql_forge;
    use tokio::time::{sleep, Duration};

    #[derive(Debug, sqlx::FromRow)]
    #[allow(dead_code)]
    struct MessageContentOnly {
        content: String,
    }

    #[derive(Debug, sqlx::FromRow)]
    #[allow(dead_code)]
    struct ResponseMsg {
        id: i64,
        thread_id: i64,
        role: String,
        content: String,
        msg_type: String,
        msg_subtype: Option<String>,
    }

    let mut last_seen = last_seen_id;
    let mut last_seen_summary = last_seen_summary_id;
    let timeout = Duration::from_secs(300); // 5 min max wait
    let poll_start = std::time::Instant::now();

    loop {
        // Check if this session still owns the channel (reassignment guard)
        if let Some(ch) = db::types::find_channel_by_id(pool, channel_id).await? {
            if ch.resource_identifier.as_deref() != Some(session_id) {
                println!(
                    "\n[ERR_SESSION_NO_CHANNEL] This session no longer has a channel. \
                     Use /channel to claim one.\n"
                );
                return Ok((last_seen, last_seen_summary));
            }
        }

        if poll_start.elapsed() > timeout {
            println!("[Timeout] Agent did not respond within 5 minutes.");
            return Ok((last_seen, last_seen_summary));
        }

        let responses: Vec<ResponseMsg> = sql_forge!(
            ResponseMsg,
            r#"
            SELECT m.id, m.thread_id, m.role, m.content, m.msg_type, m.msg_subtype
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE t.channel_id = :channel_id
              AND m.id > :last_seen
              AND t.status = 'completed'
            ORDER BY m.id ASC
            "#,
            ( :channel_id = channel_id, :last_seen = last_seen )
        )
        .fetch_all(pool)
        .await?;

        if responses.is_empty() {
            sleep(Duration::from_millis(500)).await;
            continue;
        }

        for msg in &responses {
            match msg.msg_type.as_str() {
                "tool" => {
                    let name = msg.msg_subtype.as_deref().unwrap_or("unknown");
                    println!("🔧 tool:{}", name);
                }
                "plan" => {
                    println!("🔧 tool:planned");
                }
                "reasoning" => {
                    println!("💭 reasoning");
                }
                "tool_result" => {
                    // Skip entirely — never display anything
                    continue;
                }
                "message" if msg.role == "agent" => {
                    println!("\n┌─ Agent ────────────────────────────");
                    for chunk in msg.content.split('\n') {
                        println!("│ {}", chunk);
                    }
                    println!("└─────────────────────────────────────");
                }
                "summary" => {
                    // Show with "> [quote]\n\n" prefix before the summary box
                    let cause_content = sql_forge!(
                        MessageContentOnly,
                        r#"
                        SELECT content FROM messages
                        WHERE thread_id = :thread_id AND thread_sequence = 0 AND role = 'cause'
                        LIMIT 1
                        "#,
                        ( :thread_id = msg.thread_id )
                    )
                    .fetch_optional(pool)
                    .await?
                    .map(|r: MessageContentOnly| r.content)
                    .unwrap_or_default();

                    if !cause_content.is_empty() {
                        let truncated: String = cause_content.chars().take(100).collect();
                        if cause_content.len() > 100 {
                            println!("> {}...", truncated);
                        } else {
                            println!("> {}", truncated);
                        }
                    }
                    // blank line after quote (2 newlines total including the one from println)
                    println!();
                    println!("┌─ Summary ──────────────────────────");
                    for chunk in msg.content.split('\n') {
                        println!("│ {}", chunk);
                    }
                    println!("└─────────────────────────────────────");
                    println!();

                    last_seen = msg.id;
                    return Ok((last_seen, last_seen_summary));
                }
                _ => continue,
            }
            last_seen = msg.id;
        }

        // After processing main channel messages, also check for new summaries
        // from channels that this session is subscribed to
        let subs = db::types::get_subscriptions_for_subscriber(pool, "cli", session_id).await?;
        for sub in &subs {
            let summaries = db::types::get_summaries_since(pool, sub.channel_id, last_seen_summary).await?;
            for summary in &summaries {
                // Look up the channel name for display
                let ch_name = db::types::find_channel_by_id(pool, sub.channel_id)
                    .await?
                    .map(|c| c.name)
                    .unwrap_or_else(|| format!("id={}", sub.channel_id));

                println!();
                println!("┌─ [summary from {}] ────────────────", ch_name);
                for chunk in summary.content.split('\n') {
                    println!("│ {}", chunk);
                }
                println!("└─────────────────────────────────────");

                if summary.id > last_seen_summary {
                    last_seen_summary = summary.id;
                }
            }
        }

        sleep(Duration::from_millis(200)).await;
    }
}

/// Handle the `/channel` interactive command.
///
/// Shows a 3-way menu:
///   1. Create a new channel  — prompts for name, profile, model
///   2. Use an existing channel — lists all channels for selection
///   3. Cancel
///
/// Returns `Some((channel_id, channel_name))` on success, or `None` if cancelled.
async fn handle_channel_command<R: std::io::BufRead + Unpin>(
    pool: &PgPool,
    reader: &mut R,
    session_id: &str,
) -> Result<Option<(i64, String)>> {
    use sql_forge::sql_forge;

    loop {
        println!("\n── Channel options ──");
        println!("  1. Create a new channel");
        println!("  2. Use an existing channel");
        println!("  3. Cancel");
        print!("Enter choice (1-3): ");
        io::stdout().flush()?;

        let mut choice = String::new();
        reader.read_line(&mut choice)?;

        match choice.trim() {
            "1" => {
                // ── Create a new channel ──────────────────────────────

                // 1a. Ask for channel name
                let name = loop {
                    print!("Enter channel name (alphanumeric, underscore, hyphen only): ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    reader.read_line(&mut input)?;
                    let name = input.trim().to_string();

                    if db::types::validate_channel_name(&name) {
                        // Check uniqueness (only among 'cli' platform channels)
                        let existing =
                            db::types::get_channel_by_platform_name(pool, "cli", &name).await?;
                        if existing.is_some() {
                            println!(
                                "A channel named '{}' already exists. Choose a different name.",
                                name
                            );
                            continue;
                        }
                        break name;
                    } else {
                        println!(
                            "Invalid name. Use only letters, numbers, underscores, and hyphens."
                        );
                    }
                };

                // 1b. Show available profiles from filesystem
                let data_dir = std::env::var("OMNI_DATA_DIR").unwrap_or_else(|_| "/opt/data".to_string());
                let profile_registry = crate::profile::ProfileRegistry::new(&data_dir);
                let profile_names = profile_registry.list_names();

                println!("\nAvailable profiles:");
                for (i, pn) in profile_names.iter().enumerate() {
                    println!("  {}. {}", i + 1, pn);
                }

                let selected_profile = loop {
                    print!("Choose profile (number): ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    reader.read_line(&mut input)?;
                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx >= 1 && idx <= profile_names.len() {
                            break profile_names[idx - 1].clone();
                        }
                    }
                    println!("Invalid choice.");
                };

                // 1c. Show model options
                let common_models = [
                    "",
                    "deepseek-v4-flash",
                    "gpt-4o",
                    "claude-3-opus-20240229",
                    "claude-3-sonnet-20240229",
                    "gpt-4-turbo",
                    "gpt-3.5-turbo",
                ];
                println!("\nAvailable models:");
                println!("  1. (use profile model)");
                for (i, m) in common_models.iter().skip(1).enumerate() {
                    println!("  {}. {}", i + 2, m);
                }

                let selected_model: Option<String> = loop {
                    print!("Choose model (number): ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    reader.read_line(&mut input)?;
                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx == 1 {
                            break None;
                        }
                        if idx >= 2 && idx <= common_models.len() + 1 {
                            break Some(common_models[idx - 1].to_string());
                        }
                    }
                    println!("Invalid choice.");
                };

                // 1d. Create the channel and claim it
                let new_channel = db::types::create_channel(
                    pool,
                    &name,
                    "cli",
                    &name,
                    "user",
                    &name,
                )
                .await?;

                // Set profile/model on the newly created channel
                sql_forge!(
                    r#"
                    UPDATE channels
                    SET current_profile = :profile,
                        current_model = NULLIF(:model, '')::text,
                        current_provider = ''
                    WHERE id = :id
                    "#,
                    ( :profile = &selected_profile, :model = selected_model.as_deref().unwrap_or(""), :id = new_channel.id )
                )
                .execute(pool)
                .await?;

                // Claim for this session
                crate::platform::claim_channel(pool, new_channel.id, session_id, "cli", None)
                    .await?;

                println!(
                    "Created and claimed channel '{}' (id={}).",
                    name, new_channel.id
                );
                return Ok(Some((new_channel.id, name)));
            }

            "2" => {
                // ── Use an existing channel ───────────────────────────
                let all_channels = db::types::find_all_channels(pool).await?;

                if all_channels.is_empty() {
                    println!("No channels exist yet. Create one first.");
                    continue;
                }

                println!("\nAvailable channels:");
                for (i, ch) in all_channels.iter().enumerate() {
                    let plat = ch.platform.as_deref().unwrap_or("none");
                    println!("  {}. {} ({})", i + 1, ch.name, plat);
                }
                println!("  {}. Cancel", all_channels.len() + 1);

                let selected_idx = loop {
                    print!("Choose channel (number): ");
                    io::stdout().flush()?;
                    let mut input = String::new();
                    reader.read_line(&mut input)?;
                    if let Ok(idx) = input.trim().parse::<usize>() {
                        if idx == all_channels.len() + 1 {
                            return Ok(None);
                        }
                        if idx >= 1 && idx <= all_channels.len() {
                            break idx;
                        }
                    }
                    println!("Invalid choice.");
                };

                let ch = &all_channels[selected_idx - 1];
                let ch_id = ch.id;
                let ch_name = ch.name.clone();

                // Claim the selected channel for this session
                crate::platform::claim_channel(pool, ch_id, session_id, "cli", None).await?;

                println!("Claimed channel '{}' (id={}).", ch_name, ch_id);
                return Ok(Some((ch_id, ch_name)));
            }

            "3" => {
                println!("Canceled.");
                return Ok(None);
            }

            _ => {
                println!("Please enter 1, 2, or 3.");
            }
        }
    }
}

/// Handle the `/action` command — list or run saved actions.
///
/// Usage:
///   /action         — list all actions with numbers
///   /action <num>   — run the numbered action
async fn handle_action_command(pool: &PgPool, input: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = input.trim().splitn(2, ' ').collect();
    let action_arg = parts.get(1).map(|s| s.trim());

    if let Some(num_str) = action_arg {
        if num_str.is_empty() {
            // Just "/action " — list
            let actions = db::types::list_actions(pool).await?;
            if actions.is_empty() {
                println!("No actions saved. Create one via the HTTP API: POST /actions");
                return Ok(());
            }
            println!();
            println!("┌─ Saved Actions ──────────────────────────────────────");
            for (i, a) in actions.iter().enumerate() {
                println!("│ {}. {} (tool: {})", i + 1, a.name, a.tool_name);
            }
            println!("└───────────────────────────────────────────────────────");
            println!();
            return Ok(());
        }

        // Parse the number and run the action
        let num: usize = match num_str.parse() {
            Ok(n) => n,
            Err(_) => {
                println!("Usage: /action [number]");
                println!("  /action        — list all actions");
                println!("  /action <num>  — run the numbered action");
                return Ok(());
            }
        };

        let actions = db::types::list_actions(pool).await?;
        if actions.is_empty() {
            println!("No actions saved. Create one via the HTTP API: POST /actions");
            return Ok(());
        }
        if num == 0 || num > actions.len() {
            println!("Invalid action number. Use /action to list available actions (1-{}).", actions.len());
            return Ok(());
        }

        let action = &actions[num - 1];
        println!();
        println!("┌─ Running Action ────────────────────────────────────────");
        println!("│ Name:     {}", action.name);
        println!("│ Tool:     {}", action.tool_name);
        println!("│ Params:   {}", action.params);
        println!("└─────────────────────────────────────────────────────────");
        println!();

        // We need to call the MCP tool. Since we don't have the MCP registry
        // in the CLI, we just print a message directing to use the HTTP API.
        // The run endpoint is available at POST /actions/:id/run
        println!("To execute this action, use the HTTP API:");
        println!("  POST /actions/{}/run", action.id);
        println!();
    } else {
        // No argument — list all actions
        let actions = db::types::list_actions(pool).await?;
        if actions.is_empty() {
            println!("No actions saved. Create one via the HTTP API: POST /actions");
            return Ok(());
        }
        println!();
        println!("┌─ Saved Actions ──────────────────────────────────────────");
        for (i, a) in actions.iter().enumerate() {
            println!("│ {}. {} (tool: {})", i + 1, a.name, a.tool_name);
        }
        println!("└─────────────────────────────────────────────────────────");
        println!();
    }

    Ok(())
}

/// Handle the `/usage` command — display token usage stats per channel.
async fn handle_usage_command(pool: &PgPool) -> anyhow::Result<()> {
    let stats = db::types::get_channel_usage_stats(pool).await?;

    if stats.is_empty() {
        println!("No usage data found. Send a message first to generate token usage.");
        return Ok(());
    }

    println!();
    println!("┌─ Channel Usage ────────────────────────────────────────");
    println!("│ {:<20} {:<22} {:>10} {:>10} {:>10}", "Channel", "Model", "Input", "Cached", "Output");
    println!("│ {:-<20} {:-<22} {:->10} {:->10} {:->10}", "", "", "", "", "");

    for s in &stats {
        let model = s.model.as_deref().unwrap_or("(not set)");
        println!(
            "│ {:<20} {:<22} {:>10} {:>10} {:>10}",
            s.channel_name,
            model,
            format_num(s.total_input_tokens.unwrap_or(0)),
            format_num(s.total_cached_tokens.unwrap_or(0)),
            format_num(s.total_output_tokens.unwrap_or(0)),
        );
    }

    // Summary row
    let total_in: i64 = stats.iter().map(|s| s.total_input_tokens.unwrap_or(0)).sum();
    let total_cached: i64 = stats.iter().map(|s| s.total_cached_tokens.unwrap_or(0)).sum();
    let total_out: i64 = stats.iter().map(|s| s.total_output_tokens.unwrap_or(0)).sum();
    println!("│ {:-<20} {:-<22} {:->10} {:->10} {:->10}", "", "", "", "", "");
    println!(
        "│ {:<20} {:<22} {:>10} {:>10} {:>10}",
        "TOTAL", "", format_num(total_in), format_num(total_cached), format_num(total_out),
    );
    println!("└─────────────────────────────────────────────────────────");
    println!();

    Ok(())
}

/// Format a number with thousands separators.
fn format_num(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}
