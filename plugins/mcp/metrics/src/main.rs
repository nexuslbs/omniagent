//! mcp-server-metrics — standalone MCP server for agent metrics queries.
//! Provides `get_metrics` tool that reports token usage, latency, message counts,
//! groundedness rate, retrieval hit rate, and hallucination proxy metrics.
//!
//! Communicates via stdio JSON-RPC (MCP protocol).

use anyhow::{Context, Result};
use mcp_server_util::*;
use omniagent::db;
use serde_json::Value;
use sql_forge::sql_forge;
use sqlx::PgPool;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Query result types
// ---------------------------------------------------------------------------

/// Query result for token usage aggregation.
#[derive(Debug, sqlx::FromRow)]
struct TokenAggRow {
    profile: String,
    provider: Option<String>,
    model: Option<String>,
    total_prompt_tokens: Option<i64>,
    total_completion_tokens: Option<i64>,
    total_processing_ms: Option<i64>,
    message_count: Option<i64>,
    avg_processing_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Aggregate metrics from the messages table.
async fn aggregate_metrics(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<Vec<TokenAggRow>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let rows: Vec<TokenAggRow> = sql_forge!(
        TokenAggRow,
        r#"
        SELECT
            t.profile,
            t.provider,
            t.model,
            SUM(t.input_tokens)::bigint AS total_prompt_tokens,
            SUM(t.output_tokens)::bigint AS total_completion_tokens,
            SUM(t.duration_ms)::bigint AS total_processing_ms,
            COUNT(*)::bigint AS message_count,
            AVG(t.duration_ms)::float AS avg_processing_ms
        FROM threads t
        JOIN messages m ON m.thread_id = t.id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        GROUP BY t.profile, t.provider, t.model
        ORDER BY total_processing_ms DESC
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Count how many agent responses have evidence/grounding metadata.
async fn count_grounded_responses(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<(i64, i64)> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let total: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    let grounded: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type IN ('message', 'summary')
          AND m.created_at >= :cutoff
          AND (m.metadata->'context'->>'total_chars') IS NOT NULL
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok((total.unwrap_or(0), grounded.unwrap_or(0)))
}

/// Count retrieval events (how often search tools were called).
async fn count_retrieval_events(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        SELECT COUNT(*)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        WHERE m.role = 'agent'
          AND m.msg_type = 'tool_call'
          AND m.msg_subtype IN ('search_messages', 'search_wiki')
          AND m.created_at >= :cutoff
          AND (:profile_filter = '' OR t.profile = :profile_filter)
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

/// Count user corrections (proxies for hallucination).
async fn count_corrections(
    pool: &PgPool,
    hours: i64,
    profile_filter: &str,
) -> Result<i64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(hours);

    // Look for user messages containing correction keywords after an agent message
    let count: Option<i64> = sql_forge!(
        scalar Option<i64>,
        r#"
        WITH agent_responses AS (
            SELECT m.id, t.channel_id, m.thread_id, m.created_at
            FROM messages m
            JOIN threads t ON t.id = m.thread_id
            WHERE m.role = 'agent'
              AND m.msg_type IN ('message', 'summary')
              AND m.created_at >= :cutoff
              AND (:profile_filter = '' OR t.profile = :profile_filter)
        )
        SELECT COUNT(DISTINCT m.id)::bigint
        FROM messages m
        JOIN threads t ON t.id = m.thread_id
        INNER JOIN agent_responses a
            ON t.channel_id = a.channel_id
            AND m.thread_id = a.thread_id
            AND m.created_at > a.created_at
            AND m.created_at <= a.created_at + INTERVAL '5 minutes'
        WHERE m.role = 'user'
          AND (
              LOWER(m.content) LIKE '%wrong%'
              OR LOWER(m.content) LIKE '%incorrect%'
              OR LOWER(m.content) LIKE '%that''s not%'
              OR LOWER(m.content) LIKE '%actually%'
              OR LOWER(m.content) LIKE '%no,%'
              OR LOWER(m.content) LIKE '%not what%'
              OR LOWER(m.content) LIKE '%try again%'
          )
        "#,
        ( :cutoff = cutoff, :profile_filter = profile_filter )
    )
    .fetch_one(pool)
    .await
    .ok()
    .flatten();

    Ok(count.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Tool: get_metrics
// ---------------------------------------------------------------------------

async fn handle_get_metrics(pool: &PgPool, args: &Value) -> Result<(String, bool)> {
    let hours = args.get("hours").and_then(|v| v.as_i64()).unwrap_or(24);
    let profile = args.get("profile").and_then(|v| v.as_str());
    let profile_owned = profile.map(|s| s.to_string()).unwrap_or_default();

    let usage = aggregate_metrics(pool, hours, &profile_owned).await?;
    let (total_responses, grounded_responses) =
        count_grounded_responses(pool, hours, &profile_owned).await?;
    let retrieval_count =
        count_retrieval_events(pool, hours, &profile_owned).await?;
    let correction_count =
        count_corrections(pool, hours, &profile_owned).await?;

    let mut report = format!(
        "# Agent Metrics Report\n\nPeriod: **last {} hour(s)**\n\n",
        hours
    );

    if let Some(p) = profile {
        report.push_str(&format!("Profile filter: **{}**\n\n", p));
    }

    // Summary
    let grounded_pct = if total_responses > 0 {
        (grounded_responses as f64 / total_responses as f64 * 100.0) as u32
    } else {
        0
    };

    report.push_str("## Summary\n\n");
    report.push_str(&format!(
        "- **Total agent responses**: {}\n",
        total_responses
    ));
    report.push_str(&format!(
        "- **Grounded response rate**: {}% ({} / {})\n",
        grounded_pct, grounded_responses, total_responses
    ));
    report.push_str(&format!(
        "- **Retrieval tool calls**: {}\n",
        retrieval_count
    ));
    report.push_str(&format!(
        "- **User corrections (proxy)**: {}\n\n",
        correction_count
    ));

    if usage.is_empty() {
        report.push_str("No metrics data found for this period.\n\n");
    } else {
        report.push_str("## By Profile / Provider / Model\n\n");
        report.push_str(
            "| Profile | Provider | Model | Messages | Prompt Tokens | Completion Tokens \
             | Total Time (ms) | Avg Time (ms) |\n",
        );
        report.push_str(
            "|---------|----------|-------|----------|---------------|-------------------\
             |-----------------|---------------|\n",
        );

        for row in &usage {
            report.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {:.0} |\n",
                row.profile,
                row.provider.as_deref().unwrap_or("-"),
                row.model.as_deref().unwrap_or("-"),
                row.message_count.unwrap_or(0),
                row.total_prompt_tokens.unwrap_or(0),
                row.total_completion_tokens.unwrap_or(0),
                row.total_processing_ms.unwrap_or(0),
                row.avg_processing_ms.unwrap_or(0.0),
            ));
        }

        // Totals
        let total_prompt: i64 = usage
            .iter()
            .map(|r| r.total_prompt_tokens.unwrap_or(0))
            .sum();
        let total_completion: i64 = usage
            .iter()
            .map(|r| r.total_completion_tokens.unwrap_or(0))
            .sum();
        let total_time: i64 = usage
            .iter()
            .map(|r| r.total_processing_ms.unwrap_or(0))
            .sum();

        report.push_str(&format!(
            "\n**Totals**: {} prompts | {} completion tokens | {} ms processing time\n\n",
            total_prompt, total_completion, total_time
        ));
    }

    // Hallucination metric explanation
    report.push_str("## Metrics Notes\n\n");
    report.push_str(
        "- **Grounded response rate**: Percentage of agent responses that include context \
         assembly metadata (evidence tracking)\n",
    );
    report.push_str(
        "- **Retrieval tool calls**: Number of times search_messages or search_wiki tools \
         were invoked\n",
    );
    report.push_str(
        "- **User corrections (proxy)**: Count of user messages containing correction \
         keywords (wrong, incorrect, etc.) within 5 minutes of an agent response — a proxy \
         for hallucination/quality issues\n",
    );

    Ok((report, false))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = db::connect(&database_url)
        .await
        .context("Failed to connect to database")?;
    let pool = Arc::new(pool);

    // Wrap handler to capture a clone of the pool
    let p_metrics = pool.clone();
    let metrics_handler: ToolHandler =
        Box::new(move |args: &Value| handle_get_metrics(&p_metrics, args));

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "get_metrics".to_string(),
            description:
                "Report agent performance metrics: token usage, latency, message counts, \
                 groundedness rate, retrieval hit rate, and hallucination proxy metrics. \
                 All metrics are aggregated from the messages table and can be filtered \
                 by time window and profile."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "hours": {
                        "type": "integer",
                        "description": "Lookback window in hours (default: 24)"
                    },
                    "profile": {
                        "type": "string",
                        "description": "Filter by profile name (default: all profiles)"
                    }
                }
            }),
        },
        handler: metrics_handler,
    }];

    let server_info = ServerInfo {
        name: "mcp-server-metrics".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}
