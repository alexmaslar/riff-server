use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ArtistResponse {
    pub id: String,
    pub name: String,
    pub bio: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtistDetailResponse {
    pub id: String,
    pub name: String,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub albums: Vec<AlbumSummary>,
    pub is_favorited: bool,
}

#[derive(Debug, Serialize)]
pub struct AlbumSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub cover_art_path: Option<String>,
}

pub async fn list_artists(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let rows = if let Some(ref search) = params.search {
        let pattern = format!("%{}%", search);
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT id, name, bio, image_url, COUNT(*) OVER() as total_count FROM artists WHERE name LIKE ? ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT id, name, bio, image_url, COUNT(*) OVER() as total_count FROM artists ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    };

    let rows = rows?;
    let total: Option<i64> = rows.first().map(|r| r.4);
    let artists: Vec<ArtistResponse> = rows
        .into_iter()
        .map(|(id, name, bio, image_url, _)| ArtistResponse {
            id,
            name,
            bio,
            image_url,
        })
        .collect();

    Ok(Json(json!({ "artists": artists, "total": total })))
}

pub async fn get_artist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let artist = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, name, bio, image_url FROM artists WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("artist not found".to_string()))?;

    let albums = sqlx::query_as::<_, (String, String, Option<i32>, Option<String>)>(
        "SELECT id, title, year, cover_art_path FROM albums WHERE artist_id = ? ORDER BY year, title",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let album_summaries: Vec<AlbumSummary> = albums
        .into_iter()
        .map(|(id, title, year, cover_art_path)| AlbumSummary {
            id,
            title,
            year,
            cover_art_path,
        })
        .collect();

    let is_favorited = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = 'artist' AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&id)
    .fetch_one(&state.db)
    .await
    .map(|(count,)| count > 0)
    .unwrap_or(false);

    Ok(Json(json!(ArtistDetailResponse {
        id: artist.0,
        name: artist.1,
        bio: artist.2,
        image_url: artist.3,
        albums: album_summaries,
        is_favorited,
    })))
}
