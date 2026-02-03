use axum::{extract::{Path, State}, Json};
use riff_core::{analysis, discogs, scanner};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::AppState;

pub async fn trigger_scan(State(state): State<Arc<AppState>>) -> Json<Value> {
    let library_path = match &state.config.library.path {
        Some(path) => path.clone(),
        None => return Json(json!({ "error": "no library path configured" })),
    };

    match scanner::scan_library(&state.db, &library_path).await {
        Ok(result) => {
            let enrichment_triggered = maybe_spawn_enrichment(&state);
            let analysis_triggered = if !enrichment_triggered {
                // No Discogs configured — spawn analysis directly after scan
                maybe_spawn_analysis(&state)
            } else {
                false // Analysis will be chained after enrichment completes
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
    if state.config.metadata.discogs.api_token.is_none() {
        return Json(json!({ "error": "no discogs api_token configured" }));
    }

    if maybe_spawn_enrichment(&state) {
        Json(json!({ "status": "started" }))
    } else {
        Json(json!({ "status": "already_running" }))
    }
}

/// Spawn background enrichment if Discogs is configured and no enrichment is already running.
/// Returns true if enrichment was started.
fn maybe_spawn_enrichment(state: &Arc<AppState>) -> bool {
    if state.config.metadata.discogs.api_token.is_none() {
        return false;
    }

    if state.enrichment_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!("enrichment already running, skipping");
        return false;
    }

    let enrich_state = state.clone();
    tokio::spawn(async move {
        match discogs::enrich_library(&enrich_state.db, &enrich_state.config.metadata.discogs).await {
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

        // Chain analysis after enrichment
        maybe_spawn_analysis(&enrich_state);
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

    match (artists, albums, tracks) {
        (Ok((artist_count,)), Ok((album_count,)), Ok((track_count, total_size, analyzed, pending_analysis))) => {
            Json(json!({
                "artists": artist_count,
                "albums": album_count,
                "tracks": track_count,
                "totalSize": total_size,
                "analyzed": analyzed,
                "pendingAnalysis": pending_analysis,
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
    if state.config.metadata.discogs.api_token.is_none() {
        return Json(json!({ "error": "no discogs api_token configured" }));
    }

    match discogs::enrich_album(&state.db, &state.config.metadata.discogs, &album_id).await {
        Ok(matched) => Json(json!({
            "status": "complete",
            "matched": matched,
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
