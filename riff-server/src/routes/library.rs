use axum::{extract::State, Json};
use riff_core::{discogs, scanner};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::AppState;

pub async fn trigger_scan(State(state): State<Arc<AppState>>) -> Json<Value> {
    let library_path = match &state.config.library.path {
        Some(path) => path.clone(),
        None => return Json(json!({ "error": "no library path configured" })),
    };

    match scanner::scan_library(&state.db, &library_path).await {
        Ok(result) => Json(json!({
            "status": "complete",
            "artists_added": result.artists_added,
            "albums_added": result.albums_added,
            "tracks_added": result.tracks_added,
            "errors": result.errors,
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn trigger_enrichment(State(state): State<Arc<AppState>>) -> Json<Value> {
    if state.config.metadata.discogs.api_token.is_none() {
        return Json(json!({ "error": "no discogs api_token configured" }));
    }

    let pool = state.db.clone();
    let config = state.config.metadata.discogs.clone();

    tokio::spawn(async move {
        match discogs::enrich_library(&pool, &config).await {
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
    });

    Json(json!({ "status": "started" }))
}
