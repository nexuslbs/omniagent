//! Secrets API — user-managed key/value store with versioning.
//!
//! - `GET /api/secrets` — list all secrets with current values
//! - `POST /api/secrets` — create a new secret
//! - `PUT /api/secrets/:name` — update a secret (versions the old value)
//! - `GET /api/secrets/:name/versions` — list all versions of a secret
//! - `DELETE /api/secrets/:name` — delete a secret and all its versions

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sql_forge::sql_forge;
use std::sync::Arc;
use tracing::{error, info};

use super::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SecretEntry {
    pub id: i64,
    pub name: String,
    pub field_type: String,
    pub current_value: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SecretVersionEntry {
    pub id: i64,
    pub version_number: i32,
    pub value: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSecretRequest {
    pub name: String,
    #[serde(rename = "fieldType")]
    pub field_type: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSecretRequest {
    pub value: String,
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

fn ok_json<T: Serialize>(data: T) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "success": true, "data": data })),
    )
}

fn err_json(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "error": msg })),
    )
}

fn fmt_ts(ts: &chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn secrets_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/secrets", get(list_secrets_handler))
        .route("/secrets", post(create_secret_handler))
        .route("/secrets/{name}", put(update_secret_handler))
        .route("/secrets/{name}/versions", get(list_versions_handler))
        .route("/secrets/{name}", delete(delete_secret_handler))
}

#[derive(Debug, sqlx::FromRow)]
struct SecretRow {
    id: i64,
    name: String,
    field_type: String,
    current_value: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct SecretIdRow {
    id: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct SecretCurrentRow {
    id: i64,
    name: String,
    field_type: String,
    current_value: String,
}

#[derive(Debug, sqlx::FromRow)]
struct SecretUpdateRow {
    field_type: String,
    current_value: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct SecretVersionRow {
    id: i64,
    version_number: i32,
    value: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct MaxVersionRow {
    coalesce: Option<i32>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/secrets — list all secrets with current values.
async fn list_secrets_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = match sql_forge!(
        SecretRow,
        r#"
        SELECT id, name, field_type, current_value, created_at, updated_at
        FROM secrets
        ORDER BY name ASC
        "#,
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to list secrets: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list secrets");
        }
    };

    let secrets: Vec<SecretEntry> = rows
        .into_iter()
        .map(|row| SecretEntry {
            id: row.id,
            name: row.name,
            field_type: row.field_type,
            current_value: row.current_value,
            created_at: fmt_ts(&row.created_at),
            updated_at: fmt_ts(&row.updated_at),
        })
        .collect();

    ok_json(secrets)
}

/// POST /api/secrets — create a new secret.
async fn create_secret_handler(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSecretRequest>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "Name is required");
    }
    if name.len() > 255 {
        return err_json(
            StatusCode::BAD_REQUEST,
            "Name must be 255 characters or fewer",
        );
    }
    let field_type = match body.field_type.as_str() {
        "text" => "text",
        "password" => "password",
        _ => "password",
    };

    match sql_forge!(
        SecretRow,
        r#"
        INSERT INTO secrets (name, field_type, current_value)
        VALUES (:name, :field_type, :value)
        RETURNING id, name, field_type, current_value, created_at, updated_at
        "#,
        ( :name = &name, :field_type = field_type, :value = &body.value )
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => {
            info!("Created secret '{}'", name);
            ok_json(SecretEntry {
                id: row.id,
                name: row.name,
                field_type: row.field_type,
                current_value: row.current_value,
                created_at: fmt_ts(&row.created_at),
                updated_at: fmt_ts(&row.updated_at),
            })
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("unique") || msg.contains("duplicate") {
                err_json(
                    StatusCode::CONFLICT,
                    "A secret with this name already exists",
                )
            } else {
                error!("Failed to create secret: {:?}", e);
                err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to create secret")
            }
        }
    }
}

/// PUT /api/secrets/:name — update a secret (versions the old value first).
async fn update_secret_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<UpdateSecretRequest>,
) -> impl IntoResponse {
    // 1. Get the current value
    let current = match sql_forge!(
        SecretCurrentRow,
        r#"
        SELECT id, name, field_type, current_value
        FROM secrets
        WHERE name = :name
        "#,
        ( :name = &name )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => (row.id, row.name, row.field_type, row.current_value),
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Secret not found"),
        Err(e) => {
            error!("Failed to fetch secret '{}': {:?}", name, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to fetch secret");
        }
    };

    let (secret_id, _secret_name, _field_type, old_value) = current;

    // 2. Start a transaction
    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!("Failed to begin transaction: {:?}", e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    // 3. If the old value is non-empty, version it
    if !old_value.is_empty() {
        let max_ver: Option<MaxVersionRow> = sql_forge!(
            MaxVersionRow,
            r#"
            SELECT COALESCE(MAX(version_number), 0) AS coalesce
            FROM secret_versions WHERE secret_id = :secret_id
            "#,
            ( :secret_id = secret_id )
        )
        .fetch_optional(&mut *tx)
        .await
        .unwrap_or(None);

        let next_ver = max_ver
            .map(|row| row.coalesce.unwrap_or(0) + 1)
            .unwrap_or(1);

        if let Err(e) = sql_forge!(
            r#"
            INSERT INTO secret_versions (secret_id, version_number, value)
            VALUES (:secret_id, :next_ver, :old_value)
            "#,
            ( :secret_id = secret_id, :next_ver = next_ver, :old_value = &old_value )
        )
        .execute(&mut *tx)
        .await
        {
            let _ = tx.rollback().await;
            error!("Failed to version old value for '{}': {:?}", name, e);
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to version old value",
            );
        }
    }

    // 4. Update the current value
    let update_result = sql_forge!(
        SecretUpdateRow,
        r#"
        UPDATE secrets
        SET current_value = :value, updated_at = NOW()
        WHERE id = :secret_id
        RETURNING field_type, current_value, updated_at
        "#,
        ( :value = &body.value, :secret_id = secret_id )
    )
    .fetch_one(&mut *tx)
    .await;

    match update_result {
        Ok(row) => {
            if let Err(e) = tx.commit().await {
                error!("Failed to commit transaction: {:?}", e);
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to commit");
            }
            info!("Updated secret '{}' (versioned old value)", name);
            ok_json(SecretEntry {
                id: secret_id,
                name,
                field_type: row.field_type,
                current_value: row.current_value,
                created_at: fmt_ts(&chrono::Utc::now()),
                updated_at: fmt_ts(&row.updated_at),
            })
        }
        Err(e) => {
            let _ = tx.rollback().await;
            error!("Failed to update secret '{}': {:?}", name, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to update secret")
        }
    }
}

/// GET /api/secrets/:name/versions — list all versions of a secret.
async fn list_versions_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    // First get the secret id
    let secret_id = match sql_forge!(
        SecretIdRow,
        r#"SELECT id FROM secrets WHERE name = :name"#,
        ( :name = &name )
    )
    .fetch_optional(&state.pool)
    .await
    {
        Ok(Some(row)) => row.id,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "Secret not found"),
        Err(e) => {
            error!("Failed to find secret '{}': {:?}", name, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Database error");
        }
    };

    let rows = match sql_forge!(
        SecretVersionRow,
        r#"
        SELECT id, version_number, value, created_at
        FROM secret_versions
        WHERE secret_id = :secret_id
        ORDER BY version_number DESC
        "#,
        ( :secret_id = secret_id )
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("Failed to list versions for secret '{}': {:?}", name, e);
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list versions");
        }
    };

    let versions: Vec<SecretVersionEntry> = rows
        .into_iter()
        .map(|row| SecretVersionEntry {
            id: row.id,
            version_number: row.version_number,
            value: row.value,
            created_at: fmt_ts(&row.created_at),
        })
        .collect();

    ok_json(versions)
}

/// DELETE /api/secrets/:name — delete a secret and all its versions (CASCADE).
async fn delete_secret_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match sql_forge!(
        r#"DELETE FROM secrets WHERE name = :name"#,
        ( :name = &name )
    )
    .execute(&state.pool)
    .await
    {
        Ok(result) => {
            if result.rows_affected() == 0 {
                err_json(StatusCode::NOT_FOUND, "Secret not found")
            } else {
                info!("Deleted secret '{}'", name);
                ok_json(serde_json::json!({ "deleted": true }))
            }
        }
        Err(e) => {
            error!("Failed to delete secret '{}': {:?}", name, e);
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete secret")
        }
    }
}
