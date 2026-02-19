use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddDownloadRequest {
    pub provider: String,
    pub provider_album_id: String,
    pub quality: Option<String>,
    pub library_id: Option<String>,
}

/// POST /downloads — queue an album for download from a streaming provider
pub async fn add_download(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddDownloadRequest>,
) -> Result<Json<Value>, AppError> {
    tracing::info!(
        "add_download: provider={}, album_id={}, quality={:?}, library_id={:?}",
        body.provider, body.provider_album_id, body.quality, body.library_id
    );
    let registry = state.plugin_registry.read().await;
    let streaming = registry
        .streaming_providers()
        .iter()
        .find(|p| p.provider_name() == body.provider)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "streaming provider '{}' not found",
                body.provider
            ))
        })?
        .clone();
    drop(registry);

    // Fetch album metadata so we can populate the queue entry
    let detail = streaming
        .get_album(&body.provider_album_id)
        .await
        .map_err(|e| AppError::Internal(format!("failed to fetch album from provider: {e}")))?;

    let id = uuid::Uuid::new_v4().to_string();
    let quality = body.quality.clone().unwrap_or_else(|| "lossless".to_string());

    sqlx::query(
        "INSERT INTO download_queue (id, provider, provider_album_id, album_title, artist_name, cover_art_url, quality, library_id, status, tracks_total)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?)
         ON CONFLICT (provider, provider_album_id) DO UPDATE SET status = 'queued', quality = excluded.quality, library_id = excluded.library_id, error = NULL"
    )
    .bind(&id)
    .bind(&body.provider)
    .bind(&body.provider_album_id)
    .bind(&detail.album.title)
    .bind(&detail.album.artist.name)
    .bind(&detail.album.cover_url)
    .bind(&quality)
    .bind(&body.library_id)
    .bind(detail.tracks.len() as i64)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "id": id,
        "status": "queued",
        "album_title": detail.album.title,
        "artist_name": detail.album.artist.name,
        "tracks_total": detail.tracks.len(),
    })))
}

/// GET /downloads — list download queue
pub async fn list_downloads(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query_as::<_, DownloadRow>(
        "SELECT id, provider, provider_album_id, album_title, artist_name, cover_art_url, quality,
                status, tracks_total, tracks_completed, current_track, error, local_album_id,
                created_at, completed_at
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
        "UPDATE download_queue SET status = 'cancelled' WHERE id = ? AND status IN ('queued', 'downloading')"
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
    created_at: String,
    completed_at: Option<String>,
}
