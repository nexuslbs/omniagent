use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use sqlx::{PgPool, Row};

/// Update a kanban task's status by task_id.
pub async fn update_kanban_status(pool: &PgPool, task_id: &str, status: &str) -> anyhow::Result<()> {
    sql_forge!(
        "UPDATE kanban_tasks SET status = :status, updated_at = NOW() WHERE id = :id",
        ( :status = status, :id = task_id )
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ── Kanban History ──

/// Escape a string literal for safe SQL embedding (single quotes only).
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

/// Insert a kanban_history record using raw executor.
pub async fn insert_kanban_history(
    pool: &PgPool,
    task_id: &str,
    action: &str,
    initial_board: Option<&str>,
    final_board: Option<&str>,
    previous_values: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let initial = sql_escape(initial_board.unwrap_or(""));
    let final_b = sql_escape(final_board.unwrap_or(""));
    let pv_str = previous_values
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());

    let sql = format!(
        "INSERT INTO kanban_history (kanban_task_id, action, initial_board, final_board, previous_values) \
         VALUES ('{}', '{}', NULLIF('{}', '')::text, NULLIF('{}', '')::text, '{}'::jsonb)",
        sql_escape(task_id),
        sql_escape(action),
        initial,
        final_b,
        pv_str.replace('\'', "''"),
    );

    sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await?;
    Ok(())
}

/// Fetch a single kanban task row by id.
pub async fn get_kanban_task(pool: &PgPool, task_id: &str) -> anyhow::Result<Option<KanbanTaskDb>> {
    let rows = sql_forge!(
        KanbanTaskDb,
        r#"
        SELECT id, title, body, status, priority, assignee, profile, template, archived, position, created_at, updated_at
        FROM kanban_tasks
        WHERE id = :id
        "#,
        ( :id = task_id )
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().next())
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct KanbanTaskDb {
    pub id: String,
    pub title: String,
    pub body: Option<String>,
    pub status: String,
    pub priority: Option<i32>,
    pub assignee: Option<String>,
    pub profile: Option<String>,
    pub template: Option<String>,
    pub archived: Option<bool>,
    pub position: Option<i32>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ── History query types ──

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct KanbanHistoryRow {
    pub id: i64,
    pub kanban_task_id: String,
    pub action: String,
    pub initial_board: Option<String>,
    pub final_board: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KanbanHistoryParams {
    pub task_id: Option<String>,
    pub action: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// List kanban history with optional filters.
pub async fn list_kanban_history(
    pool: &PgPool,
    params: &KanbanHistoryParams,
) -> anyhow::Result<Vec<KanbanHistoryRow>> {
    let limit: i64 = params.limit.unwrap_or(50).max(0).min(500);
    let offset: i64 = params.offset.unwrap_or(0).max(0);

    let mut where_clauses = Vec::new();
    if let Some(ref tid) = params.task_id {
        if !tid.is_empty() {
            where_clauses.push(format!("kanban_task_id = '{}'", sql_escape(tid)));
        }
    }
    if let Some(ref act) = params.action {
        if !act.is_empty() {
            where_clauses.push(format!("action = '{}'", sql_escape(act)));
        }
    }

    let where_sql = if where_clauses.is_empty() {
        String::from("1=1")
    } else {
        where_clauses.join(" AND ")
    };

    let sql = format!(
        "SELECT id, kanban_task_id, action, initial_board, final_board, \
         created_at::text AS created_at \
         FROM kanban_history WHERE {} \
         ORDER BY id DESC LIMIT {} OFFSET {}",
        where_sql, limit, offset,
    );

    let rows: Vec<sqlx::postgres::PgRow> = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            KanbanHistoryRow {
                id: r.get("id"),
                kanban_task_id: r.get("kanban_task_id"),
                action: r.get("action"),
                initial_board: r.get("initial_board"),
                final_board: r.get("final_board"),
                created_at: r.get("created_at"),
            }
        })
        .collect())
}
