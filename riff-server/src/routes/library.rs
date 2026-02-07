use axum::{extract::{Path, State}, Json};
use riff_core::{ai, analysis, discogs, scanner};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

pub async fn trigger_scan(State(state): State<Arc<AppState>>) -> Json<Value> {
    let library_path = match &state.config.read().await.library.path {
        Some(path) => path.clone(),
        None => return Json(json!({ "error": "no library path configured" })),
    };

    match scanner::scan_library(&state.db, &library_path).await {
        Ok(result) => {
            let enrichment_triggered = maybe_spawn_enrichment(&state).await;
            let analysis_triggered = if !enrichment_triggered {
                // No Discogs configured — spawn summarization (which chains analysis)
                maybe_spawn_summarization(&state).await
            } else {
                false // Summarization + analysis will be chained after enrichment completes
            };
            Json(json!({
                "status": "complete",
                "artists_added": result.artists_added,
                "albums_added": result.albums_added,
                "tracks_added": result.tracks_added,
                "errors": result.errors,
                "enrichment_triggered": enrichment_triggered,
                "analysis_triggered": analysis_triggered,
            }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn trigger_enrichment(State(state): State<Arc<AppState>>) -> Json<Value> {
    if state.config.read().await.metadata.discogs.api_token.is_none() {
        return Json(json!({ "error": "no discogs api_token configured" }));
    }

    if maybe_spawn_enrichment(&state).await {
        Json(json!({ "status": "started" }))
    } else {
        Json(json!({ "status": "already_running" }))
    }
}

/// Spawn background enrichment if Discogs is configured and no enrichment is already running.
/// Returns true if enrichment was started.
async fn maybe_spawn_enrichment(state: &Arc<AppState>) -> bool {
    let discogs_config = {
        let config = state.config.read().await;
        if config.metadata.discogs.api_token.is_none() {
            return false;
        }
        config.metadata.discogs.clone()
    };

    if state.enrichment_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("enrichment already running, skipping");
        return false;
    }

    let enrich_state = state.clone();
    tokio::spawn(async move {
        match discogs::enrich_library(&enrich_state.db, &discogs_config).await {
            Ok(result) => {
                tracing::info!(
                    "enrichment complete: {} albums, {} artists, {} covers",
                    result.albums_enriched,
                    result.artists_enriched,
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

pub async fn library_stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let artists: Result<(i64,), _> =
        sqlx::query_as("SELECT COUNT(*) FROM artists")
            .fetch_one(&state.db)
            .await;
    let albums: Result<(i64,), _> =
        sqlx::query_as("SELECT COUNT(*) FROM albums")
            .fetch_one(&state.db)
            .await;
    let tracks: Result<(i64, i64, i64, i64), _> =
        sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(file_size_bytes), 0), \
             SUM(CASE WHEN analysis_status = 'complete' THEN 1 ELSE 0 END), \
             SUM(CASE WHEN analysis_status = 'pending' THEN 1 ELSE 0 END) \
             FROM tracks"
        )
            .fetch_one(&state.db)
            .await;

    let summaries: Result<(i64, i64), _> =
        sqlx::query_as(
            "SELECT \
             SUM(CASE WHEN ai_summary IS NOT NULL THEN 1 ELSE 0 END), \
             SUM(CASE WHEN ai_summary IS NULL AND metadata_status = 'matched' THEN 1 ELSE 0 END) \
             FROM albums"
        )
            .fetch_one(&state.db)
            .await;

    let pending_ratings: Result<(i64,), _> =
        sqlx::query_as(
            "SELECT COUNT(*) FROM albums WHERE ai_summary IS NOT NULL AND ai_rating IS NULL"
        )
            .fetch_one(&state.db)
            .await;

    match (artists, albums, tracks, summaries, pending_ratings) {
        (Ok((artist_count,)), Ok((album_count,)), Ok((track_count, total_size, analyzed, pending_analysis)), Ok((summarized, pending_summaries)), Ok((pending_rate,))) => {
            Json(json!({
                "artists": artist_count,
                "albums": album_count,
                "tracks": track_count,
                "totalSize": total_size,
                "analyzed": analyzed,
                "pendingAnalysis": pending_analysis,
                "summarized": summarized,
                "pendingSummaries": pending_summaries,
                "pendingRatings": pending_rate,
            }))
        }
        _ => Json(json!({ "error": "failed to query library stats" })),
    }
}

pub async fn trigger_analysis(State(state): State<Arc<AppState>>) -> Json<Value> {
    if maybe_spawn_analysis(&state) {
        Json(json!({ "status": "started" }))
    } else {
        Json(json!({ "status": "already_running" }))
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
) -> Json<Value> {
    let discogs_config = {
        let config = state.config.read().await;
        if config.metadata.discogs.api_token.is_none() {
            return Json(json!({ "error": "no discogs api_token configured" }));
        }
        config.metadata.discogs.clone()
    };

    match discogs::enrich_album(&state.db, &discogs_config, &album_id).await {
        Ok(matched) => Json(json!({
            "status": "complete",
            "matched": matched,
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn trigger_summarization(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Json(json!({ "error": "AI summarization not enabled in config" }));
    }
    if !config.metadata.ai.album_summaries {
        return Json(json!({ "error": "Album summaries disabled in config" }));
    }
    drop(config);

    if maybe_spawn_summarization(&state).await {
        Json(json!({ "status": "started" }))
    } else {
        Json(json!({ "status": "already_running" }))
    }
}

pub async fn summarize_album(
    State(state): State<Arc<AppState>>,
    Path(album_id): Path<String>,
) -> Json<Value> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Json(json!({ "error": "AI summarization not enabled in config" }));
        }
        if !config.metadata.ai.album_summaries {
            return Json(json!({ "error": "Album summaries disabled in config" }));
        }
        config.metadata.ai.clone()
    };

    match ai::summarize_album(&state.db, &ai_config, &album_id).await {
        Ok(_) => Json(json!({ "status": "complete" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
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

pub async fn trigger_rating(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    if !config.metadata.ai.enabled {
        return Json(json!({ "error": "AI not enabled in config" }));
    }
    if !config.metadata.ai.album_ratings {
        return Json(json!({ "error": "Album ratings disabled in config" }));
    }
    drop(config);

    if maybe_spawn_rating(&state).await {
        Json(json!({ "status": "started" }))
    } else {
        Json(json!({ "status": "already_running" }))
    }
}

pub async fn rate_album(
    State(state): State<Arc<AppState>>,
    Path(album_id): Path<String>,
) -> Json<Value> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Json(json!({ "error": "AI not enabled in config" }));
        }
        if !config.metadata.ai.album_ratings {
            return Json(json!({ "error": "Album ratings disabled in config" }));
        }
        config.metadata.ai.clone()
    };

    match ai::rate_album(&state.db, &ai_config, &album_id).await {
        Ok(_) => Json(json!({ "status": "complete" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
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

pub async fn trigger_recommendations(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Json(json!({ "error": "AI not enabled in config" }));
        }
        if !config.metadata.ai.album_recommendations {
            return Json(json!({ "error": "Album recommendations disabled in config" }));
        }
        config.metadata.ai.clone()
    };

    if state.recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Json(json!({ "status": "already_running" }));
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

    Json(json!({ "status": "started" }))
}

pub async fn trigger_artist_recommendations(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Json(json!({ "error": "AI not enabled in config" }));
        }
        if !config.metadata.ai.artist_recommendations {
            return Json(json!({ "error": "Artist recommendations disabled in config" }));
        }
        config.metadata.ai.clone()
    };

    if state.artist_recommendation_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Json(json!({ "status": "already_running" }));
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

    Json(json!({ "status": "started" }))
}

pub async fn trigger_artist_bios(State(state): State<Arc<AppState>>) -> Json<Value> {
    let ai_config = {
        let config = state.config.read().await;
        if !config.metadata.ai.enabled {
            return Json(json!({ "error": "AI not enabled in config" }));
        }
        if !config.metadata.ai.artist_bios {
            return Json(json!({ "error": "Artist bios disabled in config" }));
        }
        config.metadata.ai.clone()
    };

    if state.artist_bio_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Json(json!({ "status": "already_running" }));
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

    Json(json!({ "status": "started" }))
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
