use axum::{
    extract::{Path, State},
    Json,
};
use riff_core::{config::LibraryEntry, db, scanner};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// GET /libraries — List all libraries with stats (protected, non-admin)
pub async fn list_libraries(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "SELECT l.id, l.name, l.path, l.isolated, l.display_order,
                l.auto_enrich, l.album_summaries, l.album_ratings,
                l.album_recommendations, l.artist_bios, l.artist_recommendations,
                l.scan_interval,
                (SELECT COUNT(*) FROM albums WHERE library_id = l.id) as album_count,
                (SELECT COUNT(*) FROM tracks WHERE library_id = l.id) as track_count
         FROM libraries l
         ORDER BY l.display_order",
    )
    .fetch_all(&state.db)
    .await?;

    let libraries: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "name": row.get::<String, _>("name"),
                "path": row.get::<String, _>("path"),
                "isolated": row.get::<bool, _>("isolated"),
                "albumCount": row.get::<i64, _>("album_count"),
                "trackCount": row.get::<i64, _>("track_count"),
                "autoEnrich": row.get::<Option<bool>, _>("auto_enrich"),
                "albumSummaries": row.get::<Option<bool>, _>("album_summaries"),
                "albumRatings": row.get::<Option<bool>, _>("album_ratings"),
                "albumRecommendations": row.get::<Option<bool>, _>("album_recommendations"),
                "artistBios": row.get::<Option<bool>, _>("artist_bios"),
                "artistRecommendations": row.get::<Option<bool>, _>("artist_recommendations"),
                "scanInterval": row.get::<Option<i64>, _>("scan_interval"),
            })
        })
        .collect();

    Ok(Json(json!({ "libraries": libraries })))
}

#[derive(Debug, Deserialize)]
pub struct AddLibraryBody {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub isolated: bool,
}

/// POST /libraries — Add a new library (admin)
pub async fn add_library(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddLibraryBody>,
) -> Result<Json<Value>, AppError> {
    // Add to config
    {
        let mut config = state.config.write().await;
        config.library.libraries.push(LibraryEntry {
            name: body.name.clone(),
            path: body.path.clone(),
            isolated: body.isolated,
            auto_enrich: None,
            album_summaries: None,
            album_ratings: None,
            album_recommendations: None,
            artist_bios: None,
            artist_recommendations: None,
            scan_interval: None,
        });
        config.save().map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // Sync DB
    let config = state.config.read().await;
    let resolved = config.resolved_libraries();
    drop(config);
    db::sync_libraries(&state.db, &resolved)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Get the new library ID and trigger scan
    let library_id = db::get_library_id_by_path(&state.db, &body.path)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or_else(|| AppError::Internal("library not found after sync".into()))?;

    let path = body.path.clone();
    let db = state.db.clone();
    let lib_id_for_scan = library_id.clone();
    tokio::spawn(async move {
        match scanner::scan_library(&db, &path, &lib_id_for_scan).await {
            Ok(r) => tracing::info!("scan complete for new library: {} artists, {} albums, {} tracks", r.artists_added, r.albums_added, r.tracks_added),
            Err(e) => tracing::warn!("scan failed for new library: {e}"),
        }
    });

    Ok(Json(json!({ "status": "added", "id": library_id })))
}

/// Deserialize helper: distinguishes between missing key (None) and explicit null (Some(None)).
/// Used for per-library config overrides where:
/// - absent key = don't change the value
/// - explicit null = reset to global (None)
/// - explicit true/false = set override
fn deserialize_optional_nullable<'de, D>(deserializer: D) -> Result<Option<Option<bool>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

fn deserialize_optional_nullable_u64<'de, D>(deserializer: D) -> Result<Option<Option<u64>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateLibraryBody {
    pub name: Option<String>,
    pub path: Option<String>,
    pub isolated: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub auto_enrich: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub album_summaries: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub album_ratings: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub album_recommendations: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub artist_bios: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub artist_recommendations: Option<Option<bool>>,
    #[serde(default, deserialize_with = "deserialize_optional_nullable_u64")]
    pub scan_interval: Option<Option<u64>>,
}

/// PUT /libraries/:id — Update a library (admin)
pub async fn update_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateLibraryBody>,
) -> Result<Json<Value>, AppError> {
    // Find the library path by ID
    let row: Option<(String,)> = sqlx::query_as("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;

    let lib_path = row
        .ok_or_else(|| AppError::NotFound("library not found".into()))?
        .0;

    let path_changed = body.path.as_ref().is_some_and(|p| *p != lib_path);

    // Update config
    {
        let mut config = state.config.write().await;
        if let Some(entry) = config.library.libraries.iter_mut().find(|l| l.path == lib_path) {
            if let Some(ref name) = body.name {
                entry.name = name.clone();
            }
            if let Some(ref path) = body.path {
                entry.path = path.clone();
            }
            if let Some(isolated) = body.isolated {
                entry.isolated = isolated;
            }
            if let Some(v) = body.auto_enrich {
                entry.auto_enrich = v;
            }
            if let Some(v) = body.album_summaries {
                entry.album_summaries = v;
            }
            if let Some(v) = body.album_ratings {
                entry.album_ratings = v;
            }
            if let Some(v) = body.album_recommendations {
                entry.album_recommendations = v;
            }
            if let Some(v) = body.artist_bios {
                entry.artist_bios = v;
            }
            if let Some(v) = body.artist_recommendations {
                entry.artist_recommendations = v;
            }
            if let Some(v) = body.scan_interval {
                entry.scan_interval = v;
            }
        }
        config.save().map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // If path changed, wipe data for the old library and re-scan at the new path
    if path_changed {
        db::wipe_library_data(&state.db, &id).await
            .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // Sync DB
    let config = state.config.read().await;
    let resolved = config.resolved_libraries();
    drop(config);
    db::sync_libraries(&state.db, &resolved)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Trigger re-scan if path changed
    if path_changed {
        let new_path = body.path.unwrap();
        let db = state.db.clone();
        let lib_id = id.clone();
        tokio::spawn(async move {
            match scanner::scan_library(&db, &new_path, &lib_id).await {
                Ok(r) => tracing::info!("re-scan complete for library {}: {} artists, {} albums, {} tracks", lib_id, r.artists_added, r.albums_added, r.tracks_added),
                Err(e) => tracing::warn!("re-scan failed for library {}: {e}", lib_id),
            }
        });
    }

    Ok(Json(json!({ "status": "updated" })))
}

/// DELETE /libraries/:id — Remove a library (admin)
pub async fn remove_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Find the library path by ID
    let row: Option<(String,)> = sqlx::query_as("SELECT path FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;

    let lib_path = row
        .ok_or_else(|| AppError::NotFound("library not found".into()))?
        .0;

    // Remove from config
    {
        let mut config = state.config.write().await;
        config.library.libraries.retain(|l| l.path != lib_path);
        config.save().map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // Sync DB (will wipe data and remove the row)
    let config = state.config.read().await;
    let resolved = config.resolved_libraries();
    drop(config);
    db::sync_libraries(&state.db, &resolved)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(json!({ "status": "removed" })))
}

/// POST /libraries/:id/scan — Scan a single library (admin)
pub async fn scan_single_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Guard: prevent overlap with periodic scanner
    if state.scan_running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Ok(Json(json!({ "status": "already_running" })));
    }

    let result = async {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT id, path FROM libraries WHERE id = ?")
                .bind(&id)
                .fetch_optional(&state.db)
                .await?;

        let (library_id, lib_path) =
            row.ok_or_else(|| AppError::NotFound("library not found".into()))?;

        let scan_result = scanner::scan_library(&state.db, &lib_path, &library_id)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;

        Ok(Json(json!({
            "status": "complete",
            "artists_added": scan_result.artists_added,
            "albums_added": scan_result.albums_added,
            "tracks_added": scan_result.tracks_added,
            "errors": scan_result.errors,
        })))
    }.await;

    state.scan_running.store(false, Ordering::SeqCst);
    result
}
