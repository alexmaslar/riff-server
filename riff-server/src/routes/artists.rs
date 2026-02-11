use axum::{
    extract::{Path, Query, State},
    http::header,
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
pub struct PopularTrack {
    pub rank: i32,
    pub name: String,
    pub playcount: i64,
    pub track_id: String,
    pub title: String,
    pub duration_seconds: Option<i32>,
    pub format: Option<String>,
    pub album_id: String,
    pub album_title: String,
    pub cover_art_path: Option<String>,
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
    pub popular_tracks: Vec<PopularTrack>,
}

#[derive(Debug, Serialize)]
pub struct AlbumSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub cover_art_path: Option<String>,
    pub play_count: i64,
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
) -> Result<([(header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let detail = build_artist_detail(&state.db, &id, &claims.sub).await?;
    Ok((
        [(header::CACHE_CONTROL, "private, max-age=3600")],
        Json(json!(detail)),
    ))
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
    let (albums_result, fav_result, similar_result, top_tracks_result) = tokio::join!(
        sqlx::query_as::<_, (String, String, Option<i32>, Option<String>, i64)>(
            "SELECT id, title, year, cover_art_path, play_count FROM albums WHERE artist_id = ? ORDER BY year, title",
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
        sqlx::query_as::<_, (String, i32, i64, String, String, Option<i32>, Option<String>, String, String, Option<String>)>(
            "SELECT att.track_name, att.rank, att.playcount, \
                    t.id, t.title, t.duration_seconds, t.format, \
                    al.id, al.title, al.cover_art_path \
             FROM artist_top_tracks att \
             JOIN albums al ON al.artist_id = att.artist_id \
             JOIN tracks t ON t.album_id = al.id \
                 AND LOWER(TRIM(t.title)) = LOWER(TRIM(att.track_name)) \
             WHERE att.artist_id = ? AND att.track_name != '' \
             ORDER BY att.rank \
             LIMIT 10"
        )
        .bind(artist_id)
        .fetch_all(db),
    );

    let albums = albums_result?;
    let is_favorited = fav_result.map(|(count,)| count > 0).unwrap_or(false);
    let similar_rows = similar_result?;
    let top_tracks_rows = top_tracks_result.unwrap_or_default();

    let album_summaries: Vec<AlbumSummary> = albums
        .into_iter()
        .map(|(id, title, year, cover_art_path, play_count)| AlbumSummary {
            id,
            title,
            year,
            cover_art_path,
            play_count,
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

    let popular_tracks: Vec<PopularTrack> = top_tracks_rows
        .into_iter()
        .map(|(track_name, rank, playcount, track_id, title, duration_seconds, format, album_id, album_title, cover_art_path)| {
            PopularTrack {
                rank,
                name: track_name,
                playcount,
                track_id,
                title,
                duration_seconds,
                format,
                album_id,
                album_title,
                cover_art_path,
            }
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
        popular_tracks,
    })
}
