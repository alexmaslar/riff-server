use axum::{extract::State, Json};
use riff_core::auth;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub display_name: String,
    pub role: Option<String>,
}

pub async fn list_users(State(state): State<Arc<AppState>>) -> Json<Value> {
    let users = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT id, username, display_name, role, created_at FROM users ORDER BY username",
    )
    .fetch_all(&state.db)
    .await;

    match users {
        Ok(rows) => {
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
            Json(json!({ "users": users }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateUserRequest>,
) -> Json<Value> {
    let hash = match auth::hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    let id = Uuid::new_v4();
    let role = req.role.unwrap_or_else(|| "user".to_string());

    let result = sqlx::query(
        "INSERT INTO users (id, username, password_hash, display_name, role) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&req.username)
    .bind(&hash)
    .bind(&req.display_name)
    .bind(&role)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({
            "id": id.to_string(),
            "username": req.username,
            "display_name": req.display_name,
            "role": role,
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
