use axum::{
    extract::{Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use riff_core::daily_mixes::{
    load_lb_artist_scores, order_for_flow, score_and_select, ScoringContext,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

#[derive(Deserialize)]
pub struct AutoQueueParams {
    pub seed_track_id: String,
    pub artist_id: Option<String>,
    pub exclude: Option<String>,
    pub limit: Option<usize>,
    pub library: Option<String>,
}

/// GET /autoqueue — Get auto-queued tracks based on a seed track
pub async fn get_autoqueue(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<AutoQueueParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(15).clamp(1, 50);
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;
    let exclude_ids: Vec<String> = params
        .exclude
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    // Look up seed track
    let seed = sqlx::query(
        "SELECT t.id, a.artist_id, t.album_id
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         WHERE t.id = ?",
    )
    .bind(&params.seed_track_id)
    .fetch_optional(&state.db)
    .await?;

    let seed = match seed {
        Some(row) => row,
        None => return Err(AppError::NotFound("seed track not found".to_string())),
    };

    let seed_artist_id: String = seed.get("artist_id");

    // Determine context artist (explicit param or seed track's artist)
    let context_artist_id = params.artist_id.as_deref().unwrap_or(&seed_artist_id);

    // Get the context artist's genres/styles
    let artist_genres = sqlx::query(
        "SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.genre) j
         WHERE a.artist_id = ?
         UNION
         SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.style) j
         WHERE a.artist_id = ?",
    )
    .bind(context_artist_id)
    .bind(context_artist_id)
    .fetch_all(&state.db)
    .await?;

    let genres: Vec<String> = artist_genres.iter().map(|r| r.get("genre")).collect();

    // Fetch candidate tracks
    let candidate_tracks = if genres.is_empty() {
        // Fallback: all tracks, prioritize context artist
        let mut q = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.moods,
                    ar.external_id as artist_external_id, a.genre
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.id NOT IN (SELECT value FROM json_each(?))
               AND t.library_id IN (SELECT value FROM json_each(?))
             ORDER BY CASE WHEN a.artist_id = ? THEN 0 ELSE 1 END, rating DESC
             LIMIT 200",
        );
        let exclude_json = serde_json::to_string(&exclude_ids).unwrap_or_else(|_| "[]".to_string());
        q = q.bind(exclude_json);
        q = q.bind(&library_ids);
        q = q.bind(context_artist_id);
        q.fetch_all(&state.db).await?
    } else {
        // Build genre match condition with dynamic placeholders
        let genre_params: Vec<String> = genres.iter().enumerate().map(|(i, _)| format!("?{}", i + 4)).collect();
        let genre_list = genre_params.join(", ");

        let query = format!(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.moods,
                    ar.external_id as artist_external_id, a.genre
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.id NOT IN (SELECT value FROM json_each(?1))
               AND t.library_id IN (SELECT value FROM json_each(?2))
               AND a.id IN (
                 SELECT DISTINCT a2.id FROM albums a2, json_each(a2.genre) jg
                 WHERE jg.value IN ({genre_list})
                 UNION
                 SELECT DISTINCT a3.id FROM albums a3, json_each(a3.style) js
                 WHERE js.value IN ({genre_list})
               )
             ORDER BY CASE WHEN a.artist_id = ?3 THEN 0 ELSE 1 END, rating DESC
             LIMIT 200"
        );

        let exclude_json = serde_json::to_string(&exclude_ids).unwrap_or_else(|_| "[]".to_string());
        let mut q = sqlx::query(&query).bind(&exclude_json).bind(&library_ids).bind(context_artist_id);
        for genre in &genres {
            q = q.bind(genre);
        }
        q.fetch_all(&state.db).await?
    };

    if candidate_tracks.is_empty() {
        return Ok(Json(json!({ "tracks": [] })));
    }

    // Load LB similar artist scores for the context artist
    let lb_artist_scores = load_lb_artist_scores(&state.db, context_artist_id)
        .await
        .unwrap_or_default();
    let seed_genres_lower: Vec<String> = genres.iter().map(|g| g.to_lowercase()).collect();

    let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
    let ctx = ScoringContext {
        pool: &state.db,
        user_id: &claims.sub,
        date_str: &today,
        used_track_ids: &exclude_ids,
        max_tracks_per_artist: 3,
        seed_artist_id: None,
        lb_artist_scores: &lb_artist_scores,
        seed_genres: &seed_genres_lower,
    };

    let mut selected = score_and_select(&candidate_tracks, &ctx).await?;

    // Truncate to requested limit
    selected.truncate(limit);

    if selected.is_empty() {
        return Ok(Json(json!({ "tracks": [] })));
    }

    order_for_flow(&mut selected);

    // Look up full track details for the selected IDs
    let selected_ids: Vec<String> = selected.iter().map(|t| t.id.clone()).collect();
    let ids_json = serde_json::to_string(&selected_ids).unwrap_or_else(|_| "[]".to_string());

    let detail_rows = sqlx::query(
        "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                t.format, t.album_id, ar.name as artist_name,
                t.sample_rate, t.bit_depth, t.composer,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.loudness_lufs, t.mood, t.file_size_bytes, a.play_count as album_play_count
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id IN (SELECT value FROM json_each(?))",
    )
    .bind(&ids_json)
    .fetch_all(&state.db)
    .await?;

    // Build a map for ordering
    let detail_map: std::collections::HashMap<String, &sqlx::sqlite::SqliteRow> = detail_rows
        .iter()
        .map(|row| (row.get::<String, _>("id"), row))
        .collect();

    // Return tracks in flow order
    let track_list: Vec<Value> = selected
        .iter()
        .filter_map(|mix_track| {
            detail_map.get(&mix_track.id).map(|row| {
                json!({
                    "id": row.get::<String, _>("id"),
                    "title": row.get::<String, _>("title"),
                    "trackNumber": row.get::<i32, _>("track_number"),
                    "discNumber": row.get::<i32, _>("disc_number"),
                    "durationSeconds": row.get::<i32, _>("duration_seconds"),
                    "format": row.get::<String, _>("format"),
                    "albumId": row.get::<Option<String>, _>("album_id"),
                    "artistName": row.get::<Option<String>, _>("artist_name"),
                    "sampleRate": row.get::<Option<i32>, _>("sample_rate"),
                    "bitDepth": row.get::<Option<i32>, _>("bit_depth"),
                    "composer": row.get::<Option<String>, _>("composer"),
                    "bpm": row.get::<Option<f64>, _>("bpm"),
                    "musicalKey": row.get::<Option<String>, _>("musical_key"),
                    "loudnessLufs": row.get::<Option<f64>, _>("loudness_lufs"),
                    "mood": row.get::<Option<String>, _>("mood"),
                    "fileSizeBytes": row.get::<Option<i64>, _>("file_size_bytes"),
                    "albumPlayCount": row.get::<Option<i64>, _>("album_play_count"),
                })
            })
        })
        .collect();

    Ok(Json(json!({ "tracks": track_list })))
}
