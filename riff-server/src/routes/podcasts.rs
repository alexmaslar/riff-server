use axum::{extract::State, Extension, Json};
use riff_core::auth::Claims;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// PUT /podcasts/backup — Save podcast state (subscriptions + progress)
pub async fn save_backup(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let data = serde_json::to_string(&body).map_err(|e| AppError::BadRequest(e.to_string()))?;

    // Reject payloads over 1MB
    if data.len() > 1_048_576 {
        return Err(AppError::BadRequest("backup too large (max 1MB)".into()));
    }

    sqlx::query(
        "INSERT INTO podcast_backups (user_id, data, updated_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT(user_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
    )
    .bind(&claims.sub)
    .bind(&data)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({"status": "ok"})))
}

/// GET /podcasts/backup — Retrieve podcast state
pub async fn get_backup(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT data FROM podcast_backups WHERE user_id = ?")
            .bind(&claims.sub)
            .fetch_optional(&state.db)
            .await?;

    match row {
        Some((data,)) => {
            let value: Value =
                serde_json::from_str(&data).map_err(|e| AppError::Internal(e.to_string()))?;
            Ok(Json(value))
        }
        None => Err(AppError::NotFound("no podcast backup found".into())),
    }
}
