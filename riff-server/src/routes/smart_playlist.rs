use axum::{extract::State, Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use riff_core::config::AiProvider;

use crate::error::AppError;
use crate::AppState;

#[derive(Deserialize)]
pub struct GenerateBody {
    pub prompt: String,
    pub track_count: Option<u32>,
}

#[derive(Deserialize)]
pub struct RefineBody {
    pub prompt: String,
    pub current_track_ids: Vec<String>,
    pub track_count: Option<u32>,
}

#[derive(Deserialize)]
pub struct SaveBody {
    pub title: String,
    pub description: Option<String>,
    pub track_ids: Vec<String>,
}

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

    let config = state.config.read().await;
    let estimated_cost =
        riff_core::smart_playlist::estimate_generation_cost(&config.metadata.ai);
    let provider = match config.metadata.ai.provider {
        AiProvider::Ollama => "ollama",
        AiProvider::OpenAi => "openai",
        AiProvider::Anthropic => "anthropic",
    };

    Ok(Json(json!({
        "suggestions": items,
        "estimatedCost": estimated_cost,
        "provider": provider,
    })))
}

/// POST /playlists/ai/generate
pub async fn generate(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<GenerateBody>,
) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Err(AppError::BadRequest("AI is not enabled".to_string()));
    }
    let ai_config = config.metadata.ai.clone();
    drop(config);

    // Rate limit
    if let Err(msg) = riff_core::smart_playlist::check_rate_limit(&claims.sub).await {
        return Err(AppError::BadRequest(msg));
    }

    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::BadRequest("prompt is required".to_string()));
    }

    let result = riff_core::smart_playlist::generate_smart_playlist(
        &state.db,
        &ai_config,
        prompt,
        body.track_count,
    )
    .await?;

    let tracks: Vec<Value> = result
        .tracks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "title": t.title,
                "trackNumber": t.track_number,
                "discNumber": t.disc_number,
                "durationSeconds": t.duration_seconds,
                "format": t.format,
                "albumId": t.album_id,
                "artistName": t.artist_name,
                "sampleRate": t.sample_rate,
                "bitDepth": t.bit_depth,
                "composer": t.composer,
                "bpm": t.bpm,
                "musicalKey": t.musical_key,
                "loudnessLufs": t.loudness_lufs,
                "mood": t.mood,
            })
        })
        .collect();

    Ok(Json(json!({
        "title": result.title,
        "description": result.description,
        "tracks": tracks,
        "candidateCount": result.candidate_count,
    })))
}

/// POST /playlists/ai/refine
pub async fn refine(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<RefineBody>,
) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Err(AppError::BadRequest("AI is not enabled".to_string()));
    }
    let ai_config = config.metadata.ai.clone();
    drop(config);

    // Rate limit
    if let Err(msg) = riff_core::smart_playlist::check_rate_limit(&claims.sub).await {
        return Err(AppError::BadRequest(msg));
    }

    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::BadRequest("prompt is required".to_string()));
    }

    let result = riff_core::smart_playlist::refine_smart_playlist(
        &state.db,
        &ai_config,
        prompt,
        &body.current_track_ids,
        body.track_count,
    )
    .await?;

    let tracks: Vec<Value> = result
        .tracks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "title": t.title,
                "trackNumber": t.track_number,
                "discNumber": t.disc_number,
                "durationSeconds": t.duration_seconds,
                "format": t.format,
                "albumId": t.album_id,
                "artistName": t.artist_name,
                "sampleRate": t.sample_rate,
                "bitDepth": t.bit_depth,
                "composer": t.composer,
                "bpm": t.bpm,
                "musicalKey": t.musical_key,
                "loudnessLufs": t.loudness_lufs,
                "mood": t.mood,
            })
        })
        .collect();

    Ok(Json(json!({
        "title": result.title,
        "description": result.description,
        "tracks": tracks,
        "candidateCount": result.candidate_count,
    })))
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

    let playlist_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO playlists (id, user_id, name, description) VALUES (?, ?, ?, ?)")
        .bind(&playlist_id)
        .bind(&claims.sub)
        .bind(title)
        .bind(&body.description)
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
