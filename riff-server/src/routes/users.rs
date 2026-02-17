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
pub struct UpdateAccountRequest {
    pub username: Option<String>,
    pub current_password: Option<String>,
    pub new_password: Option<String>,
    pub default_library_id: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub role: Option<String>,
}

pub async fn list_users(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT id, username, role, created_at FROM users ORDER BY username",
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let users: Vec<Value> = rows
        .into_iter()
        .map(|(id, username, role, created_at)| {
            json!({
                "id": id,
                "username": username,
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
        "INSERT INTO users (id, username, password_hash, role) VALUES (?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&req.username)
    .bind(&hash)
    .bind(&role)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    Ok(Json(json!({
        "id": id.to_string(),
        "username": req.username,
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

pub async fn update_account(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<Value>, AppError> {
    let user_id = &claims.sub;

    // Update username if provided
    if let Some(ref username) = req.username {
        let trimmed = username.trim();
        if trimmed.is_empty() {
            return Err(AppError::BadRequest("username cannot be empty".into()));
        }

        // Check uniqueness
        let (count,) = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM users WHERE username = ? AND id != ?",
        )
        .bind(trimmed)
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;

        if count > 0 {
            return Err(AppError::BadRequest("username already taken".into()));
        }

        sqlx::query("UPDATE users SET username = ? WHERE id = ?")
            .bind(trimmed)
            .bind(user_id)
            .execute(&state.db)
            .await?;
    }

    // Update password if provided
    if let Some(ref new_password) = req.new_password {
        let current_password = req
            .current_password
            .as_deref()
            .ok_or_else(|| AppError::BadRequest("current_password required to change password".into()))?;

        // Verify current password
        let (password_hash,) = sqlx::query_as::<_, (String,)>(
            "SELECT password_hash FROM users WHERE id = ?",
        )
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;

        match auth::verify_password(current_password, &password_hash) {
            Ok(true) => {}
            Ok(false) => return Err(AppError::Unauthorized("current password is incorrect".into())),
            Err(e) => return Err(AppError::Internal(e.to_string())),
        }

        if new_password.len() < 4 {
            return Err(AppError::BadRequest("new password must be at least 4 characters".into()));
        }

        let hash = auth::hash_password(new_password)
            .map_err(|e| AppError::Internal(e.to_string()))?;

        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(&hash)
            .bind(user_id)
            .execute(&state.db)
            .await?;
    }

    // Update default_library_id if provided
    if let Some(ref lib_id_opt) = req.default_library_id {
        if let Some(ref lib_id) = lib_id_opt {
            // Validate library exists
            let (count,) = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM libraries WHERE id = ?",
            )
            .bind(lib_id)
            .fetch_one(&state.db)
            .await?;

            if count == 0 {
                return Err(AppError::BadRequest("library not found".into()));
            }

            sqlx::query("UPDATE users SET default_library_id = ? WHERE id = ?")
                .bind(lib_id)
                .bind(user_id)
                .execute(&state.db)
                .await?;
        } else {
            // Clear default library
            sqlx::query("UPDATE users SET default_library_id = NULL WHERE id = ?")
                .bind(user_id)
                .execute(&state.db)
                .await?;
        }
    }

    // Return updated user
    let row = sqlx::query_as::<_, (String, String, String, Option<String>)>(
        "SELECT id, username, role, default_library_id FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "id": row.0,
        "username": row.1,
        "role": row.2,
        "default_library_id": row.3,
    })))
}
