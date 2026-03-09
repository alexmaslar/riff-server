use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// POST /downloads — streaming provider downloads are no longer supported
pub async fn add_download(
    State(_state): State<Arc<AppState>>,
    Json(_body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    Err(AppError::BadRequest(
        "streaming provider downloads are not available".to_string(),
    ))
}

/// GET /downloads — list download queue
pub async fn list_downloads(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query_as::<_, DownloadRow>(
        "SELECT id, provider, provider_album_id, album_title, artist_name, cover_art_url, quality,
                status, tracks_total, tracks_completed, current_track, error, local_album_id,
                processing_stage, created_at, completed_at
         FROM download_queue
         ORDER BY created_at DESC
         LIMIT 100"
    )
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "provider": r.provider,
                "provider_album_id": r.provider_album_id,
                "album_title": r.album_title,
                "artist_name": r.artist_name,
                "cover_art_url": r.cover_art_url,
                "quality": r.quality,
                "status": r.status,
                "tracks_total": r.tracks_total,
                "tracks_completed": r.tracks_completed,
                "current_track": r.current_track,
                "error": r.error,
                "local_album_id": r.local_album_id,
                "processing_stage": r.processing_stage,
                "stage_description": r.processing_stage.as_ref().map(|s| match s.as_str() {
                    "scanning" => "Scanning library...",
                    "enriching" => "Enriching metadata...",
                    "editorial" => "Fetching reviews...",
                    "rating" => "Rating...",
                    "extracting_tags" => "Extracting tags...",
                    "complete" => "Complete",
                    other => other,
                }),
                "created_at": r.created_at,
                "completed_at": r.completed_at,
            })
        })
        .collect();

    Ok(Json(json!({ "downloads": items })))
}

/// DELETE /downloads/{id} — cancel a queued or in-progress download
pub async fn cancel_download(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query(
        "UPDATE download_queue SET status = 'cancelled' WHERE id = ? AND status IN ('queued', 'downloading', 'processing')"
    )
    .bind(&id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "download not found or already completed".to_string(),
        ));
    }

    Ok(Json(json!({ "status": "cancelled" })))
}

#[derive(Debug, sqlx::FromRow)]
struct DownloadRow {
    id: String,
    provider: String,
    provider_album_id: String,
    album_title: String,
    artist_name: String,
    cover_art_url: Option<String>,
    quality: String,
    status: String,
    tracks_total: i64,
    tracks_completed: i64,
    current_track: Option<String>,
    error: Option<String>,
    local_album_id: Option<String>,
    processing_stage: Option<String>,
    created_at: String,
    completed_at: Option<String>,
}
