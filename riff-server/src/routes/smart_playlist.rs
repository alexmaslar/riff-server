use axum::{extract::State, Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

/// GET /playlists/ai/suggestions
pub async fn get_suggestions(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let suggestions =
        riff_core::smart_playlist::generate_suggestions(&state.db, &claims.sub).await?;

    let items: Vec<Value> = suggestions
        .into_iter()
        .map(|s| {
            json!({
                "label": s.label,
                "prompt": s.prompt,
            })
        })
        .collect();

    Ok(Json(json!({
        "suggestions": items,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveBody {
    pub title: String,
    pub description: Option<String>,
    pub track_ids: Vec<String>,
}

/// POST /playlists/ai/save
pub async fn save(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<SaveBody>,
) -> Result<Json<Value>, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title is required".to_string()));
    }
    if body.track_ids.is_empty() {
        return Err(AppError::BadRequest("track_ids cannot be empty".to_string()));
    }

    let playlist_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO playlists (id, user_id, name, description) VALUES (?, ?, ?, ?)")
        .bind(&playlist_id)
        .bind(&claims.sub)
        .bind(title)
        .bind(&body.description)
        .execute(&state.db)
        .await?;

    for (i, track_id) in body.track_ids.iter().enumerate() {
        let pt_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
        )
        .bind(&pt_id)
        .bind(&playlist_id)
        .bind(track_id)
        .bind(i as i64)
        .execute(&state.db)
        .await?;
    }

    // Get total duration
    let total_duration = sqlx::query_as::<_, (i64,)>(
        "SELECT COALESCE(SUM(t.duration_seconds), 0)
         FROM playlist_tracks pt JOIN tracks t ON pt.track_id = t.id
         WHERE pt.playlist_id = ?",
    )
    .bind(&playlist_id)
    .fetch_one(&state.db)
    .await
    .map(|(d,)| d)
    .unwrap_or(0);

    Ok(Json(json!({
        "id": playlist_id,
        "name": title,
        "description": body.description,
        "trackCount": body.track_ids.len(),
        "totalDuration": total_duration,
    })))
}
