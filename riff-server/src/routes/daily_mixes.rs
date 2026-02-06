use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use riff_core::auth::Claims;
use riff_core::daily_mixes;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;
use tokio::fs::File;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

/// GET /mixes/daily — Get today's mixes for the authenticated user
pub async fn list_daily_mixes(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let today = chrono::Utc::now().date_naive();
    let today_str = today.format("%Y-%m-%d").to_string();

    // Generate mixes on-demand if none exist for today
    let count = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM daily_mixes WHERE user_id = ? AND mix_date = ?",
    )
    .bind(&claims.sub)
    .bind(&today_str)
    .fetch_one(&state.db)
    .await?;

    if count.0 == 0 {
        if let Err(e) = daily_mixes::generate_daily_mixes(&state.db, &claims.sub, today).await {
            tracing::warn!("on-demand daily mix generation failed for {}: {e}", claims.sub);
        }
    }

    let rows = sqlx::query(
        "SELECT dm.id, dm.mix_type, dm.title, dm.description, dm.seed_value,
                COUNT(dmt.track_id) as track_count,
                COALESCE(SUM(t.duration_seconds), 0) as total_duration
         FROM daily_mixes dm
         LEFT JOIN daily_mix_tracks dmt ON dm.id = dmt.mix_id
         LEFT JOIN tracks t ON dmt.track_id = t.id
         WHERE dm.user_id = ? AND dm.mix_date = ?
         GROUP BY dm.id
         ORDER BY CASE dm.mix_type
             WHEN 'artist' THEN 1
             WHEN 'genre' THEN 2
             WHEN 'deep_cuts' THEN 3
             WHEN 'decade' THEN 4
         END",
    )
    .bind(&claims.sub)
    .bind(&today_str)
    .fetch_all(&state.db)
    .await?;

    let mut mixes: Vec<Value> = Vec::new();

    for row in &rows {
        let mix_id: String = row.get("id");
        let seed_value: Option<String> = row.get("seed_value");

        // Extract seed artist image if this is an artist mix
        let seed_artist_image_url = if let Some(ref sv) = seed_value {
            if let Some(artist_id) = sv.strip_prefix("artist:") {
                sqlx::query_as::<_, (Option<String>,)>(
                    "SELECT image_url FROM artists WHERE id = ?",
                )
                .bind(artist_id)
                .fetch_optional(&state.db)
                .await?
                .and_then(|r| r.0)
            } else {
                None
            }
        } else {
            None
        };

        mixes.push(json!({
            "id": mix_id,
            "mixType": row.get::<String, _>("mix_type"),
            "title": row.get::<String, _>("title"),
            "description": row.get::<Option<String>, _>("description"),
            "trackCount": row.get::<i64, _>("track_count"),
            "totalDuration": row.get::<i64, _>("total_duration"),
            "seedArtistImageUrl": seed_artist_image_url,
        }));
    }

    Ok(Json(json!({
        "date": today_str,
        "mixes": mixes,
    })))
}

/// GET /mixes/daily/:id — Get a specific mix with full track list
pub async fn get_daily_mix(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let mix = sqlx::query(
        "SELECT id, mix_type, title, description, seed_value, mix_date
         FROM daily_mixes WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(&claims.sub)
    .fetch_optional(&state.db)
    .await?;

    let mix = match mix {
        Some(row) => row,
        None => return Err(AppError::NotFound("mix not found".to_string())),
    };

    let tracks = sqlx::query(
        "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                t.format, t.album_id, ar.name as artist_name,
                t.sample_rate, t.bit_depth, t.composer,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.loudness_lufs, t.mood
         FROM daily_mix_tracks dmt
         JOIN tracks t ON dmt.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE dmt.mix_id = ?
         ORDER BY dmt.sort_order",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await?;

    let track_list: Vec<Value> = tracks
        .iter()
        .map(|row| {
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
            })
        })
        .collect();

    Ok(Json(json!({
        "id": mix.get::<String, _>("id"),
        "mixType": mix.get::<String, _>("mix_type"),
        "title": mix.get::<String, _>("title"),
        "description": mix.get::<Option<String>, _>("description"),
        "tracks": track_list,
    })))
}

/// POST /mixes/daily/:id/save — Save a daily mix as a permanent playlist
pub async fn save_mix_as_playlist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Verify the mix belongs to this user
    let mix = sqlx::query(
        "SELECT id, title, description FROM daily_mixes WHERE id = ? AND user_id = ?",
    )
    .bind(&id)
    .bind(&claims.sub)
    .fetch_optional(&state.db)
    .await?;

    let mix = match mix {
        Some(row) => row,
        None => return Err(AppError::NotFound("mix not found".to_string())),
    };

    let mix_title: String = mix.get("title");
    let mix_description: Option<String> = mix.get("description");

    // Create a new playlist
    let playlist_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO playlists (id, user_id, name, description) VALUES (?, ?, ?, ?)",
    )
    .bind(&playlist_id)
    .bind(&claims.sub)
    .bind(&mix_title)
    .bind(&mix_description)
    .execute(&state.db)
    .await?;

    // Copy tracks from the mix to the playlist
    let mix_tracks = sqlx::query_as::<_, (String, i64)>(
        "SELECT track_id, sort_order FROM daily_mix_tracks WHERE mix_id = ? ORDER BY sort_order",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await?;

    for (track_id, sort_order) in &mix_tracks {
        let pt_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
        )
        .bind(&pt_id)
        .bind(&playlist_id)
        .bind(track_id)
        .bind(sort_order)
        .execute(&state.db)
        .await?;
    }

    Ok(Json(json!({
        "id": playlist_id,
        "name": mix_title,
        "description": mix_description,
        "trackCount": mix_tracks.len(),
    })))
}

/// GET /mixes/daily/:id/cover — Public (no auth, like album covers)
pub async fn get_mix_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT cover_path FROM daily_mixes WHERE id = ?",
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
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "mix not found" })))
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
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}
