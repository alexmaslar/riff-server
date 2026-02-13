use axum::{extract::{Path, State}, Json};
use riff_core::{ai, analysis, musicbrainz, scanner};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

pub async fn trigger_scan(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    let resolved_libs = config.resolved_libraries();
    drop(config);

    if resolved_libs.is_empty() {
        return Err(AppError::BadRequest("no library path configured".into()));
    }

    let mut total_artists = 0u32;
    let mut total_albums = 0u32;
    let mut total_tracks = 0u32;
    let mut all_errors = Vec::new();

    for lib_entry in &resolved_libs {
        let library_id = riff_core::db::get_library_id_by_path(&state.db, &lib_entry.path)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?
            .ok_or_else(|| AppError::Internal(format!("no library_id for path {:?}", lib_entry.path)))?;

        match scanner::scan_library(&state.db, &lib_entry.path, &library_id).await {
            Ok(result) => {
                total_artists += result.artists_added;
                total_albums += result.albums_added;
                total_tracks += result.tracks_added;
                all_errors.extend(result.errors);
            }
            Err(e) => all_errors.push(format!("scan failed for {:?}: {}", lib_entry.name, e)),
        }
    }

    let enrichment_triggered = maybe_spawn_enrichment(&state).await;
    let analysis_triggered = if !enrichment_triggered {
        maybe_spawn_summarization(&state).await
    } else {
        false
    };
    Ok(Json(json!({
        "status": "complete",
        "artists_added": total_artists,
        "albums_added": total_albums,
        "tracks_added": total_tracks,
        "errors": all_errors,
        "enrichment_triggered": enrichment_triggered,
        "analysis_triggered": analysis_triggered,
    })))
}

pub async fn trigger_enrichment(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    if maybe_spawn_enrichment(&state).await {
        Ok(Json(json!({ "status": "started" })))
    } else {
        Ok(Json(json!({ "status": "already_running" })))
    }
}

/// Spawn background enrichment if not already running. Returns true if started.
async fn maybe_spawn_enrichment(state: &Arc<AppState>) -> bool {
    if state.enrichment_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("enrichment already running, skipping");
        return false;
    }

    let enrich_state = state.clone();
    tokio::spawn(async move {
        match musicbrainz::enrich_library(&enrich_state.db).await {
            Ok(result) => {
                tracing::info!(
                    "enrichment complete: {} albums, {} covers",
                    result.albums_enriched,
                    result.covers_downloaded,
                );
            }
            Err(e) => tracing::warn!("enrichment failed: {}", e),
        }
        enrich_state.enrichment_running.store(false, Ordering::SeqCst);

        // Chain summarization after enrichment
        maybe_spawn_summarization(&enrich_state).await;
    });

    true
}

pub async fn library_stats(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let (artist_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM artists")
            .fetch_one(&state.db)
            .await?;
    let (album_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM albums")
            .fetch_one(&state.db)
            .await?;
    let (track_count, total_size, analyzed, pending_analysis): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(file_size_bytes), 0), \
             SUM(CASE WHEN analysis_status = 'complete' THEN 1 ELSE 0 END), \
             SUM(CASE WHEN analysis_status = 'pending' THEN 1 ELSE 0 END) \
             FROM tracks"
        )
            .fetch_one(&state.db)
            .await?;

    let (summarized, pending_summaries): (i64, i64) =
        sqlx::query_as(
            "SELECT \
             SUM(CASE WHEN ai_summary IS NOT NULL THEN 1 ELSE 0 END), \
             SUM(CASE WHEN ai_summary IS NULL AND metadata_status = 'matched' THEN 1 ELSE 0 END) \
             FROM albums"
        )
            .fetch_one(&state.db)
            .await?;

    let (pending_rate,): (i64,) =
        sqlx::query_as(
            "SELECT COUNT(*) FROM albums WHERE ai_summary IS NOT NULL AND ai_rating IS NULL"
        )
            .fetch_one(&state.db)
            .await?;

    Ok(Json(json!({
        "artists": artist_count,
        "albums": album_count,
        "tracks": track_count,
        "totalSize": total_size,
        "analyzed": analyzed,
        "pendingAnalysis": pending_analysis,
        "summarized": summarized,
        "pendingSummaries": pending_summaries,
        "pendingRatings": pending_rate,
    })))
}

pub async fn trigger_analysis(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    if maybe_spawn_analysis(&state) {
        Ok(Json(json!({ "status": "started" })))
    } else {
        Ok(Json(json!({ "status": "already_running" })))
    }
}

/// Spawn background analysis if not already running. Returns true if analysis was started.
fn maybe_spawn_analysis(state: &Arc<AppState>) -> bool {
    if state.analysis_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("analysis already running, skipping");
        return false;
    }

    let analysis_state = state.clone();
    tokio::spawn(async move {
        match analysis::analyze_library(&analysis_state.db).await {
            Ok(result) => {
                tracing::info!(
                    "analysis complete: {} analyzed, {} failed, {} skipped",
                    result.tracks_analyzed,
                    result.tracks_failed,
                    result.tracks_skipped,
                );
            }
            Err(e) => tracing::warn!("analysis failed: {}", e),
        }
        analysis_state.analysis_running.store(false, Ordering::SeqCst);
    });

    true
}

pub async fn enrich_album(
    State(state): State<Arc<AppState>>,
    Path(album_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let matched = musicbrainz::enrich_album(&state.db, &album_id).await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "status": "complete",
        "matched": matched,
    })))
}

pub async fn trigger_summarization(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Err(AppError::BadRequest("AI summarization not enabled in config".into()));
    }
    if !config.metadata.ai.album_summaries {
        return Err(AppError::BadRequest("Album summaries disabled in config".into()));
    }
    drop(config);

    if maybe_spawn_summarization(&state).await {
        Ok(Json(json!({ "status": "started" })))
    } else {
        Ok(Json(json!({ "status": "already_running" })))
    }
}

pub async fn summarize_album(
    State(state): State<Arc<AppState>>,
    Path(album_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Err(AppError::BadRequest("AI summarization not enabled in config".into()));
        }
        if !config.metadata.ai.album_summaries {
            return Err(AppError::BadRequest("Album summaries disabled in config".into()));
        }
        config.metadata.ai.clone()
    };

    ai::summarize_album(&state.db, &ai_config, &album_id).await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(json!({ "status": "complete" })))
}

pub(crate) async fn maybe_spawn_summarization(state: &Arc<AppState>) -> bool {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return false;
        }
        if !config.metadata.ai.album_summaries {
            // Skip to next in chain
            return maybe_spawn_rating(state).await;
        }
        config.metadata.ai.clone()
    };

    if state.summarization_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("summarization already running, skipping");
        return false;
    }

    let sum_state = state.clone();
    tokio::spawn(async move {
        match ai::summarize_library(&sum_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "summarization complete: {} summarized, {} errors",
                    result.albums_summarized,
                    result.errors.len(),
                );
            }
            Err(e) => tracing::warn!("summarization failed: {}", e),
        }
        sum_state.summarization_running.store(false, Ordering::SeqCst);

        // Chain rating after summarization, then analysis
        maybe_spawn_rating(&sum_state).await;
    });

    true
}

pub async fn trigger_rating(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Err(AppError::BadRequest("AI not enabled in config".into()));
    }
    if !config.metadata.ai.album_ratings {
        return Err(AppError::BadRequest("Album ratings disabled in config".into()));
    }
    drop(config);

    if maybe_spawn_rating(&state).await {
        Ok(Json(json!({ "status": "started" })))
    } else {
        Ok(Json(json!({ "status": "already_running" })))
    }
}

pub async fn rate_album(
    State(state): State<Arc<AppState>>,
    Path(album_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Err(AppError::BadRequest("AI not enabled in config".into()));
        }
        if !config.metadata.ai.album_ratings {
            return Err(AppError::BadRequest("Album ratings disabled in config".into()));
        }
        config.metadata.ai.clone()
    };

    ai::rate_album(&state.db, &ai_config, &album_id).await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Json(json!({ "status": "complete" })))
}

async fn maybe_spawn_rating(state: &Arc<AppState>) -> bool {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return false;
        }
        if !config.metadata.ai.album_ratings {
            // Skip to next in chain
            return maybe_spawn_recommendations(state).await;
        }
        config.metadata.ai.clone()
    };

    if state.rating_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("rating already running, skipping");
        return false;
    }

    let rate_state = state.clone();
    tokio::spawn(async move {
        match ai::rate_library(&rate_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "rating complete: {} rated, {} errors",
                    result.albums_rated,
                    result.errors.len(),
                );
            }
            Err(e) => tracing::warn!("rating failed: {}", e),
        }
        rate_state.rating_running.store(false, Ordering::SeqCst);

        // Chain recommendations after rating, then analysis
        maybe_spawn_recommendations(&rate_state).await;
    });

    true
}

pub async fn trigger_recommendations(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Err(AppError::BadRequest("AI not enabled in config".into()));
        }
        if !config.metadata.ai.album_recommendations {
            return Err(AppError::BadRequest("Album recommendations disabled in config".into()));
        }
        config.metadata.ai.clone()
    };

    if state.recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match ai::recommend_library_force(&rec_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "recommendations complete: {} albums, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("recommendations failed: {}", e),
        }
        rec_state.recommendation_running.store(false, Ordering::SeqCst);
    });

    Ok(Json(json!({ "status": "started" })))
}

pub async fn trigger_artist_recommendations(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Err(AppError::BadRequest("AI not enabled in config".into()));
        }
        if !config.metadata.ai.artist_recommendations {
            return Err(AppError::BadRequest("Artist recommendations disabled in config".into()));
        }
        config.metadata.ai.clone()
    };

    if state.artist_recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match ai::recommend_artists_force(&rec_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "artist recommendations complete: {} artists, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("artist recommendations failed: {}", e),
        }
        rec_state.artist_recommendation_running.store(false, Ordering::SeqCst);
    });

    Ok(Json(json!({ "status": "started" })))
}

pub async fn trigger_artist_bios(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Err(AppError::BadRequest("AI not enabled in config".into()));
        }
        if !config.metadata.ai.artist_bios {
            return Err(AppError::BadRequest("Artist bios disabled in config".into()));
        }
        config.metadata.ai.clone()
    };

    if state.artist_bio_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let bio_state = state.clone();
    tokio::spawn(async move {
        match ai::bio_artists(&bio_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "artist bios complete: {} processed, {} errors",
                    result.artists_processed,
                    result.errors.len(),
                );
            }
            Err(e) => tracing::warn!("artist bios failed: {}", e),
        }
        bio_state.artist_bio_running.store(false, Ordering::SeqCst);
    });

    Ok(Json(json!({ "status": "started" })))
}

async fn maybe_spawn_recommendations(state: &Arc<AppState>) -> bool {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return false;
        }
        if !config.metadata.ai.album_recommendations {
            // Skip to next in chain
            return maybe_spawn_artist_recommendations(state).await;
        }
        config.metadata.ai.clone()
    };

    if state.recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("recommendations already running, skipping");
        return false;
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match ai::recommend_library(&rec_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "recommendations complete: {} albums, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("recommendations failed: {}", e),
        }
        rec_state.recommendation_running.store(false, Ordering::SeqCst);

        // Chain artist recommendations after album recommendations
        maybe_spawn_artist_recommendations(&rec_state).await;
    });

    true
}

async fn maybe_spawn_artist_recommendations(state: &Arc<AppState>) -> bool {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return false;
        }
        if !config.metadata.ai.artist_recommendations {
            // Skip to analysis (end of AI chain)
            maybe_spawn_analysis(state);
            return false;
        }
        config.metadata.ai.clone()
    };

    if state.artist_recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("artist recommendations already running, skipping");
        return false;
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match ai::recommend_artists(&rec_state.db, &ai_config).await {
            Ok(result) => {
                tracing::info!(
                    "artist recommendations complete: {} artists, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("artist recommendations failed: {}", e),
        }
        rec_state.artist_recommendation_running.store(false, Ordering::SeqCst);

        // Chain analysis after artist recommendations
        maybe_spawn_analysis(&rec_state);
    });

    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearAiDataRequest {
    pub album_summaries: Option<bool>,
    pub album_ratings: Option<bool>,
    pub album_recommendations: Option<bool>,
    pub artist_bios: Option<bool>,
    pub artist_recommendations: Option<bool>,
}

pub async fn clear_ai_data(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ClearAiDataRequest>,
) -> Result<Json<Value>, AppError> {
    tracing::info!("Clear AI data requested");
    let mut cleared = serde_json::Map::new();

    if body.album_summaries == Some(true) {
        let result = sqlx::query("UPDATE albums SET ai_summary = NULL WHERE ai_summary IS NOT NULL")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} album summaries", result.rows_affected());
        cleared.insert("album_summaries".into(), json!(result.rows_affected()));
    }

    if body.album_ratings == Some(true) {
        let result = sqlx::query("UPDATE albums SET ai_rating = NULL WHERE ai_rating IS NOT NULL")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} album ratings", result.rows_affected());
        cleared.insert("album_ratings".into(), json!(result.rows_affected()));
    }

    if body.album_recommendations == Some(true) {
        let result = sqlx::query("DELETE FROM album_recommendations")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} album recommendations", result.rows_affected());
        cleared.insert("album_recommendations".into(), json!(result.rows_affected()));
    }

    if body.artist_bios == Some(true) {
        let result = sqlx::query("UPDATE artists SET ai_bio = NULL WHERE ai_bio IS NOT NULL")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} artist bios", result.rows_affected());
        cleared.insert("artist_bios".into(), json!(result.rows_affected()));
    }

    if body.artist_recommendations == Some(true) {
        let result = sqlx::query("DELETE FROM artist_recommendations")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} artist recommendations", result.rows_affected());
        cleared.insert("artist_recommendations".into(), json!(result.rows_affected()));
    }

    Ok(Json(json!({ "cleared": cleared })))
}
