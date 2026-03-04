use axum::{extract::{Path, State}, Json};
use riff_core::{analysis, musicbrainz, scanner};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

pub async fn trigger_scan(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    // Guard: prevent overlap with periodic scanner
    if !state.stage_manager.try_start("scan") {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let result = async {
        let config = state.config.read().await;
        let resolved_libs = config.resolved_libraries();
        drop(config);

        if resolved_libs.is_empty() {
            return Err(AppError::BadRequest("no library path configured".into()));
        }

        let mut total_artists = 0u32;
        let mut total_albums = 0u32;
        let mut total_tracks = 0u32;
        let mut total_tracks_removed = 0u32;
        let mut total_albums_removed = 0u32;
        let mut total_artists_removed = 0u32;
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
                    total_tracks_removed += result.tracks_removed;
                    total_albums_removed += result.albums_removed;
                    total_artists_removed += result.artists_removed;
                    all_errors.extend(result.errors);
                }
                Err(e) => all_errors.push(format!("scan failed for {:?}: {}", lib_entry.name, e)),
            }
        }

        let pipeline_triggered = if total_albums > 0 || total_tracks > 0 || total_artists > 0 {
            state.pipeline_notify.notify_one();
            true
        } else {
            false
        };
        Ok(Json(json!({
            "status": "complete",
            "artists_added": total_artists,
            "albums_added": total_albums,
            "tracks_added": total_tracks,
            "tracks_removed": total_tracks_removed,
            "albums_removed": total_albums_removed,
            "artists_removed": total_artists_removed,
            "errors": all_errors,
            "pipeline_triggered": pipeline_triggered,
        })))
    }.await;

    state.stage_manager.finish("scan");
    result
}

pub async fn trigger_enrichment(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    state.pipeline_notify.notify_one();
    Ok(Json(json!({ "status": "started" })))
}

pub async fn trigger_editorial(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    if !state.stage_manager.try_start("editorial") {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let ed_state = state.clone();
    tokio::spawn(async move {
        let editorial_providers = ed_state.plugin_registry.read().await
            .editorial_providers().to_vec();
        match riff_core::editorial::enrich_library_editorial(&ed_state.db, &editorial_providers).await {
            Ok(result) => {
                tracing::info!(
                    "editorial enrichment complete: {} reviews added, {} errors",
                    result.reviews_added,
                    result.errors.len(),
                );
            }
            Err(e) => tracing::warn!("editorial enrichment failed: {}", e),
        }
        ed_state.stage_manager.finish("editorial");
    });

    Ok(Json(json!({ "status": "started" })))
}

pub async fn library_stats(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let (artist_result, album_result, track_result) = tokio::join!(
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM artists")
            .fetch_one(&state.db),
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM albums")
            .fetch_one(&state.db),
        sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT COUNT(*), COALESCE(SUM(file_size_bytes), 0), \
             SUM(CASE WHEN analysis_status = 'complete' THEN 1 ELSE 0 END), \
             SUM(CASE WHEN analysis_status = 'pending' THEN 1 ELSE 0 END) \
             FROM tracks"
        )
            .fetch_one(&state.db),
    );

    let (artist_count,) = artist_result?;
    let (album_count,) = album_result?;
    let (track_count, total_size, analyzed, pending_analysis) = track_result?;

    Ok(Json(json!({
        "artists": artist_count,
        "albums": album_count,
        "tracks": track_count,
        "totalSize": total_size,
        "analyzed": analyzed,
        "pendingAnalysis": pending_analysis,
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
    if !state.stage_manager.try_start("analysis") {
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
        analysis_state.stage_manager.finish("analysis");
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


pub async fn trigger_recommendations(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    if !state.stage_manager.try_start("recommendation") {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match riff_core::recommendations::generate_recommendations_force(&rec_state.db).await {
            Ok(result) => {
                tracing::info!(
                    "recommendations complete: {} albums, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("recommendations failed: {}", e),
        }
        rec_state.stage_manager.finish("recommendation");
    });

    Ok(Json(json!({ "status": "started" })))
}

pub async fn trigger_artist_recommendations(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    if !state.stage_manager.try_start("artist_recommendation") {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let rec_state = state.clone();
    tokio::spawn(async move {
        match riff_core::recommendations::generate_artist_recommendations_force(&rec_state.db).await {
            Ok(result) => {
                tracing::info!(
                    "artist recommendations complete: {} artists, {} recommendations",
                    result.albums_processed,
                    result.recommendations_generated,
                );
            }
            Err(e) => tracing::warn!("artist recommendations failed: {}", e),
        }
        rec_state.stage_manager.finish("artist_recommendation");
    });

    Ok(Json(json!({ "status": "started" })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearDataRequest {
    pub album_recommendations: Option<bool>,
    pub artist_recommendations: Option<bool>,
    pub editorial_data: Option<bool>,
}

pub async fn clear_data(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ClearDataRequest>,
) -> Result<Json<Value>, AppError> {
    tracing::info!("Clear data requested");
    let mut cleared = serde_json::Map::new();

    if body.album_recommendations == Some(true) {
        let result = sqlx::query("DELETE FROM album_recommendations")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} album recommendations", result.rows_affected());
        cleared.insert("album_recommendations".into(), json!(result.rows_affected()));
    }

    if body.artist_recommendations == Some(true) {
        let result = sqlx::query("DELETE FROM artist_recommendations")
            .execute(&state.db)
            .await?;
        tracing::info!("Cleared {} artist recommendations", result.rows_affected());
        cleared.insert("artist_recommendations".into(), json!(result.rows_affected()));
    }

    if body.editorial_data == Some(true) {
        let summaries = sqlx::query(
            "UPDATE albums SET rating = NULL, \
             moods = '[]', descriptors = '[]', keywords = '[]', \
             rating_sources = '[]' \
             WHERE rating IS NOT NULL OR moods != '[]'"
        )
            .execute(&state.db)
            .await?;
        let reviews = sqlx::query("DELETE FROM editorial_reviews")
            .execute(&state.db)
            .await?;
        let total = summaries.rows_affected() + reviews.rows_affected();
        tracing::info!("Cleared editorial data: {} albums, {} reviews",
            summaries.rows_affected(), reviews.rows_affected());
        cleared.insert("editorial_data".into(), json!(total));
    }

    Ok(Json(json!({ "cleared": cleared })))
}
