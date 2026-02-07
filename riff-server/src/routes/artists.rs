use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
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
pub struct SimilarArtist {
    pub id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ArtistDetailResponse {
    pub id: String,
    pub name: String,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub albums: Vec<AlbumSummary>,
    pub is_favorited: bool,
    pub similar_artists: Vec<SimilarArtist>,
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
            "SELECT id, name, COALESCE(ai_bio, bio) as bio, image_url, COUNT(*) OVER() as total_count FROM artists WHERE name LIKE ? ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT id, name, COALESCE(ai_bio, bio) as bio, image_url, COUNT(*) OVER() as total_count FROM artists ORDER BY name LIMIT ? OFFSET ?",
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
    let detail = build_artist_detail(&state.db, &id, &claims.sub).await?;
    Ok(Json(json!(detail)))
}

pub async fn build_artist_detail(
    db: &SqlitePool,
    artist_id: &str,
    user_id: &str,
) -> Result<ArtistDetailResponse, AppError> {
    let artist = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, name, COALESCE(ai_bio, bio) as bio, image_url FROM artists WHERE id = ?",
    )
    .bind(artist_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound("artist not found".to_string()))?;

    // Run independent queries concurrently
    let (albums_result, fav_result, similar_result) = tokio::join!(
        sqlx::query_as::<_, (String, String, Option<i32>, Option<String>)>(
            "SELECT id, title, year, cover_art_path FROM albums WHERE artist_id = ? ORDER BY year, title",
        )
        .bind(artist_id)
        .fetch_all(db),
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = 'artist' AND entity_id = ?",
        )
        .bind(user_id)
        .bind(artist_id)
        .fetch_one(db),
        sqlx::query_as::<_, (String, String, Option<String>, String)>(
            "SELECT a.id, a.name, a.image_url, r.reason \
             FROM artist_recommendations r \
             JOIN artists a ON r.recommended_artist_id = a.id \
             WHERE r.artist_id = ? \
             ORDER BY r.sort_order"
        )
        .bind(artist_id)
        .fetch_all(db),
    );

    let albums = albums_result?;
    let is_favorited = fav_result.map(|(count,)| count > 0).unwrap_or(false);
    let similar_rows = similar_result?;

    let album_summaries: Vec<AlbumSummary> = albums
        .into_iter()
        .map(|(id, title, year, cover_art_path)| AlbumSummary {
            id,
            title,
            year,
            cover_art_path,
        })
        .collect();

    let similar_artists: Vec<SimilarArtist> = similar_rows
        .into_iter()
        .map(|(id, name, image_url, reason)| SimilarArtist {
            id,
            name,
            image_url,
            reason,
        })
        .collect();

    Ok(ArtistDetailResponse {
        id: artist.0,
        name: artist.1,
        bio: artist.2,
        image_url: artist.3,
        albums: album_summaries,
        is_favorited,
        similar_artists,
    })
}
