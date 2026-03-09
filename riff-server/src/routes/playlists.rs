use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{Acquire, QueryBuilder, Row, Sqlite};
use std::sync::Arc;
use tokio::fs::File;
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::albums::CoverParams;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListPlaylistsParams {
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePlaylistParams {
    pub library: Option<String>,
}

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
    Query(params): Query<ListPlaylistsParams>,
) -> Result<([(axum::http::header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;
    let include_null_library = params.library.is_none();

    let rows = sqlx::query(
        "SELECT p.id, p.name, p.description,
                COUNT(pt.id) as track_count,
                COALESCE(SUM(t.duration_seconds), 0) as total_duration,
                p.created_at, p.updated_at,
                p.cover_path,
                COUNT(*) OVER() as total_count
         FROM playlists p
         LEFT JOIN playlist_tracks pt ON p.id = pt.playlist_id
         LEFT JOIN tracks t ON pt.track_id = t.id
         WHERE p.user_id = ?
         AND (p.library_id IN (SELECT value FROM json_each(?))
              OR (p.library_id IS NULL AND ?))
         GROUP BY p.id
         ORDER BY p.updated_at DESC",
    )
    .bind(&claims.sub)
    .bind(&library_ids)
    .bind(include_null_library)
    .fetch_all(&state.db)
    .await?;

    let total: Option<i64> = rows.first().map(|r| r.get("total_count"));
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
                "has_cover": row.get::<Option<String>, _>("cover_path").is_some(),
            })
        })
        .collect();
    Ok((
        [(axum::http::header::CACHE_CONTROL, "private, max-age=60")],
        Json(json!({ "playlists": playlists, "total": total })),
    ))
}

pub async fn get_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let playlist = sqlx::query_as::<_, (String, String, Option<String>, String, String, Option<String>)>(
        "SELECT id, name, description, created_at, updated_at, cover_path
         FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(&claims.sub)
    .fetch_optional(&state.db)
    .await?;

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
    .await?;

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
        "has_cover": playlist.5.is_some(),
    })))
}

pub async fn create_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<CreatePlaylistParams>,
    Json(body): Json<CreatePlaylistBody>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4().to_string();

    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;
    let lib_ids: Vec<String> = super::helpers::decode_json_array(&library_ids);
    let playlist_library_id = lib_ids.first().cloned();

    sqlx::query(
        "INSERT INTO playlists (id, user_id, name, description, library_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&claims.sub)
    .bind(&body.name)
    .bind(&body.description)
    .bind(&playlist_library_id)
    .execute(&state.db)
    .await?;

    let (id, name, description, created_at, updated_at) = sqlx::query_as::<_, (String, String, Option<String>, String, String)>(
        "SELECT id, name, description, created_at, updated_at FROM playlists WHERE id = ?",
    )
    .bind(&id)
    .fetch_one(&state.db)
    .await?;

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
        .await?;

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
    .await?;

    if count == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    let max_order = sqlx::query_as::<_, (i64,)>(
        "SELECT COALESCE(MAX(sort_order), -1) FROM playlist_tracks WHERE playlist_id = ?",
    )
    .bind(&playlist_id)
    .fetch_one(&state.db)
    .await?;

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&playlist_id)
    .bind(&body.track_id)
    .bind(max_order.0 + 1)
    .execute(&state.db)
    .await?;

    if let Err(e) = sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!(error = %e, "playlist timestamp update failed");
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
    .await?;

    if count == 0 {
        return Err(AppError::NotFound("playlist not found".into()));
    }

    sqlx::query(
        "DELETE FROM playlist_tracks WHERE playlist_id = ? AND track_id = ?",
    )
    .bind(&playlist_id)
    .bind(&track_id)
    .execute(&state.db)
    .await?;

    if let Err(e) = sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!(error = %e, "playlist timestamp update failed");
    }

    Ok(Json(json!({ "ok": true })))
}

pub async fn reorder_tracks(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(playlist_id): Path<String>,
    Json(body): Json<ReorderTracksBody>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db.acquire().await?;
    let mut tx = conn.begin().await?;

    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM playlists WHERE id = ? AND user_id = ?",
    )
    .bind(&playlist_id)
    .bind(&claims.sub)
    .fetch_one(&mut *tx)
    .await?;

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

        builder.build().execute(&mut *tx).await?;
    }

    sqlx::query("UPDATE playlists SET updated_at = datetime('now') WHERE id = ?")
        .bind(&playlist_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(json!({ "ok": true })))
}

/// GET /playlists/:id/cover — Serve playlist cover art
pub async fn get_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<CoverParams>,
) -> Response {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT cover_path FROM playlists WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let cover_path = match row {
        Ok(Some((Some(path),))) => path,
        Ok(Some((None,))) | Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "no cover art" })))
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    if let Some(w) = params.w {
        let w = w.clamp(50, 2000);
        return crate::routes::albums::serve_thumbnail(&cover_path, &id, w).await;
    }

    let file = match File::open(&cover_path).await {
        Ok(f) => f,
        Err(_) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "cover file missing" })))
                .into_response()
        }
    };

    let file_size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = tokio_util::io::ReaderStream::new(file);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CONTENT_LENGTH, file_size)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
