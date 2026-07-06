//! Schedule CRUD API — replaces dashboard's schedule.ts SQL.
//!
//! All 10 SQL queries from the original TypeScript are replaced with
//! `sql_forge!()` macros. No raw sqlx query calls.
//!
//! - `GET    /schedule`              — list cron jobs (optionally filter by active)
//! - `GET    /schedule/{id}`         — single cron job detail
//! - `POST   /schedule`              — create/upsert cron job
//! - `PATCH  /schedule/{id}`         — update cron job fields (NULLIF pattern)
//! - `PATCH  /schedule/{id}/toggle`  — toggle active state
//! - `GET    /schedule/{id}/threads` — threads for a schedule task
//! - `GET    /schedule/{id}/subtasks` — subtasks for all threads of a job
//! - `POST   /schedule/{id}/run`     — manually trigger a cron job

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::FromRow;
use std::sync::Arc;
use tracing::error;

use super::{err_json, ok_json, AppState};

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn schedule_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/schedule", get(list_schedule_handler))
        .route("/schedule/{id}", get(get_schedule_handler))
        .route("/schedule", post(create_schedule_handler))
        .route("/schedule/{id}", patch(update_schedule_handler))
        .route("/schedule/{id}/toggle", patch(toggle_schedule_handler))
        .route("/schedule/{id}/threads", get(schedule_threads_handler))
        .route("/schedule/{id}/subtasks", get(schedule_subtasks_handler))
        .route("/schedule/{id}/run", post(run_schedule_handler))
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct JobEntry {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub schedule: String,
    pub prompt_preview: String,
    pub prompt: Option<String>,
    pub skills: Vec<String>,
    pub enabled: bool,
    pub active: bool,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub action_name: Option<String>,
    pub channel_id: Option<i64>,
    pub channel_name: Option<String>,
    pub profile: Option<String>,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub status: String,
    pub silent: bool,
    pub template: Option<String>,
    pub planning_mode: String,
}

#[derive(Debug, Serialize)]
pub struct ScheduleThread {
    pub id: i64,
    pub thread_id: i64,
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub subtype: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub processing_time_ms: Option<i64>,
    pub token_usage: Option<String>,
    pub iteration_number: Option<i32>,
    pub thread_sequence: Option<i32>,
    pub created_at: Option<String>,
    pub metadata: Option<String>,
    pub thread_status: Option<String>,
    pub channel_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ScheduleThreadsResponse {
    pub rows: Vec<ScheduleThread>,
    pub total: i64,
}

#[derive(Debug, Serialize)]
pub struct SubtaskEntry {
    pub id: i64,
    pub description: String,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub thread_id: Option<i64>,
    pub thread_title: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubtasksResponse {
    pub subtasks: Vec<SubtaskEntry>,
}

// ---------------------------------------------------------------------------
// Row types (sqlx::FromRow for sql_forge!)
// ---------------------------------------------------------------------------

#[derive(FromRow)]
struct CronJobRow {
    id: String,
    name: String,
    display_name: Option<String>,
    schedule: String,
    prompt: Option<String>,
    skills: Option<String>,
    enabled: Option<bool>,
    active: Option<bool>,
    mode: Option<String>,
    action_id: Option<String>,
    action_name: Option<String>,
    channel_id: Option<i64>,
    channel_name: Option<String>,
    profile: Option<String>,
    last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    silent: Option<bool>,
    template: Option<String>,
    planning_mode: Option<String>,
}

#[derive(FromRow)]
struct IdRow {
    id: String,
}

#[derive(FromRow)]
struct ThreadCountRow {
    total: Option<i64>,
}

#[derive(FromRow)]
struct ScheduleThreadRow {
    id: i64,
    thread_id: i64,
    role: Option<String>,
    content: Option<String>,
    msg_type: Option<String>,
    subtype: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    processing_time_ms: Option<i32>,
    token_usage: Option<String>,
    iteration_number: Option<i32>,
    thread_sequence: Option<i32>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    metadata: Option<String>,
    thread_status: Option<String>,
    channel_name: Option<String>,
}

#[derive(FromRow)]
struct SubtaskRow {
    id: i64,
    description: String,
    status: Option<String>,
    priority: Option<i32>,
    thread_id: Option<i64>,
    thread_title: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListScheduleQuery {
    pub active: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateScheduleRequest {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub active: Option<bool>,
    pub channel_id: Option<i64>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub enabled: Option<bool>,
    pub silent: Option<bool>,
    pub template: Option<String>,
    pub planning_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateScheduleRequest {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub active: Option<bool>,
    pub enabled: Option<bool>,
    pub channel_id: Option<i64>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub silent: Option<bool>,
    pub template: Option<String>,
    pub planning_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToggleRequest {
    pub active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct ThreadsQueryParams {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RunScheduleRequest {
    pub force: Option<bool>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JSONB skills field into a Vec<String>, handling both array and
/// string-encoded JSON representations stored as text.
fn parse_skills(val: Option<String>) -> Vec<String> {
    match val {
        None => vec![],
        Some(s) => {
            // Try parsing as JSON array first
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(&s) {
                arr
            } else if s.trim().starts_with('[') {
                vec![]
            } else if s.is_empty() {
                vec![]
            } else {
                // Fallback: treat as comma-separated
                s.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect()
            }
        }
    }
}

fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Look up the default cron channel ID. Returns 0 as fallback (caller should handle).
async fn lookup_cron_channel(pool: &sqlx::PgPool) -> i64 {
    match sql_forge!(
        i64,
        r#"SELECT id FROM channels WHERE platform = 'cron' AND name = 'cron' LIMIT 1"#,
    )
    .fetch_optional(pool)
    .await
    {
        Ok(Some(id)) => id,
        _ => 0,
    }
}

fn fmt_ts_opt(ts: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    ts.map(|t| fmt_ts(&t))
}

fn generate_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect()
}

/// Validate a 5-field cron expression. Returns an error message if invalid.
fn validate_cron(schedule: &str) -> Option<String> {
    let fields: Vec<&str> = schedule.trim().split_whitespace().collect();
    if fields.len() != 5 {
        Some(format!(
            "Invalid cron expression: expected 5 fields (min hour dom month dow), got {}. \
             Use 5-field Linux format, e.g. '0 9 * * 1-5' for weekdays at 9am.",
            fields.len()
        ))
    } else {
        None
    }
}

fn job_row_to_entry(row: CronJobRow) -> JobEntry {
    let prompt_preview = row
        .prompt
        .as_deref()
        .map(|p| {
            if p.len() > 100 {
                format!("{}...", &p[..100])
            } else {
                p.to_string()
            }
        })
        .unwrap_or_default();

    let enabled = row.enabled.unwrap_or(false);
    let silent = row.silent.unwrap_or(false);
    let active = row.active.unwrap_or(false);

    JobEntry {
        id: row.id,
        name: row.name,
        display_name: row.display_name,
        schedule: row.schedule,
        prompt_preview,
        prompt: row.prompt,
        skills: parse_skills(row.skills),
        enabled,
        active,
        mode: row.mode,
        action_id: row.action_id,
        action_name: row.action_name,
        channel_id: row.channel_id,
        channel_name: row.channel_name,
        profile: row.profile,
        last_run: fmt_ts_opt(row.last_run_at),
        next_run: fmt_ts_opt(row.next_run_at),
        last_run_at: fmt_ts_opt(row.last_run_at),
        next_run_at: fmt_ts_opt(row.next_run_at),
        created_at: row.created_at.as_ref().map(|dt| fmt_ts(dt)).unwrap_or_default(),
        status: if enabled {
            "active".to_string()
        } else {
            "paused".to_string()
        },
        silent,
        template: row.template,
        planning_mode: row.planning_mode.unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /schedule — list cron jobs, optionally filtering by active status.
///
/// SQL queries used: 1 (single SELECT with DISTINCT ON and optional WHERE)
async fn list_schedule_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListScheduleQuery>,
) -> impl IntoResponse {
    let active_only = params.active.as_deref() != Some("false");

    let rows = match sql_forge!(
        CronJobRow,
        r#"
        SELECT DISTINCT ON (cj.name)
            cj.id,
            cj.name,
            cj.display_name,
            cj.schedule,
            cj.prompt,
            cj.skills,
            cj.enabled,
            cj.active,
            cj.mode,
            cj.action_id,
            NULL::TEXT AS action_name,
            cj.channel_id,
            ch.name AS channel_name,
            cj.profile,
            cj.last_run_at,
            cj.next_run_at,
            cj.created_at,
            cj.silent,
            cj.template,
            cj.planning_mode
        FROM cron_jobs cj
        LEFT JOIN channels ch ON ch.id = cj.channel_id
        WHERE (:active_only = false OR cj.active = true)
        ORDER BY cj.name, cj.created_at DESC
        "#,
        ( :active_only = active_only )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[schedule] list query failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch cron jobs",
            );
        }
    };

    let jobs: Vec<JobEntry> = rows.into_iter().map(job_row_to_entry).collect();
    ok_json(jobs)
}

/// GET /schedule/{id} — single cron job detail.
///
/// SQL queries used: 1 (SELECT with LEFT JOIN and WHERE id = :id)
async fn get_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let row = match sql_forge!(
        CronJobRow,
        r#"
        SELECT
            cj.id,
            cj.name,
            cj.display_name,
            cj.schedule,
            cj.prompt,
            cj.skills,
            cj.enabled,
            cj.active,
            cj.mode,
            cj.action_id,
            NULL::TEXT AS action_name,
            cj.channel_id,
            ch.name AS channel_name,
            cj.profile,
            cj.last_run_at,
            cj.next_run_at,
            cj.created_at,
            cj.silent,
            cj.template,
            cj.planning_mode
        FROM cron_jobs cj
        LEFT JOIN channels ch ON ch.id = cj.channel_id
        WHERE cj.id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Job not found");
        }
        Err(e) => {
            error!("[schedule/{}] get query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch cron job",
            );
        }
    };

    ok_json(job_row_to_entry(row))
}

/// POST /schedule — create or upsert a cron job.
///
/// SQL queries used: 1 (INSERT with ON CONFLICT DO UPDATE)
async fn create_schedule_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateScheduleRequest>,
) -> impl IntoResponse {
    let name = body.name.as_deref().unwrap_or("");
    let schedule_val = body.schedule.as_deref().unwrap_or("");

    if name.is_empty() || schedule_val.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Name and schedule are required");
    }

    // Validate 5-field cron format
    if let Some(err) = validate_cron(schedule_val) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }

    let job_id = generate_id(name);
    let display_name = body.display_name.as_deref().unwrap_or(name);

    // When no channel_id is provided, use the permanent default cron channel
    let effective_channel_id: i64 = if let Some(cid) = body.channel_id {
        if cid != 0 { cid } else { lookup_cron_channel(&state.pool).await }
    } else {
        lookup_cron_channel(&state.pool).await
    };

    // Execute INSERT with RETURNING, using ON CONFLICT DO UPDATE for upsert
    match sql_forge!(
        r#"
        INSERT INTO cron_jobs (
            id, name, display_name, schedule, prompt, active,
            channel_id, profile, mode, action_id, enabled,
            silent, template, planning_mode
        )
        VALUES (
            :id, :name, :display_name, :schedule, :prompt, :active,
            :channel_id, NULLIF(:profile, '')::text, :mode, :action_id, :enabled,
            :silent, :template, NULLIF(:planning_mode, '')::text
        )
        ON CONFLICT (id) DO UPDATE SET
            name = EXCLUDED.name,
            display_name = EXCLUDED.display_name,
            schedule = EXCLUDED.schedule,
            prompt = EXCLUDED.prompt,
            active = EXCLUDED.active,
            channel_id = EXCLUDED.channel_id,
            profile = EXCLUDED.profile,
            mode = EXCLUDED.mode,
            action_id = EXCLUDED.action_id,
            enabled = EXCLUDED.enabled,
            silent = EXCLUDED.silent,
            template = EXCLUDED.template,
            planning_mode = EXCLUDED.planning_mode,
            updated_at = NOW()
        "#,
        ( :id = &job_id,
          :name = name,
          :display_name = display_name,
          :schedule = schedule_val,
          :prompt = body.prompt.as_deref().unwrap_or(""),
          :active = body.active.unwrap_or(true),
          :channel_id = effective_channel_id,
          :profile = body.profile.as_deref().unwrap_or(""),
          :mode = body.mode.as_deref().unwrap_or("agentic"),
          :action_id = body.action_id.as_deref().unwrap_or(""),
          :enabled = body.enabled.unwrap_or(true),
          :silent = body.silent.unwrap_or(false),
          :template = body.template.as_deref().unwrap_or(""),
          :planning_mode = body.planning_mode.as_deref().unwrap_or("") )
    )
    .execute(&state.pool)
    .await
    {
        Ok(_) => {
            ok_json(serde_json::json!({ "success": true, "id": job_id }))
        }
        Err(e) => {
            error!("[schedule] create failed: {:?}", e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create cron job",
            )
        }
    }
}

/// PATCH /schedule/{id} — update cron job fields.
///
/// Uses the NULLIF/CASE pattern for text fields to preserve existing values
/// when a field is not provided (empty string sentinel).
///
/// SQL queries used: 2 (check exists + UPDATE with NULLIF)
async fn update_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateScheduleRequest>,
) -> impl IntoResponse {
    // ── 1. Check job exists and fetch current values ──
    let current_job = match sql_forge!(
        CronJobRow,
        r#"
        SELECT
            cj.id, cj.name, cj.display_name, cj.schedule, cj.prompt, cj.skills,
            cj.enabled, cj.active, cj.mode, cj.action_id, NULL::TEXT AS action_name, cj.channel_id,
            NULL::TEXT AS channel_name, cj.profile,
            cj.last_run_at, cj.next_run_at, cj.created_at, cj.silent,
            cj.template, cj.planning_mode
        FROM cron_jobs cj
        WHERE cj.id = :id
        "#,
        ( :id = &id )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            return err_json(StatusCode::NOT_FOUND, "Job not found");
        }
        Err(e) => {
            error!("[schedule/{}] check query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to check cron job",
            );
        }
    };

    // ── 2. Validate cron format if schedule is being updated ──
    if let Some(ref sched) = body.schedule {
        if let Some(err) = validate_cron(sched) {
            return err_json(StatusCode::BAD_REQUEST, &err);
        }
    }

    // ── 3. Apply updates using NULLIF pattern ──
    // Text fields use CASE WHEN :field = '' THEN field ELSE NULLIF(:field, '')::text END
    // which preserves existing value when not provided (empty string sentinel).
    // Boolean and numeric fields use the current value as fallback.
    match sql_forge!(
        r#"
        UPDATE cron_jobs
        SET
            name = CASE
                WHEN :name = '' THEN name
                ELSE NULLIF(:name, '')::text
            END,
            display_name = CASE
                WHEN :display_name = '' THEN display_name
                ELSE NULLIF(:display_name, '')::text
            END,
            schedule = CASE
                WHEN :schedule = '' THEN schedule
                ELSE NULLIF(:schedule, '')::text
            END,
            prompt = CASE
                WHEN :prompt = '' THEN prompt
                ELSE NULLIF(:prompt, '')::text
            END,
            channel_id = :channel_id,
            profile = CASE
                WHEN :profile = '' THEN profile
                ELSE NULLIF(:profile, '')::text
            END,
            mode = CASE
                WHEN :mode = '' THEN mode
                ELSE NULLIF(:mode, '')::text
            END,
            action_id = CASE
                WHEN :action_id = '' THEN action_id
                ELSE NULLIF(:action_id, '')::text
            END,
            enabled = :enabled,
            active = :active,
            silent = :silent,
            template = CASE
                WHEN :template = '' THEN template
                ELSE NULLIF(:template, '')::text
            END,
            planning_mode = CASE
                WHEN :planning_mode = '' THEN planning_mode
                ELSE NULLIF(:planning_mode, '')::text
            END,
            updated_at = NOW()
        WHERE id = :id
        "#,
        ( :id = &id,
          :name = body.name.as_deref().unwrap_or(""),
          :display_name = body.display_name.as_deref().unwrap_or(""),
          :schedule = body.schedule.as_deref().unwrap_or(""),
          :prompt = body.prompt.as_deref().unwrap_or(""),
          :channel_id = body.channel_id.or(current_job.channel_id).unwrap_or(0),
          :profile = body.profile.as_deref().unwrap_or(""),
          :mode = body.mode.as_deref().unwrap_or(""),
          :action_id = body.action_id.as_deref().unwrap_or(""),
          :enabled = body.enabled.unwrap_or(current_job.enabled.unwrap_or(true)),
          :active = body.active.unwrap_or(current_job.active.unwrap_or(true)),
          :silent = body.silent.unwrap_or(current_job.silent.unwrap_or(false)),
          :template = body.template.as_deref().unwrap_or(""),
          :planning_mode = body.planning_mode.as_deref().unwrap_or("") )
    )
    .execute(&state.pool)
    .await
    {
        Ok(_) => ok_json(serde_json::json!({ "success": true })),
        Err(e) => {
            error!("[schedule/{}] update failed: {:?}", id, e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to update cron job",
            )
        }
    }
}

/// PATCH /schedule/{id}/toggle — toggle the active state of a cron job.
///
/// SQL queries used: 1 (UPDATE active)
async fn toggle_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ToggleRequest>,
) -> impl IntoResponse {
    let active = match body.active {
        Some(val) => val,
        None => {
            return err_json(StatusCode::BAD_REQUEST, "Missing 'active' field");
        }
    };

    match sql_forge!(
        r#"UPDATE cron_jobs SET active = :active, updated_at = NOW() WHERE id = :id"#,
        ( :active = active, :id = &id )
    )
    .execute(&state.pool)
    .await
    {
        Ok(result) if result.rows_affected() > 0 => {
            ok_json(serde_json::json!({ "success": true, "active": active }))
        }
        Ok(_) => err_json(StatusCode::NOT_FOUND, "Job not found"),
        Err(e) => {
            error!("[schedule/{}/toggle] failed: {:?}", id, e);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to toggle cron job",
            )
        }
    }
}

/// GET /schedule/{id}/threads — threads for a schedule task with pagination.
///
/// SQL queries used: 2 (COUNT + paginated SELECT with LATERAL join)
async fn schedule_threads_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<ThreadsQueryParams>,
) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(10).min(100).max(1);
    let order_asc = params.order.as_deref() == Some("asc");

    // ── Total count ──
    let total = match sql_forge!(
        ThreadCountRow,
        r#"SELECT COUNT(*) AS total FROM threads WHERE schedule_task_id = :id"#,
        ( :id = &id )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => row.total.unwrap_or(0),
        Err(e) => {
            error!("[schedule/{}/threads] count query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to count threads",
            );
        }
    };

    // ── Paginated rows with LATERAL join for the last message ──
    // Order direction is controlled via CASE expressions to stay within
    // sql_forge!() and avoid SQL injection from string interpolation.
    let rows = match sql_forge!(
        ScheduleThreadRow,
        r#"
        SELECT
            last_msg.id,
            last_msg.thread_id,
            last_msg.role,
            last_msg.content,
            last_msg.msg_type,
            last_msg.msg_subtype AS subtype,
            t.provider,
            t.model,
            last_msg.processing_time_ms,
            last_msg.token_usage,
            last_msg.iteration_number,
            last_msg.thread_sequence,
            last_msg.created_at,
            last_msg.metadata::text AS metadata,
            t.status AS thread_status,
            c.name AS channel_name
        FROM threads t
        LEFT JOIN channels c ON c.id = t.channel_id
        LEFT JOIN LATERAL (
            SELECT m.id, m.thread_id, m.role, m.content, m.msg_type,
                   m.msg_subtype, m.processing_time_ms, m.token_usage::text AS token_usage,
                   m.iteration_number, m.thread_sequence,
                   m.created_at, m.metadata
            FROM messages m
            WHERE m.thread_id = t.id
            ORDER BY m.id DESC
            LIMIT 1
        ) last_msg ON true
        WHERE t.schedule_task_id = :id
        ORDER BY
            CASE WHEN :order_asc THEN last_msg.created_at END ASC,
            CASE WHEN :order_asc = false THEN last_msg.created_at END DESC
            NULLS LAST
        OFFSET :offset LIMIT :limit
        "#,
        ( :id = &id,
          :order_asc = order_asc,
          :offset = offset,
          :limit = limit )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[schedule/{}/threads] data query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch threads",
            );
        }
    };

    let thread_rows: Vec<ScheduleThread> = rows
        .into_iter()
        .map(|r| ScheduleThread {
            id: r.id,
            thread_id: r.thread_id,
            role: r.role,
            content: r.content,
            msg_type: r.msg_type,
            subtype: r.subtype,
            provider: r.provider,
            model: r.model,
            processing_time_ms: r.processing_time_ms.map(|v| v as i64),
            token_usage: r.token_usage.and_then(|s| serde_json::from_str(&s).ok()),
            iteration_number: r.iteration_number,
            thread_sequence: r.thread_sequence,
            created_at: r.created_at.map(|dt| fmt_ts(&dt)),
            metadata: r.metadata.and_then(|s| serde_json::from_str(&s).ok()),
            thread_status: r.thread_status,
            channel_name: r.channel_name,
        })
        .collect();

    ok_json(ScheduleThreadsResponse {
        rows: thread_rows,
        total,
    })
}

/// GET /schedule/{id}/subtasks — subtasks for all threads of a schedule job.
///
/// SQL queries used: 1 (SELECT with JOIN)
async fn schedule_subtasks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let subtasks = match sql_forge!(
        SubtaskRow,
        r#"
        SELECT
            ts.id,
            ts.description,
            ts.status,
            ts.priority,
            ts.thread_id,
            COALESCE(NULLIF(t.cause, ''), t.id::text) AS thread_title,
            ts.created_at,
            ts.updated_at
        FROM thread_subtasks ts
        JOIN threads t ON t.id = ts.thread_id
        WHERE t.schedule_task_id = :id
        ORDER BY t.id, ts.priority DESC, ts.id ASC
        "#,
        ( :id = &id )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("[schedule/{}/subtasks] query failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to fetch subtasks",
            );
        }
    };

    let entries: Vec<SubtaskEntry> = subtasks
        .into_iter()
        .map(|r| SubtaskEntry {
            id: r.id,
            description: r.description,
            status: r.status,
            priority: r.priority,
            thread_id: r.thread_id,
            thread_title: r.thread_title,
            created_at: r.created_at.map(|dt| fmt_ts(&dt)),
            updated_at: r.updated_at.map(|dt| fmt_ts(&dt)),
        })
        .collect();

    ok_json(SubtasksResponse { subtasks: entries })
}

/// POST /schedule/{id}/run — manually trigger a cron job.
///
/// Delegates to `crate::scheduler::fire_cron_job_by_id` (the same function
/// used by the existing `/run-cron/{schedule_id}` endpoint).
///
/// No SQL queries here — the actual run logic is in the scheduler module.
async fn run_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RunScheduleRequest>,
) -> impl IntoResponse {
    let force = body.force.unwrap_or(false);

    match crate::scheduler::fire_cron_job_by_id(
        &state.pool,
        &state.data_dir,
        &state.mcp_registry,
        &state.app_context,
        &id,
        force,
    )
    .await
    {
        Ok(thread_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "schedule_id": id,
                "thread_id": thread_id,
            })),
        ),
        Err(e) => {
            let msg = e.to_string();
            error!("[schedule/{}/run] Failed: {}", id, msg);

            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else if msg.contains("not active") {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };

            (
                status,
                Json(serde_json::json!({
                    "status": "error",
                    "error": msg,
                    "schedule_id": id,
                })),
            )
        }
    }
}
