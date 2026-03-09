use axum::{extract::State, Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
use crate::AppState;

/// GET /playlists/ai/suggestions
pub async fn get_suggestions(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let suggestions =
        riff_core::smart_playlist::generate_suggestions(&state.db, &claims.sub).await?;

    let items: Vec<Value> = suggestions
        .into_iter()
        .map(|s| {
            json!({
                "label": s.label,
                "prompt": s.prompt,
            })
        })
        .collect();

    Ok(Json(json!({
        "suggestions": items,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveBody {
    pub title: String,
    pub description: Option<String>,
    pub track_ids: Vec<String>,
    pub library: Option<String>,
}

/// POST /playlists/ai/save
pub async fn save(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<SaveBody>,
) -> Result<Json<Value>, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::BadRequest("title is required".to_string()));
    }
    if body.track_ids.is_empty() {
        return Err(AppError::BadRequest("track_ids cannot be empty".to_string()));
    }

    let library_ids = riff_core::db::resolve_library_ids(&state.db, body.library.as_deref()).await?;
    let lib_ids: Vec<String> = super::helpers::decode_json_array(&library_ids);
    let playlist_library_id = lib_ids.first().cloned();

    let playlist_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO playlists (id, user_id, name, description, library_id) VALUES (?, ?, ?, ?, ?)")
        .bind(&playlist_id)
        .bind(&claims.sub)
        .bind(title)
        .bind(&body.description)
        .bind(&playlist_library_id)
        .execute(&state.db)
        .await?;

    for (i, track_id) in body.track_ids.iter().enumerate() {
        let pt_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
        )
        .bind(&pt_id)
        .bind(&playlist_id)
        .bind(track_id)
        .bind(i as i64)
        .execute(&state.db)
        .await?;
    }

    // Get total duration
    let total_duration = sqlx::query_as::<_, (i64,)>(
        "SELECT COALESCE(SUM(t.duration_seconds), 0)
         FROM playlist_tracks pt JOIN tracks t ON pt.track_id = t.id
         WHERE pt.playlist_id = ?",
    )
    .bind(&playlist_id)
    .fetch_one(&state.db)
    .await
    .map(|(d,)| d)
    .unwrap_or(0);

    Ok(Json(json!({
        "id": playlist_id,
        "name": title,
        "description": body.description,
        "trackCount": body.track_ids.len(),
        "totalDuration": total_duration,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateBody {
    pub prompt: String,
    pub title: Option<String>,
    pub genres: Option<Vec<String>>,
    pub energy: Option<i32>,
    pub era: Option<String>,
    pub moods: Option<Vec<String>>,
    pub track_count: Option<usize>,
    pub library: Option<String>,
}

/// POST /playlists/generate — Generate a playlist from a natural language prompt
pub async fn generate(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<GenerateBody>,
) -> Result<Json<Value>, AppError> {
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::BadRequest("prompt is required".to_string()));
    }

    let genres = body.genres.unwrap_or_default();
    let energy = body.energy.unwrap_or(3);
    let moods = body.moods.unwrap_or_default();
    tracing::info!(
        prompt = prompt,
        title = body.title.as_deref(),
        genres = ?genres,
        energy = energy,
        era = body.era.as_deref(),
        moods = ?moods,
        "smart playlist request"
    );

    let track_count = body.track_count.unwrap_or(25);
    let library_ids = riff_core::db::resolve_library_ids(&state.db, body.library.as_deref()).await?;
    let lib_ids: Vec<String> = super::helpers::decode_json_array(&library_ids);
    let playlist_library_id = lib_ids.first().cloned();

    let criteria = riff_core::smart_playlist::PlaylistCriteria {
        prompt: prompt.to_string(),
        genres,
        energy,
        era: body.era.clone(),
        moods,
    };

    let result = riff_core::smart_playlist::generate_playlist_from_prompt(
        &state.db,
        &criteria,
        track_count,
        &library_ids,
        &claims.sub,
    )
    .await?;

    // Use client-provided title (original user prompt) or fall back to auto-generated name
    let playlist_name = body
        .title
        .as_deref()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| {
            // Capitalize first letter
            let mut chars = t.chars();
            match chars.next() {
                None => t.to_string(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .unwrap_or(result.name.clone());
    let playlist_description = body
        .title
        .as_deref()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or(result.description.clone());

    // Save as a playlist
    let playlist_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO playlists (id, user_id, name, description, library_id) VALUES (?, ?, ?, ?, ?)")
        .bind(&playlist_id)
        .bind(&claims.sub)
        .bind(&playlist_name)
        .bind(&playlist_description)
        .bind(&playlist_library_id)
        .execute(&state.db)
        .await?;

    for (i, track_id) in result.track_ids.iter().enumerate() {
        let pt_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO playlist_tracks (id, playlist_id, track_id, sort_order) VALUES (?, ?, ?, ?)",
        )
        .bind(&pt_id)
        .bind(&playlist_id)
        .bind(track_id)
        .bind(i as i64)
        .execute(&state.db)
        .await?;
    }

    // Fetch full track details for response
    let ids_json = serde_json::to_string(&result.track_ids).unwrap_or_else(|_| "[]".to_string());
    let detail_rows = sqlx::query(
        "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                t.format, t.album_id, ar.name as artist_name,
                t.sample_rate, t.bit_depth,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.loudness_lufs, t.mood
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id IN (SELECT value FROM json_each(?))",
    )
    .bind(&ids_json)
    .fetch_all(&state.db)
    .await?;

    let detail_map: std::collections::HashMap<String, &sqlx::sqlite::SqliteRow> = detail_rows
        .iter()
        .map(|row| (row.get::<String, _>("id"), row))
        .collect();

    use sqlx::Row;
    let score_map: std::collections::HashMap<&str, f32> = result
        .track_ids
        .iter()
        .zip(result.scores.iter())
        .map(|(id, s)| (id.as_str(), *s))
        .collect();

    let track_list: Vec<Value> = result
        .track_ids
        .iter()
        .filter_map(|id| {
            detail_map.get(id.as_str()).map(|row| {
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
                    "bpm": row.get::<Option<f64>, _>("bpm"),
                    "musicalKey": row.get::<Option<String>, _>("musical_key"),
                    "loudnessLufs": row.get::<Option<f64>, _>("loudness_lufs"),
                    "mood": row.get::<Option<String>, _>("mood"),
                    "similarityScore": score_map.get(id.as_str()).copied(),
                })
            })
        })
        .collect();

    let total_duration: i32 = track_list
        .iter()
        .filter_map(|t| t["durationSeconds"].as_i64())
        .sum::<i64>() as i32;

    let (min_score, max_score) = result.scores.iter().copied().fold(
        (f32::MAX, f32::MIN),
        |(mn, mx), s| (mn.min(s), mx.max(s)),
    );

    Ok(Json(json!({
        "id": playlist_id,
        "name": playlist_name,
        "description": playlist_description,
        "trackCount": track_list.len(),
        "totalDuration": total_duration,
        "tracks": track_list,
        "scoreRange": {
            "min": min_score,
            "max": max_score,
        },
    })))
}
