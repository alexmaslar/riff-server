use axum::{
    extract::{Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleFavoriteBody {
    pub entity_type: String,
    pub entity_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFavoritesParams {
    pub r#type: Option<String>,
    pub limit: Option<i64>,
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckFavoriteParams {
    pub entity_type: String,
    pub entity_id: String,
}

/// POST /favorites — Toggle favorite (atomic: try DELETE first, INSERT if nothing was removed)
pub async fn toggle_favorite(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<ToggleFavoriteBody>,
) -> Result<Json<Value>, AppError> {
    if !matches!(body.entity_type.as_str(), "album" | "artist" | "track") {
        return Err(AppError::BadRequest("Invalid entity_type".to_string()));
    }

    // Atomic per-statement in SQLite WAL mode: try to delete first.
    let result = sqlx::query(
        "DELETE FROM favorites WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&body.entity_type)
    .bind(&body.entity_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() > 0 {
        // Was favorited, now removed
        Ok(Json(json!({ "favorited": false })))
    } else {
        // Wasn't favorited, add it
        sqlx::query(
            "INSERT INTO favorites (user_id, entity_type, entity_id) VALUES (?, ?, ?)",
        )
        .bind(&claims.sub)
        .bind(&body.entity_type)
        .bind(&body.entity_id)
        .execute(&state.db)
        .await?;

        Ok(Json(json!({ "favorited": true })))
    }
}

/// GET /favorites?type=album — List favorites by entity_type
pub async fn list_favorites(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<ListFavoritesParams>,
) -> Result<([(axum::http::header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let limit = params.limit.unwrap_or(50);
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let entity_type = match params.r#type.as_deref() {
        Some(t) => t,
        None => return Err(AppError::BadRequest("type parameter is required".to_string())),
    };

    match entity_type {
        "album" => {
            let rows = sqlx::query(
                "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                        a.label, a.cover_art_path, a.added_at, a.play_count
                 FROM favorites f
                 JOIN albums a ON f.entity_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'album'
                 AND a.library_id IN (SELECT value FROM json_each(?))
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(&library_ids)
            .bind(limit)
            .fetch_all(&state.db)
            .await?;

            let albums: Vec<Value> = rows
                .iter()
                .map(|row| super::helpers::album_row_to_json(row))
                .collect();
            Ok((
                [(axum::http::header::CACHE_CONTROL, "private, max-age=60")],
                Json(json!({ "albums": albums })),
            ))
        }
        "artist" => {
            let rows = sqlx::query(
                "SELECT ar.id, ar.name, ar.bio, ar.image_url
                 FROM favorites f
                 JOIN artists ar ON f.entity_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'artist'
                 AND ar.library_id IN (SELECT value FROM json_each(?))
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(&library_ids)
            .bind(limit)
            .fetch_all(&state.db)
            .await?;

            let artists: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id": row.get::<String, _>("id"),
                        "name": row.get::<String, _>("name"),
                        "bio": row.get::<Option<String>, _>("bio"),
                        "image_url": row.get::<Option<String>, _>("image_url"),
                    })
                })
                .collect();
            Ok((
                [(axum::http::header::CACHE_CONTROL, "private, max-age=60")],
                Json(json!({ "artists": artists })),
            ))
        }
        "track" => {
            let rows = sqlx::query(
                "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                        t.format, t.album_id, ar.name as artist_name
                 FROM favorites f
                 JOIN tracks t ON f.entity_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'track'
                 AND a.library_id IN (SELECT value FROM json_each(?))
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(&library_ids)
            .bind(limit)
            .fetch_all(&state.db)
            .await?;

            let tracks: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id": row.get::<String, _>("id"),
                        "title": row.get::<String, _>("title"),
                        "track_number": row.get::<i32, _>("track_number"),
                        "disc_number": row.get::<i32, _>("disc_number"),
                        "duration_seconds": row.get::<i32, _>("duration_seconds"),
                        "format": row.get::<String, _>("format"),
                        "album_id": row.get::<String, _>("album_id"),
                        "artist_name": row.get::<String, _>("artist_name"),
                    })
                })
                .collect();
            Ok((
                [(axum::http::header::CACHE_CONTROL, "private, max-age=60")],
                Json(json!({ "tracks": tracks })),
            ))
        }
        _ => Err(AppError::BadRequest("invalid type, must be album, artist, or track".to_string())),
    }
}

/// GET /favorites/check?entityType=album&entityId=uuid — Check if favorited
pub async fn check_favorite(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<CheckFavoriteParams>,
) -> Result<Json<Value>, AppError> {
    if !matches!(params.entity_type.as_str(), "album" | "artist" | "track") {
        return Err(AppError::BadRequest("Invalid entity_type".to_string()));
    }

    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&params.entity_type)
    .bind(&params.entity_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "favorited": count > 0 })))
}
