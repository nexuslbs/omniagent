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
mod profile;
mod prompt_builder;
mod scheduler;
mod server;
mod vectorizer;

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

    match cli.command.unwrap_or(Command::Server) {
        Command::Server => run_server().await,
        Command::Cli { channel, profile, model, provider } => run_cli(channel, profile, model, provider).await,
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

    // Create AppContext and MCP registry
    let readonly_pool = db::connect(&cfg.database_readonly_url).await?;
    let ctx = mcp::AppContext::new(pool.clone(), readonly_pool, &data_dir, &workspace_dir, Some(cfg.qdrant_url.clone()));
    let mcp = mcp::default_registry(&ctx);

    // Build the agent with MCP context
    let agent = agent::Agent::new(pool.clone(), agent_cfg.clone(), mcp, ctx);

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

    // Create platform registry and register built-in platforms
    let mut registry = platform::PlatformRegistry::new();
    registry.register(Box::new(platform::TelegramPlatform::new()));
    let _platform_handles = registry.start_all(pool.clone());

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
    let vectorizer_handle = tokio::spawn(async move {
        vectorizer::spawn_vectorizers(pool_vectorizer, &cfg, &data_dir).await;
    });

    // Spawn cron scheduler
    let cron_handle = scheduler::spawn(pool.clone());

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
async fn run_cli(channel_name: String, profile_name: String, model: Option<String>, provider: Option<String>) -> Result<()> {
    let database_url = std::env::var("DATABASE_URL")?;
    let pool = db::connect(&database_url).await?;

    // Find or create the CLI channel
    let channel = ensure_cli_channel(&pool, &channel_name, &profile_name, model.as_deref(), provider.as_deref()).await?;
    let channel_id = channel.id;

    println!("\n┌─────────────────────────────────────────────────────────┐");
    println!("│  OmniAgent CLI — channel: {}, profile: {}  │", channel.name, channel.current_profile);
    println!("│  Type your messages. /exit to quit. /new for new thread  │");
    println!("└─────────────────────────────────────────────────────────┘\n");

    // Get or create the current thread
    let mut thread_id = get_or_create_thread(&pool, channel_id).await?;
    let mut next_sequence = get_next_sequence(&pool, channel_id, thread_id).await?;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

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
            "/new" => {
                // Start a new thread
                let root_msg = models::MessageNew {
                    channel_id,
                    role: "user".to_string(),
                    content: "/new".to_string(),
                    status: models::MessageStatus::Completed,
                    thread_id: None,
                    thread_sequence: 0,
                    external_id: None,
                    metadata: serde_json::json!({"cli_new_thread": true}),
                    embedding: None,
                    summary_text: None,
                    is_summary: false,
                    msg_type: "message".to_string(),
                    msg_subtype: None,
                    iteration_count: 0,
                    profile: profile_name.clone(),
                    provider: channel.current_provider.clone(),
                    model: channel.current_model.clone(),
                    processing_time_ms: None,
                    token_usage: None,
                    iterations: 0,
                };
                let saved = db::types::init_thread_root(&pool, &root_msg).await?;
                thread_id = saved.thread_id;
                next_sequence = 1;
                println!("┌─ New conversation thread #{} ────────────────────────┐", thread_id);
                continue;
            }
            _ => {}
        }

        // Insert user message as pending
        let msg = models::MessageNew {
            channel_id,
            role: "user".to_string(),
            content: input.clone(),
            status: models::MessageStatus::Pending,
            thread_id: Some(thread_id),
            thread_sequence: next_sequence,
            external_id: None,
            metadata: serde_json::json!({}),
            embedding: None,
            summary_text: None,
            is_summary: false,
            msg_type: "message".to_string(),
            msg_subtype: None,
            iteration_count: 0,
            profile: profile_name.clone(),
            provider: channel.current_provider.clone(),
            model: channel.current_model.clone(),
            processing_time_ms: None,
            token_usage: None,
            iterations: 0,
        };
        db::types::create_message(&pool, &msg).await?;
        let _user_msg_id = next_sequence;

        // Poll for agent responses
        poll_for_response(&pool, channel_id, thread_id, _user_msg_id).await?;

        // Update next_sequence to reflect all messages added by the agent
        let max_seq = get_max_sequence(&pool, channel_id, thread_id).await?;
        next_sequence = max_seq + 1;
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
            id, name, platform, external_id, cause,
            current_profile, current_model, current_provider,
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
            || model.map_or(true, |m| channel_db.current_model.as_deref() != Some(m))
            || provider.map_or(true, |p| channel_db.current_provider.as_deref() != Some(p))
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
            platform: "cli".to_string(),
            external_id: channel_db.name,
            cause: "user".to_string(),
            current_profile: profile_name.to_string(),
            current_model: model.map(|m| m.to_string()),
            current_provider: provider.map(|p| p.to_string()),
            metadata: serde_json::json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        return Ok(channel);
    }

    // Create new channel
    let new_channel = db::types::create_channel(pool, channel_name, "cli", channel_name, "user").await?;
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
        platform: "cli".to_string(),
        external_id: channel_name.to_string(),
        cause: "user".to_string(),
        current_profile: profile_name.to_string(),
        current_model: model.map(|m| m.to_string()),
        current_provider: provider.map(|p| p.to_string()),
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    })
}

/// Get or create a thread for the CLI session.
async fn get_or_create_thread(pool: &PgPool, channel_id: i64) -> Result<i64> {
    use sql_forge::sql_forge;

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
        SELECT thread_id, id FROM messages
        WHERE channel_id = :channel_id
          AND thread_sequence = 0
          AND status = 'completed'
        ORDER BY id DESC
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

    // Create a new thread by inserting a seq-0 message
    let root_msg = models::MessageNew {
        channel_id,
        role: "user".to_string(),
        content: "/start".to_string(),
        status: models::MessageStatus::Completed,
        thread_id: None,
        thread_sequence: 0,
        external_id: None,
        metadata: serde_json::json!({"cli_start": true}),
        embedding: None,
        summary_text: None,
        is_summary: false,
        msg_type: "message".to_string(),
        msg_subtype: None,
        iteration_count: 0,
        profile: "default".to_string(),
        provider: None,
        model: None,
        processing_time_ms: None,
        token_usage: None,
        iterations: 0,
    };
    let saved = db::types::init_thread_root(pool, &root_msg).await?;
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
        "SELECT MAX(thread_sequence) AS \"max_seq\" FROM messages WHERE channel_id = :channel_id AND thread_id = :thread_id",
        ( :channel_id = channel_id, :thread_id = thread_id )
    )
    .fetch_one(pool)
    .await?;

    Ok(row.max_seq.unwrap_or(0) + 1)
}

/// Get the maximum thread_sequence in a thread.
async fn get_max_sequence(pool: &PgPool, channel_id: i64, thread_id: i64) -> Result<i32> {
    get_next_sequence(pool, channel_id, thread_id).await.map(|n| n - 1)
}

/// Poll for agent responses after inserting a user message.
/// Returns the message ID of the agent's response.
/// Polls every 500ms and prints messages as they arrive.
async fn poll_for_response(
    pool: &PgPool,
    channel_id: i64,
    thread_id: i64,
    after_sequence: i32,
) -> Result<i32> {
    use sql_forge::sql_forge;
    use tokio::time::{sleep, Duration};

    #[derive(Debug, sqlx::FromRow)]
    #[allow(dead_code)]
    struct ResponseMsg {
        id: i64,
        role: String,
        content: String,
        msg_type: String,
        msg_subtype: Option<String>,
        thread_sequence: i32,
    }

    let mut seen_up_to = after_sequence;
    let timeout = Duration::from_secs(300); // 5 min max wait
    let poll_start = std::time::Instant::now();

    loop {
        if poll_start.elapsed() > timeout {
            println!("[Timeout] Agent did not respond within 5 minutes.");
            return Ok(seen_up_to);
        }

        let responses: Vec<ResponseMsg> = sql_forge!(
            ResponseMsg,
            r#"
            SELECT id, role, content, msg_type, msg_subtype, thread_sequence
            FROM messages
            WHERE channel_id = :channel_id
              AND thread_id = :thread_id
              AND thread_sequence > :after_sequence
            ORDER BY thread_sequence ASC
            "#,
            ( :channel_id = channel_id, :thread_id = thread_id, :after_sequence = seen_up_to )
        )
        .fetch_all(pool)
        .await?;

        if responses.is_empty() {
            sleep(Duration::from_millis(500)).await;
            continue;
        }

        for msg in &responses {
            // Skip tool/tool_result messages in CLI output
            match msg.msg_type.as_str() {
                "tool" | "tool_result" => continue,
                _ => {}
            }

            let prefix = match msg.msg_type.as_str() {
                "reasoning" => "┌─ Reasoning ──────────────────────────",
                "message" if msg.role == "agent" => "┌─ Agent ────────────────────────────",
                "summary" => "┌─ Summary ──────────────────────────",
                _ => continue,
            };

            // Print the response
            println!("\n{}", prefix);
            for chunk in msg.content.split('\n') {
                println!("│ {}", chunk);
            }
            println!("└─────────────────────────────────────");

            seen_up_to = msg.thread_sequence;

            // Once we see a summary, the response is complete
            if msg.msg_type == "summary" {
                println!();
                return Ok(seen_up_to);
            }
        }

        sleep(Duration::from_millis(200)).await;
    }
}
