use axum::{extract::State, Json};
use riff_core::auth;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, AppError> {
    let user = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(
        "SELECT id, username, password_hash, role, default_library_id FROM users WHERE username = ?",
    )
    .bind(&req.username)
    .fetch_optional(&state.db)
    .await;

    let (id_str, username, password_hash, role, default_library_id) = match user {
        Ok(Some(row)) => row,
        Ok(None) => return Err(AppError::Unauthorized("invalid credentials".into())),
        Err(e) => return Err(AppError::Internal(e.to_string())),
    };

    match auth::verify_password(&req.password, &password_hash) {
        Ok(true) => {}
        Ok(false) => return Err(AppError::Unauthorized("invalid credentials".into())),
        Err(e) => return Err(AppError::Internal(e.to_string())),
    }

    let user_id = match Uuid::parse_str(&id_str) {
        Ok(id) => id,
        Err(e) => return Err(AppError::BadRequest(e.to_string())),
    };

    match auth::create_token(&user_id, &username, &role, &state.config.read().await.auth.jwt_secret) {
        Ok(token) => Ok(Json(json!({
            "token": token,
            "user": {
                "id": id_str,
                "username": username,
                "role": role,
                "default_library_id": default_library_id,
            }
        }))),
        Err(e) => Err(AppError::Internal(e.to_string())),
    }
}

pub async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, AppError> {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return Err(AppError::Unauthorized("missing authorization header".into())),
    };

    let jwt_secret = state.config.read().await.auth.jwt_secret.clone();

    let claims = match auth::validate_token(token, &jwt_secret) {
        Ok(c) => c,
        Err(e) => return Err(AppError::Unauthorized(format!("invalid token: {}", e))),
    };

    let user_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(e) => return Err(AppError::BadRequest(e.to_string())),
    };

    match auth::create_token(&user_id, &claims.username, &claims.role, &jwt_secret) {
        Ok(new_token) => Ok(Json(json!({ "token": new_token }))),
        Err(e) => Err(AppError::Internal(e.to_string())),
    }
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}
