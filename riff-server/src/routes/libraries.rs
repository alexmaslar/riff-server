use axum::{
    extract::{Path, State},
    Json,
};
use riff_core::{config::LibraryEntry, db, scanner};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// GET /libraries — List all libraries with stats (protected, non-admin)
pub async fn list_libraries(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "SELECT l.id, l.name, l.path, l.isolated, l.display_order,
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
    // Check for duplicate path
    {
        let config = state.config.read().await;
        let resolved = config.resolved_libraries();
        if resolved.iter().any(|l| l.path == body.path) {
            return Err(AppError::BadRequest(format!("a library already exists at path: {}", body.path)));
        }
    }

    // Add to config
    {
        let mut config = state.config.write().await;
        config.library.libraries.push(LibraryEntry {
            name: body.name.clone(),
            path: body.path.clone(),
            isolated: body.isolated,
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
            Ok(r) => tracing::info!(
                artists_added = r.artists_added,
                albums_added = r.albums_added,
                tracks_added = r.tracks_added,
                tracks_removed = r.tracks_removed,
                albums_removed = r.albums_removed,
                artists_removed = r.artists_removed,
                "scan complete for new library",
            ),
            Err(e) => tracing::warn!(error = %e, "scan failed for new library"),
        }
    });

    // Invalidate library IDs cache
    *state.library_ids_cache.write().await = None;

    Ok(Json(json!({ "status": "added", "id": library_id })))
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
        // Migrate legacy single-path config to libraries array if needed
        if config.library.libraries.is_empty() {
            if let Some(legacy_path) = config.library.path.take() {
                config.library.libraries.push(LibraryEntry {
                    name: "Music".to_string(),
                    path: legacy_path,
                    isolated: false,
                    scan_interval: None,
                });
            }
        }
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
            if let Some(v) = body.scan_interval {
                entry.scan_interval = v;
            }
        }
        config.save().map_err(|e| AppError::Internal(e.to_string()))?;
    }

    // If path changed, relocate file paths and re-scan at the new path
    if path_changed {
        db::relocate_library_paths(&state.db, &id, &lib_path, body.path.as_ref().expect("path_changed is true")).await
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
        let new_path = body.path.expect("path_changed is true");
        let db = state.db.clone();
        let lib_id = id.clone();
        tokio::spawn(async move {
            match scanner::scan_library(&db, &new_path, &lib_id).await {
                Ok(r) => tracing::info!(
                    library_id = %lib_id,
                    artists_added = r.artists_added,
                    albums_added = r.albums_added,
                    tracks_added = r.tracks_added,
                    tracks_removed = r.tracks_removed,
                    albums_removed = r.albums_removed,
                    artists_removed = r.artists_removed,
                    "re-scan complete",
                ),
                Err(e) => tracing::warn!(library_id = %lib_id, error = %e, "re-scan failed"),
            }
        });
    }

    // Invalidate library IDs cache
    *state.library_ids_cache.write().await = None;

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
        // Migrate legacy single-path config to libraries array if needed
        if config.library.libraries.is_empty() {
            if let Some(legacy_path) = config.library.path.take() {
                config.library.libraries.push(LibraryEntry {
                    name: "Music".to_string(),
                    path: legacy_path,
                    isolated: false,
                    scan_interval: None,
                });
            }
        }
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

    // Invalidate library IDs cache
    *state.library_ids_cache.write().await = None;

    Ok(Json(json!({ "status": "removed" })))
}

/// POST /libraries/:id/scan — Scan a single library (admin)
pub async fn scan_single_library(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Guard: prevent overlap with periodic scanner
    if !state.stage_manager.try_start("scan") {
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
            "tracks_removed": scan_result.tracks_removed,
            "albums_removed": scan_result.albums_removed,
            "artists_removed": scan_result.artists_removed,
            "errors": scan_result.errors,
        })))
    }.await;

    state.stage_manager.finish("scan");
    result
}
