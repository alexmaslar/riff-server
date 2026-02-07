use axum::{
    extract::{Path, State},
    Extension, Json,
};
use riff_core::auth::{self, Claims};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub display_name: String,
    pub role: Option<String>,
}

pub async fn list_users(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT id, username, display_name, role, created_at FROM users ORDER BY username",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let users: Vec<Value> = rows
        .into_iter()
        .map(|(id, username, display_name, role, created_at)| {
            json!({
                "id": id,
                "username": username,
                "display_name": display_name,
                "role": role,
                "created_at": created_at,
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })))
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<Json<Value>, AppError> {
    let hash = auth::hash_password(&req.password)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let id = Uuid::new_v4();
    let role = req.role.unwrap_or_else(|| "user".to_string());

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, display_name, role) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&req.username)
    .bind(&hash)
    .bind(&req.display_name)
    .bind(&role)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    Ok(Json(json!({
        "id": id.to_string(),
        "username": req.username,
        "display_name": req.display_name,
        "role": role,
    })))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    if claims.sub == user_id {
        return Err(AppError::Forbidden("cannot delete yourself".into()));
    }

    // Verify user exists
    let (count,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    if count == 0 {
        return Err(AppError::NotFound("user not found".into()));
    }

    // Delete related data then the user
    let queries = [
        "DELETE FROM favorites WHERE user_id = ?",
        "DELETE FROM play_history WHERE user_id = ?",
        "DELETE FROM playlist_tracks WHERE playlist_id IN (SELECT id FROM playlists WHERE user_id = ?)",
        "DELETE FROM playlists WHERE user_id = ?",
        "DELETE FROM users WHERE id = ?",
    ];

    for query in queries {
        sqlx::query(query).bind(&user_id).execute(&state.db).await
            .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    Ok(Json(json!({ "status": "deleted" })))
}
