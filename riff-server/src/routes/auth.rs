use axum::{extract::State, Json};
use riff_core::auth;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Json<Value> {
    let user = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT id, username, password_hash, role FROM users WHERE username = ?",
    )
    .bind(&req.username)
    .fetch_optional(&state.db)
    .await;

    let (id_str, username, password_hash, role) = match user {
        Ok(Some(row)) => row,
        Ok(None) => return Json(json!({ "error": "invalid credentials" })),
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    match auth::verify_password(&req.password, &password_hash) {
        Ok(true) => {}
        Ok(false) => return Json(json!({ "error": "invalid credentials" })),
        Err(e) => return Json(json!({ "error": e.to_string() })),
    }

    let user_id = match Uuid::parse_str(&id_str) {
        Ok(id) => id,
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    match auth::create_token(&user_id, &username, &role, &state.config.auth.jwt_secret) {
        Ok(token) => Json(json!({
            "token": token,
            "user": {
                "id": id_str,
                "username": username,
                "role": role,
            }
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return Json(json!({ "error": "missing authorization header" })),
    };

    let claims = match auth::validate_token(token, &state.config.auth.jwt_secret) {
        Ok(c) => c,
        Err(e) => return Json(json!({ "error": format!("invalid token: {}", e) })),
    };

    let user_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    match auth::create_token(&user_id, &claims.username, &claims.role, &state.config.auth.jwt_secret) {
        Ok(new_token) => Json(json!({ "token": new_token })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}
