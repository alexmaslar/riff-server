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
use unicode_normalization::UnicodeNormalization;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub library: Option<String>,
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
    pub id: String,
    pub rank: i32,
    pub name: String,
    pub playcount: i64,
    pub track_id: String,
    pub title: String,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_seconds: Option<i32>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
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
    pub track_count: i64,
}

#[tracing::instrument(skip(state))]
pub async fn list_artists(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let rows = if let Some(ref search) = params.search {
        let pattern = format!("%{}%", search);
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT id, name, bio, image_url, COUNT(*) OVER() as total_count \
             FROM artists \
             WHERE name LIKE ? AND library_id IN (SELECT value FROM json_each(?)) \
             ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(&pattern)
        .bind(&library_ids)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT id, name, bio, image_url, COUNT(*) OVER() as total_count \
             FROM artists \
             WHERE library_id IN (SELECT value FROM json_each(?)) \
             ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(&library_ids)
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

#[tracing::instrument(skip(state, claims))]
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

#[derive(Debug, Deserialize)]
pub struct StoriesParams {
    pub library: Option<String>,
}

pub async fn artist_stories(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<StoriesParams>,
) -> Result<Json<Value>, AppError> {
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let rows = sqlx::query_as::<_, (String, String, Option<String>, bool)>(
        "WITH new_music AS ( \
            SELECT DISTINCT ar.id, ar.name, ar.image_url, 1 as has_new_music, 0 as play_count \
            FROM artists ar \
            JOIN albums al ON al.artist_id = ar.id \
            WHERE al.added_at >= datetime('now', '-7 days') \
              AND al.library_id IN (SELECT value FROM json_each(?1)) \
              AND ar.image_url IS NOT NULL \
        ), \
        top_played AS ( \
            SELECT ar.id, ar.name, ar.image_url, 0 as has_new_music, COUNT(*) as play_count \
            FROM play_history ph \
            JOIN tracks t ON ph.track_id = t.id \
            JOIN albums a ON t.album_id = a.id \
            JOIN artists ar ON a.artist_id = ar.id \
            WHERE ph.user_id = ?2 \
              AND a.library_id IN (SELECT value FROM json_each(?1)) \
              AND ar.image_url IS NOT NULL \
            GROUP BY ar.id \
            ORDER BY play_count DESC \
            LIMIT 20 \
        ), \
        combined AS ( \
            SELECT *, ROW_NUMBER() OVER (PARTITION BY id ORDER BY has_new_music DESC, play_count DESC) as rn \
            FROM (SELECT * FROM new_music UNION ALL SELECT * FROM top_played) \
        ) \
        SELECT id, name, image_url, has_new_music \
        FROM combined WHERE rn = 1 \
        ORDER BY has_new_music DESC, play_count DESC \
        LIMIT 10",
    )
    .bind(&library_ids)
    .bind(&claims.sub)
    .fetch_all(&state.db)
    .await?;

    let artists: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, image_url, has_new_music)| {
            json!({
                "id": id,
                "name": name,
                "image_url": image_url,
                "has_new_music": has_new_music,
            })
        })
        .collect();

    Ok(Json(json!({ "artists": artists })))
}

#[derive(Debug, Deserialize)]
pub struct StreamingAlbumsParams {
    pub provider: Option<String>,
}

pub async fn get_streaming_albums(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<StreamingAlbumsParams>,
) -> Result<Json<Value>, AppError> {
    let provider_name = params.provider.unwrap_or_else(|| "tidal".to_string());

    // Look up artist name from DB
    let artist_row = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM artists WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("artist not found".to_string()))?;
    let artist_name = artist_row.0;

    // Find the provider
    let registry = state.plugin_registry.read().await;
    let provider = registry
        .streaming_providers()
        .iter()
        .find(|p| p.provider_name() == provider_name)
        .cloned();
    drop(registry);

    let Some(provider) = provider else {
        return Err(AppError::NotFound(format!(
            "streaming provider '{}' not configured",
            provider_name
        )));
    };

    // Search for artist on the provider
    let search_results = provider
        .search(&artist_name, 5)
        .await
        .map_err(|e| AppError::Internal(format!("provider search failed: {e}")))?;

    // Find matching artist (case-insensitive exact match)
    let artist_name_lower = artist_name.to_lowercase();
    let streaming_artist = search_results
        .artists
        .into_iter()
        .find(|a| a.name.to_lowercase() == artist_name_lower);

    let Some(streaming_artist) = streaming_artist else {
        return Ok(Json(json!({
            "provider": provider_name,
            "albums": [],
        })));
    };

    // Get artist albums from provider
    let streaming_albums = provider
        .get_artist_albums(&streaming_artist.provider_id)
        .await
        .map_err(|e| AppError::Internal(format!("album fetch failed: {e}")))?;

    // Get library albums for this artist
    let library_albums = sqlx::query_as::<_, (String, String, Option<i32>)>(
        "SELECT id, title, year FROM albums WHERE artist_id = ?",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await?;

    // Normalize: NFKD decompose (splits accents from base chars), lowercase, ASCII alphanumeric only.
    // This ensures "DeBÍ" matches regardless of whether Í is NFC (U+00CD) or NFD (I + combining accent).
    fn normalize(s: &str) -> String {
        s.nfkd().collect::<String>().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
    }

    // Build merged album list
    let albums: Vec<Value> = streaming_albums
        .into_iter()
        .map(|album| {
            let match_result = library_albums.iter().find(|(_, lib_title, lib_year)| {
                let titles_match = normalize(&album.title) == normalize(lib_title);
                let years_match = match (album.year, *lib_year) {
                    (Some(sy), Some(ly)) => (sy - ly).abs() <= 1,
                    _ => true,
                };
                titles_match && years_match
            });

            let in_library = match_result.is_some();
            let library_album_id = match_result.map(|(lid, _, _)| lid.clone());
            let qualities: Vec<String> = album
                .available_qualities
                .iter()
                .filter_map(|q| serde_json::to_value(q).ok())
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();

            json!({
                "provider_id": album.provider_id,
                "title": album.title,
                "artist": {
                    "provider_id": album.artist.provider_id,
                    "name": album.artist.name,
                    "image_url": album.artist.image_url,
                },
                "year": album.year,
                "cover_url": album.cover_url,
                "track_count": album.track_count,
                "available_qualities": qualities,
                "in_library": in_library,
                "library_album_id": library_album_id,
                "album_type": album.album_type,
            })
        })
        .collect();

    Ok(Json(json!({
        "provider": provider_name,
        "albums": albums,
    })))
}

pub async fn build_artist_detail(
    db: &SqlitePool,
    artist_id: &str,
    user_id: &str,
) -> Result<ArtistDetailResponse, AppError> {
    let artist = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, name, bio, image_url FROM artists WHERE id = ?",
    )
    .bind(artist_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound("artist not found".to_string()))?;

    // Run independent queries concurrently
    let (albums_result, fav_result, similar_result, top_tracks_result) = tokio::join!(
        sqlx::query_as::<_, (String, String, Option<i32>, Option<String>, i64, i64)>(
            "SELECT a.id, a.title, a.year, a.cover_art_path, a.play_count, \
             (SELECT COUNT(*) FROM tracks t WHERE t.album_id = a.id) as track_count \
             FROM albums a WHERE a.artist_id = ? ORDER BY a.year, a.title",
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
             AND EXISTS ( \
                 SELECT 1 FROM albums src_al \
                 JOIN libraries src_lib ON src_al.library_id = src_lib.id \
                 JOIN albums rec_al ON rec_al.artist_id = a.id \
                 JOIN libraries rec_lib ON rec_al.library_id = rec_lib.id \
                 WHERE src_al.artist_id = r.artist_id \
                 AND (src_al.library_id = rec_al.library_id OR (src_lib.isolated = 0 AND rec_lib.isolated = 0)) \
             ) \
             ORDER BY r.sort_order"
        )
        .bind(artist_id)
        .fetch_all(db),
        sqlx::query_as::<_, (String, i32, i64, String, String, Option<i32>, Option<String>, Option<i32>, Option<i32>, Option<i32>, Option<i32>, String, String, Option<String>)>(
            "SELECT att.track_name, att.rank, att.playcount, \
                    t.id, t.title, t.duration_seconds, t.format, \
                    t.sample_rate, t.bit_depth, t.track_number, t.disc_number, \
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
    let top_tracks_rows = top_tracks_result?;

    let album_summaries: Vec<AlbumSummary> = albums
        .into_iter()
        .map(|(id, title, year, cover_art_path, play_count, track_count)| AlbumSummary {
            id,
            title,
            year,
            cover_art_path,
            play_count,
            track_count,
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
        .map(|(track_name, rank, playcount, track_id, title, duration_seconds, format, sample_rate, bit_depth, track_number, disc_number, album_id, album_title, cover_art_path)| {
            PopularTrack {
                id: track_id.clone(),
                rank,
                name: track_name,
                playcount,
                track_id,
                title,
                track_number,
                disc_number,
                duration_seconds,
                format,
                sample_rate,
                bit_depth,
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
