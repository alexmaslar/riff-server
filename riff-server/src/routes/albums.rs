use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::fs::File;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AlbumResponse {
    pub id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub cover_art_path: Option<String>,
    pub added_at: String,
}

#[derive(Debug, Serialize)]
pub struct AlbumDetailResponse {
    pub id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub cover_art_path: Option<String>,
    pub ai_summary: Option<String>,
    pub added_at: String,
    pub tracks: Vec<TrackSummary>,
}

#[derive(Debug, Serialize)]
pub struct TrackSummary {
    pub id: String,
    pub title: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub duration_seconds: i32,
    pub format: String,
}

pub async fn list_albums(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let order_clause = match params.sort.as_deref() {
        Some("year") => "a.year DESC, a.title",
        Some("added") => "a.added_at DESC",
        Some("artist") => "ar.name, a.year, a.title",
        _ => "a.title",
    };

    let query = if let Some(ref search) = params.search {
        let pattern = format!("%{}%", search);
        sqlx::query_as::<_, (String, String, String, String, Option<i32>, String, String, Option<String>, Option<String>, String)>(
            &format!(
                "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.cover_art_path, a.added_at
                 FROM albums a JOIN artists ar ON a.artist_id = ar.id
                 WHERE a.title LIKE ?1 OR ar.name LIKE ?1
                 ORDER BY {} LIMIT ?2 OFFSET ?3", order_clause
            ),
        )
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (String, String, String, String, Option<i32>, String, String, Option<String>, Option<String>, String)>(
            &format!(
                "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.cover_art_path, a.added_at
                 FROM albums a JOIN artists ar ON a.artist_id = ar.id
                 ORDER BY {} LIMIT ?1 OFFSET ?2", order_clause
            ),
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    };

    match query {
        Ok(rows) => {
            let albums: Vec<AlbumResponse> = rows
                .into_iter()
                .map(|(id, title, artist_id, artist_name, year, genre, style, label, cover_art_path, added_at)| {
                    AlbumResponse {
                        id,
                        title,
                        artist_id,
                        artist_name,
                        year,
                        genre: serde_json::from_str(&genre).unwrap_or_default(),
                        style: serde_json::from_str(&style).unwrap_or_default(),
                        label,
                        cover_art_path,
                        added_at,
                    }
                })
                .collect();
            Json(json!({ "albums": albums }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn get_album(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let album = sqlx::query_as::<_, (String, String, String, String, Option<i32>, String, String, Option<String>, Option<String>, Option<String>, Option<String>, String)>(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.cover_art_path, a.ai_summary, a.added_at
         FROM albums a JOIN artists ar ON a.artist_id = ar.id
         WHERE a.id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let album = match album {
        Ok(Some(row)) => row,
        Ok(None) => return Json(json!({ "error": "album not found" })),
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    let tracks = sqlx::query_as::<_, (String, String, i32, i32, i32, String)>(
        "SELECT id, title, track_number, disc_number, duration_seconds, format
         FROM tracks WHERE album_id = ? ORDER BY disc_number, track_number",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let track_summaries: Vec<TrackSummary> = tracks
        .into_iter()
        .map(|(id, title, track_number, disc_number, duration_seconds, format)| TrackSummary {
            id,
            title,
            track_number,
            disc_number,
            duration_seconds,
            format,
        })
        .collect();

    Json(json!(AlbumDetailResponse {
        id: album.0,
        title: album.1,
        artist_id: album.2,
        artist_name: album.3,
        year: album.4,
        genre: serde_json::from_str(&album.5).unwrap_or_default(),
        style: serde_json::from_str(&album.6).unwrap_or_default(),
        label: album.7,
        catalog_number: album.8,
        cover_art_path: album.9,
        ai_summary: album.10,
        added_at: album.11,
        tracks: track_summaries,
    }))
}

pub async fn get_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT cover_art_path FROM albums WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let cover_path = match row {
        Ok(Some((Some(path),))) => path,
        Ok(Some((None,))) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "no cover art" })))
                .into_response()
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "album not found" })))
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

    let mime = if cover_path.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };

    let file = match File::open(&cover_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("file open failed: {}", e) })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}
