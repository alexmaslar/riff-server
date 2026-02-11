use axum::{
    extract::{Path, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{QueryBuilder, Row, Sqlite};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistBody {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddTrackBody {
    pub track_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReorderTracksBody {
    pub track_ids: Vec<String>,
}

pub async fn list_playlists(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "SELECT p.id, p.name, p.description,
                COUNT(pt.id) as track_count,
                COALESCE(SUM(t.duration_seconds), 0) as total_duration,
                p.created_at, p.updated_at
         FROM playlists p
         LEFT JOIN playlist_tracks pt ON p.id = pt.playlist_id
         LEFT JOIN tracks t ON pt.track_id = t.id
         WHERE p.user_id = ?
         GROUP BY p.id
         ORDER BY p.updated_at DESC",
    )
    .bind(&claims.sub)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let playlists: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "name": row.get::<String, _>("name"),
                "description": row.get::<Option<String>, _>("description"),
                "track_count": row.get::<i64, _>("track_count"),
                "total_duration": row.get::<i64, _>("total_duration"),
                "created_at": row.get::<String, _>("created_at"),
                "updated_at": row.get::<String, _>("updated_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "playlists": playlists })))
}

pub async fn get_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let playlist = sqlx::query_as::<_, (String, String, Option<String>, String, String)>(
        "SELECT id, name, description, created_at, updated_at
         FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(&claims.sub)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let playlist = match playlist {
        Some(row) => row,
        None => return Err(AppError::NotFound("playlist not found".into())),
    };

    let tracks = sqlx::query_as::<_, (String, String, i32, i32, i32, String, String, String, Option<i64>, i64)>(
        "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds, t.format,
                t.album_id, ar.name, t.file_size_bytes, a.play_count
         FROM playlist_tracks pt
         JOIN tracks t ON pt.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE pt.playlist_id = ?
         ORDER BY pt.sort_order",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let track_list: Vec<Value> = tracks
        .into_iter()
        .map(
            |(id, title, track_number, disc_number, duration_seconds, format, album_id, artist_name, file_size_bytes, album_play_count)| {
                json!({
                    "id": id,
                    "title": title,
                    "track_number": track_number,
                    "disc_number": disc_number,
                    "duration_seconds": duration_seconds,
                    "format": format,
                    "album_id": album_id,
                    "artist_name": artist_name,
                    "file_size_bytes": file_size_bytes,
                    "album_play_count": album_play_count,
                })
            },
        )
        .collect();

    Ok(Json(json!({
        "id": playlist.0,
        "name": playlist.1,
        "description": playlist.2,
        "tracks": track_list,
        "created_at": playlist.3,
        "updated_at": playlist.4,
    })))
}

pub async fn create_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<CreatePlaylistBody>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO playlists (id, user_id, name, description) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&claims.sub)
    .bind(&body.name)
    .bind(&body.description)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let (id, name, description, created_at, updated_at) = sqlx::query_as::<_, (String, String, Option<String>, String, String)>(
        "SELECT id, name, description, created_at, updated_at FROM playlists WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(json!({
        "id": id,
        "name": name,
        "description": description,
        "track_count": 0,
        "total_duration": 0,
        "created_at": created_at,
        "updated_at": updated_at,
    })))
}

pub async fn delete_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query("DELETE FROM playlists WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(&claims.sub)
        .execute(&state.db)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    Ok(Json(json!({ "ok": true })))
}

pub async fn add_track(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(playlist_id): Path<String>,
    Json(body): Json<AddTrackBody>,
) -> Result<Json<Value>, AppError> {
    // Verify ownership
    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&playlist_id)
    .bind(&claims.sub)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    if count == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    let max_order = sqlx::query_as::<_, (i64,)>(
        "SELECT COALESCE(MAX(sort_order), -1) FROM playlist_tracks WHERE playlist_id = ?",
    )
    .bind(&playlist_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or((0,));

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&playlist_id)
    .bind(&body.track_id)
    .bind(max_order.0 + 1)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    if let Err(e) = sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!("playlist timestamp update failed: {e}");
    }

    Ok(Json(json!({ "ok": true })))
}

pub async fn remove_track(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path((playlist_id, track_id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&playlist_id)
    .bind(&claims.sub)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    if count == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    sqlx::query(
        "DELETE FROM playlist_tracks WHERE playlist_id = ? AND track_id = ?",
    )
    .bind(&playlist_id)
    .bind(&track_id)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    if let Err(e) = sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!("playlist timestamp update failed: {e}");
    }

    Ok(Json(json!({ "ok": true })))
}

pub async fn reorder_tracks(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(playlist_id): Path<String>,
    Json(body): Json<ReorderTracksBody>,
) -> Result<Json<Value>, AppError> {
    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&playlist_id)
    .bind(&claims.sub)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    if count == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    if !body.track_ids.is_empty() {
        // Batch reorder with a single UPDATE using CASE
        let mut builder = QueryBuilder::<Sqlite>::new(
            "UPDATE playlist_tracks SET sort_order = CASE track_id",
        );
        for (i, track_id) in body.track_ids.iter().enumerate() {
            builder.push(" WHEN ");
            builder.push_bind(track_id.clone());
            builder.push(" THEN ");
            builder.push_bind(i as i64);
        }
        builder.push(" END WHERE playlist_id = ");
        builder.push_bind(&playlist_id);
        builder.push(" AND track_id IN (");
        let mut sep = builder.separated(", ");
        for track_id in &body.track_ids {
            sep.push_bind(track_id.clone());
        }
        sep.push_unseparated(")");

        builder.build().execute(&state.db).await
            .map_err(|e| {
                tracing::warn!("playlist reorder failed: {e}");
                AppError::Internal("reorder failed".into())
            })?;
    }

    if let Err(e) = sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!("playlist timestamp update failed: {e}");
    }

    Ok(Json(json!({ "ok": true })))
}
