use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;

use crate::error::AppError;
use crate::AppState;

/// HLS cache directory: ~/Library/Application Support/riff/hls_cache/{track_id}/
fn hls_cache_dir(track_id: &str) -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("riff").join("hls_cache").join(track_id))
}

/// GET /tracks/{id}/hls/playlist.m3u8
pub async fn master_playlist(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if !state.ffmpeg_available {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "ffmpeg not available" })),
        )
            .into_response();
    }

    let track = sqlx::query_as::<_, (String, String)>(
        "SELECT file_path, format FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let (file_path, _format) = match track {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "track not found" })))
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

    let cache_dir = match hls_cache_dir(&id) {
        Some(d) => d,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not determine data directory" })),
            )
                .into_response()
        }
    };

    let playlist_path = cache_dir.join("playlist.m3u8");

    // If playlist already exists on disk, serve it directly
    if !playlist_path.exists() {
        // Acquire generation lock to prevent concurrent FFmpeg runs for the same track
        {
            let mut generating = state.hls_generating.lock().await;
            if generating.contains(&id) {
                drop(generating);
                // Another task is generating — wait for it
                for _ in 0..300 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    if playlist_path.exists() {
                        break;
                    }
                }
                if !playlist_path.exists() {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "HLS generation timed out" })),
                    )
                        .into_response();
                }
            } else {
                generating.insert(id.clone());
            }
        }

        // Generate if still needed (double-check after acquiring lock)
        if !playlist_path.exists() {
            let config = state.config.read().await;
            let bitrate = config.streaming.remote_bitrate;
            drop(config);

            if let Err(e) = generate_segments(&file_path, &cache_dir, bitrate).await {
                // Remove from in-progress set
                state.hls_generating.lock().await.remove(&id);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("HLS generation failed: {e:?}") })),
                )
                    .into_response();
            }
        }

        // Remove from in-progress set
        state.hls_generating.lock().await.remove(&id);
    }

    // Touch the directory so cleanup knows it was recently accessed
    let _ = filetime::set_file_mtime(&cache_dir, filetime::FileTime::now());

    // Serve the m3u8 playlist
    match tokio::fs::read_to_string(&playlist_path).await {
        Ok(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
            .header(header::CACHE_CONTROL, "private, max-age=3600")
            .body(Body::from(content))
            .unwrap(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to read playlist: {e}") })),
        )
            .into_response(),
    }
}

/// GET /tracks/{id}/hls/{segment}
pub async fn serve_segment(
    State(_state): State<Arc<AppState>>,
    Path((id, segment)): Path<(String, String)>,
) -> Response {
    // Validate segment filename — prevent path traversal
    if !segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid segment name" })),
        )
            .into_response();
    }

    let cache_dir = match hls_cache_dir(&id) {
        Some(d) => d,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not determine data directory" })),
            )
                .into_response()
        }
    };

    let segment_path = cache_dir.join(&segment);
    let file = match File::open(&segment_path).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "segment not found" })),
            )
                .into_response()
        }
    };

    let file_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("metadata read failed: {e}") })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Run FFmpeg to generate HLS segments from a source file.
async fn generate_segments(
    file_path: &str,
    cache_dir: &std::path::Path,
    bitrate_kbps: u32,
) -> Result<(), AppError> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|e| AppError::Internal(format!("failed to create HLS cache dir: {e}")))?;

    let seg_pattern = cache_dir.join("seg%03d.ts");
    let playlist_path = cache_dir.join("playlist.m3u8");

    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-i",
            file_path,
            "-vn",
            "-codec:a",
            "aac",
            "-b:a",
            &format!("{}k", bitrate_kbps),
            "-f",
            "hls",
            "-hls_time",
            "10",
            "-hls_list_size",
            "0",
            "-hls_segment_filename",
            seg_pattern.to_str().unwrap_or("seg%03d.ts"),
            playlist_path.to_str().unwrap_or("playlist.m3u8"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|e| AppError::Internal(format!("failed to run ffmpeg: {e}")))?;

    if !status.success() {
        // Clean up partial output
        let _ = tokio::fs::remove_dir_all(cache_dir).await;
        return Err(AppError::Internal(format!(
            "ffmpeg HLS segmentation failed with status {status}"
        )));
    }

    Ok(())
}

/// Remove HLS cache directories that haven't been accessed in 24 hours.
pub async fn cleanup_hls_cache() {
    let base = match dirs::data_dir().map(|d| d.join("riff").join("hls_cache")) {
        Some(d) if d.exists() => d,
        _ => return,
    };

    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(86400);

    let mut entries = match tokio::fs::read_dir(&base).await {
        Ok(e) => e,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        if let Some(mtime) = mtime {
            if mtime < cutoff {
                tracing::debug!("removing stale HLS cache: {}", path.display());
                let _ = tokio::fs::remove_dir_all(&path).await;
            }
        }
    }
}
