//! Schedule CRUD API: backed by `{data_dir}/config/tasks.yml` (`schedules:`).
//!
//! Definitions (previously `cron_jobs` rows) now live in the git-tracked yml
//! file; every handler reads/writes it (parsed fresh per request, so edits to
//! the file take effect immediately). The API response shape is unchanged so
//! the dashboard keeps working: `id` = yml key, `name` = key, and the legacy
//! field names (schedule/prompt/enabled/active/mode/action_id/profile/
//! channel_id/template/plan/...) are preserved. `last_run`/`next_run`/
//! `created_at` no longer exist and are returned as null/empty; runs are
//! observable via the threads view (`GET /schedule/{id}/threads`).
//!
//! - `GET    /schedule`             : list schedules (optionally filter by active)
//! - `GET    /schedule/{id}`        : single schedule detail
//! - `POST   /schedule`             : create/upsert schedule
//! - `PATCH  /schedule/{id}`        : update schedule fields
//! - `PATCH  /schedule/{id}/toggle` : toggle enabled state
//! - `GET    /schedule/{id}/threads`: threads for a schedule task
//! - `GET    /schedule/{id}/subtasks`: subtasks for all threads of a job
//! - `POST   /schedule/{id}/run`    : manually trigger a schedule

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
use crate::tasks_yaml::{self, ScheduleDef};

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
    pub display_name: String,
    pub schedule: String,
    pub prompt_preview: String,
    pub prompt: Option<String>,
    pub skills: Vec<String>,
    pub enabled: bool,
    pub active: bool,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub action_name: Option<String>,
    pub channel_id: Option<String>,
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
    pub plan: bool,
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
// Row types (threads/subtasks queries stay DB-backed — runs live in threads)
// ---------------------------------------------------------------------------

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
    pub channel_id: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub enabled: Option<bool>,
    pub silent: Option<bool>,
    pub template: Option<String>,
    pub plan: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateScheduleRequest {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub schedule: Option<String>,
    pub prompt: Option<String>,
    pub active: Option<bool>,
    pub enabled: Option<bool>,
    pub channel_id: Option<String>,
    pub profile: Option<String>,
    pub mode: Option<String>,
    pub action_id: Option<String>,
    pub silent: Option<bool>,
    pub template: Option<String>,
    pub plan: Option<bool>,
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
            } else if s.trim().starts_with('[') || s.is_empty() {
                vec![]
            } else {
                // Fallback: treat as comma-separated
                s.split(',')
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .collect()
            }
        }
    }
}

fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn fmt_ts_opt(ts: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    ts.map(|t| fmt_ts(&t))
}

fn generate_id(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Validate a 5-field cron expression. Returns an error message if invalid.
fn validate_cron(schedule: &str) -> Option<String> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
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

/// Build a JobEntry from a tasks.yml schedule key+def (channel NAME resolved
/// to id; unknown → None). last_run/next_run/created_at no longer exist and
/// are returned as null/empty (runs are visible via /schedule/{id}/threads).
async fn schedule_to_entry(
    pool: &sqlx::PgPool,
    data_dir: &str,
    key: &str,
    def: &ScheduleDef,
) -> JobEntry {
    let channel_id = tasks_yaml::resolve_channel_id(pool, def.channel.as_deref()).await;
    let prompt_preview = def
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
    let enabled = def.enabled;
    let action_id = def.action.clone();
    let action_name = action_id.as_deref().and_then(|a| {
        super::actions::load_actions(data_dir)
            .actions
            .get(a)
            .and_then(|act| act.description.clone())
    });
    JobEntry {
        id: key.to_string(),
        name: key.to_string(),
        display_name: def.display_name.clone().unwrap_or_else(|| key.to_string()),
        schedule: def.cron.clone(),
        prompt_preview,
        prompt: def.prompt.clone(),
        skills: parse_skills(def.skills.clone()),
        enabled,
        active: enabled,
        mode: Some(def.mode()),
        action_id,
        action_name,
        channel_id,
        channel_name: def.channel.clone(),
        profile: def.profile.clone(),
        last_run: None,
        next_run: None,
        last_run_at: None,
        next_run_at: None,
        created_at: String::new(),
        status: if enabled {
            "active".to_string()
        } else {
            "paused".to_string()
        },
        silent: def.silent.unwrap_or(false),
        template: def.template.clone(),
        plan: def.plan().unwrap_or(false),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /schedule: list schedules from tasks.yml (active filter = enabled).
async fn list_schedule_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListScheduleQuery>,
) -> impl IntoResponse {
    let active_only = params.active.as_deref() != Some("false");
    let tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[schedule] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load schedules from tasks.yml",
            );
        }
    };
    let mut keys: Vec<&String> = tasks
        .schedules
        .iter()
        .filter(|(_, def)| !active_only || def.enabled)
        .map(|(k, _)| k)
        .collect();
    keys.sort();
    let mut jobs: Vec<JobEntry> = Vec::with_capacity(keys.len());
    for key in keys {
        let def = &tasks.schedules[key];
        jobs.push(schedule_to_entry(&state.pool, &state.data_dir, key, def).await);
    }
    ok_json(jobs)
}

/// GET /schedule/{id}: single schedule detail from tasks.yml.
async fn get_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[schedule/{}] load tasks.yml failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load schedules from tasks.yml",
            );
        }
    };
    match tasks.schedules.get(&id) {
        Some(def) => {
            let entry = schedule_to_entry(&state.pool, &state.data_dir, &id, def).await;
            ok_json(entry)
        }
        None => err_json(StatusCode::NOT_FOUND, "Job not found"),
    }
}

/// POST /schedule: create or upsert a schedule in tasks.yml.
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
    let mode = body.mode.as_deref().unwrap_or("agentic");
    let action_id = body.action_id.as_deref().map(str::trim).unwrap_or("");
    if mode == "action" && action_id.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "mode=action requires an action_id");
    }
    if mode != "agentic" && mode != "action" {
        return err_json(
            StatusCode::BAD_REQUEST,
            "mode must be 'agentic' or 'action'",
        );
    }

    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[schedule] load tasks.yml failed: {:?}", e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load schedules from tasks.yml",
            );
        }
    };

    // Channel: id → NAME for the yml (unknown id → no channel = default).
    let channel_name = tasks_yaml::channel_name_for_id(&state.pool, body.channel_id).await;

    let mut def = ScheduleDef {
        cron: schedule_val.to_string(),
        prompt: body.prompt.clone().filter(|p| !p.trim().is_empty()),
        channel: channel_name,
        profile: body.profile.clone().filter(|p| !p.trim().is_empty()),
        display_name: body.display_name.clone().filter(|d| !d.trim().is_empty()),
        template: body.template.clone().filter(|t| !t.trim().is_empty()),
        silent: body.silent,
        ..Default::default()
    };
    if mode == "action" {
        def.action = Some(action_id.to_string());
    }
    // Legacy create defaulted plan=true for new jobs — preserve that.
    def.plan = Some(body.plan.unwrap_or(true));
    // yml has no separate `active` column: enabled doubles as active.
    def.enabled = body.enabled.or(body.active).unwrap_or(true);

    if let Err(err) = tasks_yaml::validate_schedule(&job_id, &def) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }
    tasks.schedules.insert(job_id.clone(), def);
    if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
        error!("[schedule] save tasks.yml failed: {:?}", e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save schedule to tasks.yml",
        );
    }
    ok_json(serde_json::json!({ "success": true, "id": job_id }))
}

/// PATCH /schedule/{id}: update schedule fields in tasks.yml.
/// Text fields: None = keep, Some("") = clear, Some(v) = set.
/// Booleans: None = keep.
async fn update_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateScheduleRequest>,
) -> impl IntoResponse {
    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[schedule/{}] load tasks.yml failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load schedules from tasks.yml",
            );
        }
    };
    let def = match tasks.schedules.get_mut(&id) {
        Some(d) => d,
        None => return err_json(StatusCode::NOT_FOUND, "Job not found"),
    };

    // Validate cron if being updated
    if let Some(ref sched) = body.schedule {
        if let Some(err) = validate_cron(sched) {
            return err_json(StatusCode::BAD_REQUEST, &err);
        }
        def.cron = sched.clone();
    }
    if let Some(p) = body.prompt.clone() {
        def.prompt = if p.is_empty() { None } else { Some(p) };
    }
    if let Some(d) = body.display_name.clone() {
        def.display_name = if d.is_empty() { None } else { Some(d) };
    }
    if let Some(p) = body.profile.clone() {
        def.profile = if p.is_empty() { None } else { Some(p) };
    }
    if let Some(t) = body.template.clone() {
        def.template = if t.is_empty() { None } else { Some(t) };
    }
    if let Some(silent) = body.silent {
        def.silent = Some(silent);
    }
    if let Some(en) = body.enabled {
        def.enabled = en;
    }
    if let Some(active) = body.active {
        def.enabled = active;
    }
    if let Some(mode) = body.mode.as_deref() {
        if mode == "action" {
            if body
                .action_id
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                return err_json(StatusCode::BAD_REQUEST, "mode=action requires an action_id");
            }
            def.action = body.action_id.clone().filter(|a| !a.trim().is_empty());
        } else if mode == "agentic" {
            def.action = None;
        } else {
            return err_json(
                StatusCode::BAD_REQUEST,
                "mode must be 'agentic' or 'action'",
            );
        }
    } else if let Some(action_id) = body.action_id.clone() {
        if action_id.trim().is_empty() {
            def.action = None;
        } else {
            def.action = Some(action_id);
        }
    }
    if let Some(cid) = body.channel_id {
        if cid.trim().is_empty() {
            def.channel = None;
        } else {
            def.channel = tasks_yaml::channel_name_for_id(&state.pool, Some(cid)).await;
        }
    }
    if let Some(plan) = body.plan {
        def.plan = Some(plan);
    }

    if let Err(err) = tasks_yaml::validate_schedule(&id, def) {
        return err_json(StatusCode::BAD_REQUEST, &err);
    }
    if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
        error!("[schedule/{}] save tasks.yml failed: {:?}", id, e);
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save schedule to tasks.yml",
        );
    }
    ok_json(serde_json::json!({ "success": true }))
}

/// PATCH /schedule/{id}/toggle: toggle the enabled state of a schedule.
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
    let mut tasks = match tasks_yaml::load_tasks(&state.data_dir) {
        Ok(t) => t,
        Err(e) => {
            error!("[schedule/{}/toggle] load tasks.yml failed: {:?}", id, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load schedules from tasks.yml",
            );
        }
    };
    match tasks.schedules.get_mut(&id) {
        Some(def) => {
            def.enabled = active;
            if let Err(e) = tasks_yaml::save_tasks(&state.data_dir, &tasks) {
                error!("[schedule/{}/toggle] save failed: {:?}", id, e);
                return err_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to save schedule to tasks.yml",
                );
            }
            ok_json(serde_json::json!({ "success": true, "active": active }))
        }
        None => err_json(StatusCode::NOT_FOUND, "Job not found"),
    }
}

/// GET /schedule/{id}/threads: threads for a schedule task with pagination.
///
/// SQL queries used: 2 (COUNT + paginated SELECT with LATERAL join)
async fn schedule_threads_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<ThreadsQueryParams>,
) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0).max(0);
    let limit = params.limit.unwrap_or(10).clamp(1, 100);
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
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to count threads");
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
            t.duration_ms AS processing_time_ms,
            last_msg.token_usage,
            last_msg.iteration_number,
            last_msg.thread_sequence,
            last_msg.created_at,
            last_msg.metadata::text AS metadata,
            t.status AS thread_status,
            t.channel_id AS channel_name
        FROM threads t
        LEFT JOIN LATERAL (
            SELECT m.id, m.thread_id, m.role, m.content, m.msg_type,
                   m.msg_subtype, NULL::text AS token_usage,
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
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch threads");
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

/// GET /schedule/{id}/subtasks: subtasks for all threads of a schedule job.
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

/// POST /schedule/{id}/run: manually trigger a schedule.
///
/// Delegates to `crate::scheduler::fire_cron_job_by_id` (the same function
/// used by the existing `/run-cron/{schedule_id}` endpoint).
///
/// No SQL queries here: the actual run logic is in the scheduler module.
async fn run_schedule_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<RunScheduleRequest>,
) -> impl IntoResponse {
    let _force = body.force.unwrap_or(false);

    match crate::scheduler::fire_cron_job_by_id(
        &state.pool,
        &state.data_dir,
        &state.plugin_manager,
        &state.app_context,
        &id,
        true,
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
