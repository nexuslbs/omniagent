//! Overview endpoint handler
//!
//! Replaces the dashboard's SQL queries at overview.ts:
//! - GET /overview — recent 50 threads
//! - GET /overview/dashboard — KPIs, charts, and analytics
//!
//! All queries use `sql_forge!()`. The multi-CTE dashboard query is split
//! into individual queries (one per section) and assembled in Rust rather
//! than using PostgreSQL's `row_to_json` / `json_agg`.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Serialize;
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn overview_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/overview", get(overview_handler))
        .route("/overview/dashboard", get(dashboard_handler))
}

// ---------------------------------------------------------------------------
// Row types (sqlx::FromRow for sql_forge!)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct OverviewRow {
    id: i64,
    channel_id: i64,
    thread_id: i64,
    content_preview: Option<String>,
    status: Option<String>,
    processing_time_ms: Option<i32>,
    total_tokens: Option<i32>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    channel_name: Option<String>,
    model: Option<String>,
    thread_count: Option<i64>,
}

#[derive(FromRow)]
struct KpiRow {
    threads_today: Option<i64>,
    avg_response_time: Option<i64>,
    tokens_today: Option<i64>,
    active_channels: Option<i64>,
    threads_yesterday: Option<i64>,
    avg_response_yesterday: Option<i64>,
    tokens_yesterday: Option<i64>,
}

#[derive(FromRow)]
struct HourlyRow {
    bucket: Option<chrono::DateTime<chrono::Utc>>,
    count: Option<i64>,
}

#[derive(FromRow)]
struct StatusDistRow {
    status: Option<String>,
    count: Option<i64>,
}

#[derive(FromRow)]
struct TokenTrendRow {
    day: Option<String>,
    tokens: Option<i64>,
}

#[derive(FromRow)]
struct ChannelHealthRow {
    name: Option<String>,
    threads_today: Option<i64>,
    avg_duration: Option<i64>,
    success_rate: Option<f64>,
    last_activity: Option<String>,
}

#[derive(FromRow)]
struct TopToolRow {
    tool: Option<String>,
    count: Option<i64>,
}

// ---------------------------------------------------------------------------
// Response types (serde::Serialize for ok_json)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OverviewEntry {
    id: i64,
    channel_id: i64,
    thread_id: i64,
    content_preview: String,
    status: String,
    processing_time_ms: Option<i64>,
    /// Always 0 — the original dashboard adds this field
    prompt_tokens: i64,
    /// total_tokens (input + output) mapped to completion_tokens
    completion_tokens: i64,
    created_at: String,
    channel_name: String,
    thread_count: i64,
    model: Option<String>,
}

#[derive(Serialize)]
struct KpiResponse {
    threads_today: i64,
    avg_response_time: i64,
    tokens_today: i64,
    active_channels: i64,
    threads_yesterday: i64,
    avg_response_yesterday: i64,
    tokens_yesterday: i64,
}

#[derive(Serialize)]
struct HourlyEntry {
    bucket: String,
    count: i64,
}

#[derive(Serialize)]
struct StatusDistEntry {
    status: String,
    count: i64,
}

#[derive(Serialize)]
struct TokenTrendEntry {
    day: String,
    tokens: i64,
}

#[derive(Serialize)]
struct ChannelHealthEntry {
    name: String,
    threads_today: i64,
    avg_duration: i64,
    success_rate: f64,
    last_activity: String,
}

#[derive(Serialize)]
struct TopToolEntry {
    tool: String,
    count: i64,
}

#[derive(Serialize)]
struct DashboardResponse {
    kpis: KpiResponse,
    threads_over_time: Vec<HourlyEntry>,
    status_distribution: Vec<StatusDistEntry>,
    token_trend: Vec<TokenTrendEntry>,
    recent_activity: Vec<OverviewEntry>,
    channel_health: Vec<ChannelHealthEntry>,
    top_tools: Vec<TopToolEntry>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /overview — recent 50 threads
///
/// Mirrors the original dashboard overview query exactly.
async fn overview_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let rows = match sql_forge!(
        OverviewRow,
        r#"
        SELECT
            t.id,
            t.channel_id,
            t.id AS thread_id,
            LEFT(COALESCE(m.content, ''), 200) AS content_preview,
            COALESCE(t.status, 'unknown') AS status,
            t.duration_ms AS processing_time_ms,
            (t.input_tokens + t.output_tokens) AS total_tokens,
            COALESCE(t.created_at, NOW()) AS created_at,
            COALESCE(c.name, 'unknown') AS channel_name,
            t.model,
            (SELECT COUNT(*) FROM messages sub WHERE sub.thread_id = t.id) AS thread_count
        FROM threads t
        JOIN messages m ON m.thread_id = t.id AND m.thread_sequence = 0
        LEFT JOIN channels c ON c.id = t.channel_id
        ORDER BY t.id DESC
        LIMIT 50
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[overview] query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch overview data",
            );
        }
    };

    let entries: Vec<OverviewEntry> = rows
        .into_iter()
        .map(|r| {
            OverviewEntry {
                id: r.id,
                channel_id: r.channel_id,
                thread_id: r.thread_id,
                content_preview: r.content_preview.unwrap_or_default(),
                status: r.status.unwrap_or_default(),
                processing_time_ms: r.processing_time_ms.map(|v| v as i64),
                prompt_tokens: 0,
                completion_tokens: r.total_tokens.map(|v| v as i64).unwrap_or(0),
                created_at: r.created_at.map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default(),
                channel_name: r.channel_name.unwrap_or_default(),
                thread_count: r.thread_count.unwrap_or(0),
                model: r.model,
            }
        })
        .collect();

    ok_json(entries)
}

/// GET /overview/dashboard — KPIs, time series, and analytics
///
/// The original Express query used a single multi-CTE query with
/// `row_to_json` and `json_agg`. This Rust version runs individual
/// `sql_forge!()` queries for each section and assembles the response.
async fn dashboard_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // ── 1. KPIs ────────────────────────────────────────────────────────────
    let kpis = match sql_forge!(
        KpiRow,
        r#"
        SELECT
            COALESCE((SELECT COUNT(*) FROM threads
                WHERE created_at >= date_trunc('day', NOW())), 0)::bigint AS threads_today,
            COALESCE((SELECT AVG(duration_ms)::bigint FROM threads
                WHERE status = 'completed' AND ended_at >= date_trunc('day', NOW())), 0) AS avg_response_time,
            COALESCE((SELECT SUM(input_tokens + output_tokens) FROM threads
                WHERE created_at >= date_trunc('day', NOW())), 0)::bigint AS tokens_today,
            COALESCE((SELECT COUNT(DISTINCT channel_id) FROM threads
                WHERE created_at >= date_trunc('day', NOW() - INTERVAL '1 day')), 0)::bigint AS active_channels,
            COALESCE((SELECT COUNT(*) FROM threads
                WHERE created_at >= date_trunc('day', NOW() - INTERVAL '1 day')
                  AND created_at < date_trunc('day', NOW())), 0)::bigint AS threads_yesterday,
            COALESCE((SELECT AVG(duration_ms)::bigint FROM threads
                WHERE status = 'completed'
                  AND ended_at >= date_trunc('day', NOW() - INTERVAL '1 day')
                  AND ended_at < date_trunc('day', NOW())), 0) AS avg_response_yesterday,
            COALESCE((SELECT SUM(input_tokens + output_tokens) FROM threads
                WHERE created_at >= date_trunc('day', NOW() - INTERVAL '1 day')
                  AND created_at < date_trunc('day', NOW())), 0)::bigint AS tokens_yesterday
        "#,
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            error!("[dashboard] kpis query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch dashboard KPIs",
            );
        }
    };

    // ── 2. Hourly thread counts (7 days) ──────────────────────────────────
    let hourly_rows = match sql_forge!(
        HourlyRow,
        r#"
        SELECT
            date_trunc('hour', g) AS bucket,
            COALESCE(COUNT(t.id), 0)::bigint AS count
        FROM generate_series(
            date_trunc('hour', NOW() - INTERVAL '7 days'),
            date_trunc('hour', NOW()),
            INTERVAL '1 hour'
        ) g
        LEFT JOIN threads t ON date_trunc('hour', t.created_at) = g
        GROUP BY bucket
        ORDER BY bucket
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| HourlyEntry {
                bucket: r.bucket.map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default(),
                count: r.count.unwrap_or(0),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] hourly query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch hourly thread counts",
            );
        }
    };

    // ── 3. Status distribution ────────────────────────────────────────────
    let status_dist = match sql_forge!(
        StatusDistRow,
        r#"
        SELECT COALESCE(t.status, 'unknown') AS status, COUNT(*)::bigint AS count
        FROM threads t
        GROUP BY t.status
        ORDER BY count DESC
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| StatusDistEntry {
                status: r.status.unwrap_or_default(),
                count: r.count.unwrap_or(0),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] status_dist query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch status distribution",
            );
        }
    };

    // ── 4. Token trend (14 days) ──────────────────────────────────────────
    let token_trend = match sql_forge!(
        TokenTrendRow,
        r#"
        SELECT
            g::date::text AS day,
            COALESCE(SUM(t.input_tokens + t.output_tokens), 0)::bigint AS tokens
        FROM generate_series(
            (NOW() - INTERVAL '13 days')::date,
            NOW()::date,
            INTERVAL '1 day'
        ) g
        LEFT JOIN threads t ON t.created_at::date = g::date
        GROUP BY g::date
        ORDER BY g::date
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| TokenTrendEntry {
                day: r.day.unwrap_or_default(),
                tokens: r.tokens.unwrap_or(0),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] token_trend query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch token trend",
            );
        }
    };

    // ── 5. Recent activity (10 threads) ───────────────────────────────────
    let recent = match sql_forge!(
        OverviewRow,
        r#"
        SELECT
            t.id,
            t.channel_id,
            t.id AS thread_id,
            LEFT(COALESCE(m.content, ''), 200) AS content_preview,
            COALESCE(t.status, 'unknown') AS status,
            t.duration_ms AS processing_time_ms,
            (t.input_tokens + t.output_tokens) AS total_tokens,
            COALESCE(t.created_at, NOW()) AS created_at,
            COALESCE(c.name, 'unknown') AS channel_name,
            t.model,
            (SELECT COUNT(*) FROM messages sub WHERE sub.thread_id = t.id) AS thread_count
        FROM threads t
        JOIN messages m ON m.thread_id = t.id AND m.thread_sequence = 0
        LEFT JOIN channels c ON c.id = t.channel_id
        ORDER BY t.id DESC
        LIMIT 10
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                let completion = r.total_tokens.map(|v| v as i64).unwrap_or(0);
                OverviewEntry {
                    id: r.id,
                    channel_id: r.channel_id,
                    thread_id: r.thread_id,
                    content_preview: r.content_preview.unwrap_or_default(),
                    status: r.status.unwrap_or_default(),
                    processing_time_ms: r.processing_time_ms.map(|v| v as i64),
                    prompt_tokens: 0,
                    completion_tokens: completion,
                    created_at: r.created_at.map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default(),
                    channel_name: r.channel_name.unwrap_or_default(),
                    thread_count: r.thread_count.unwrap_or(0),
                    model: r.model,
                }
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] recent query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch recent activity",
            );
        }
    };

    // ── 6. Channel health ─────────────────────────────────────────────────
    let channel_health = match sql_forge!(
        ChannelHealthRow,
        r#"
        SELECT
            COALESCE(c.name, 'unknown') AS name,
            COUNT(*) FILTER (
                WHERE t.created_at >= date_trunc('day', NOW()) AND t.status != 'system'
            )::bigint AS threads_today,
            COALESCE(AVG(t.duration_ms) FILTER (WHERE t.status = 'completed')::bigint, 0) AS avg_duration,
            CASE
                WHEN COUNT(*) FILTER (WHERE t.status != 'system') > 0
                THEN ROUND(
                    COUNT(*) FILTER (WHERE t.status = 'completed')::numeric
                    / GREATEST(COUNT(*) FILTER (WHERE t.status != 'system'), 1), 2
                )::float8
                ELSE 0
            END AS success_rate,
            COALESCE(MAX(t.created_at)::text, '') AS last_activity
        FROM threads t
        LEFT JOIN channels c ON c.id = t.channel_id
        GROUP BY c.name
        ORDER BY threads_today DESC
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| ChannelHealthEntry {
                name: r.name.unwrap_or_default(),
                threads_today: r.threads_today.unwrap_or(0),
                avg_duration: r.avg_duration.unwrap_or(0),
                success_rate: r.success_rate.unwrap_or(0.0),
                last_activity: r.last_activity.unwrap_or_default(),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] channel_health query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch channel health",
            );
        }
    };

    // ── 7. Top tools (last 7 days) ────────────────────────────────────────
    let top_tools = match sql_forge!(
        TopToolRow,
        r#"
        SELECT
            COALESCE(m.msg_subtype, 'unknown') AS tool,
            COUNT(*)::bigint AS count
        FROM messages m
        WHERE m.msg_type = 'tool'
            AND m.created_at >= NOW() - INTERVAL '7 days'
        GROUP BY m.msg_subtype
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| TopToolEntry {
                tool: r.tool.unwrap_or_default(),
                count: r.count.unwrap_or(0),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("[dashboard] top_tools query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch top tools",
            );
        }
    };

    let dashboard = DashboardResponse {
        kpis: KpiResponse {
            threads_today: kpis.threads_today.unwrap_or(0),
            avg_response_time: kpis.avg_response_time.unwrap_or(0),
            tokens_today: kpis.tokens_today.unwrap_or(0),
            active_channels: kpis.active_channels.unwrap_or(0),
            threads_yesterday: kpis.threads_yesterday.unwrap_or(0),
            avg_response_yesterday: kpis.avg_response_yesterday.unwrap_or(0),
            tokens_yesterday: kpis.tokens_yesterday.unwrap_or(0),
        },
        threads_over_time: hourly_rows,
        status_distribution: status_dist,
        token_trend,
        recent_activity: recent,
        channel_health,
        top_tools,
    };

    ok_json(dashboard)
}
