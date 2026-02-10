use axum::{
    body::Body,
    extract::{Path, State},
    http::header,
    response::Response,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;

use crate::error::AppError;
use crate::AppState;

/// HLS cache directory: ~/Library/Application Support/riff/hls_cache/{track_id}/
fn hls_cache_dir(track_id: &str) -> Result<PathBuf, AppError> {
    dirs::data_dir()
        .map(|d| d.join("riff").join("hls_cache").join(track_id))
        .ok_or_else(|| AppError::Internal("could not determine data directory".into()))
}

/// GET /tracks/{id}/hls/playlist.m3u8
pub async fn master_playlist(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    if !state.ffmpeg_available {
        return Err(AppError::Internal("ffmpeg not available".into()));
    }

    let (file_path, _format) = sqlx::query_as::<_, (String, String)>(
        "SELECT file_path, format FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("track not found".into()))?;

    let cache_dir = hls_cache_dir(&id)?;
    let playlist_path = cache_dir.join("playlist.m3u8");

    // Determine if we need to generate, wait, or serve from cache.
    // The existence check and lock acquisition are atomic to prevent TOCTOU races.
    enum Action {
        Cached,
        Generate,
        Wait,
    }
    let action = {
        let mut generating = state.hls_generating.lock().await;
        if playlist_path.exists() {
            Action::Cached
        } else if generating.contains(&id) {
            Action::Wait
        } else {
            generating.insert(id.clone());
            Action::Generate
        }
    };

    match action {
        Action::Cached => {} // Already generated, serve below
        Action::Wait => {
            // Another task is generating — poll until the playlist appears
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if playlist_path.exists() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(AppError::Internal("HLS generation timed out".into()));
                }
            }
        }
        Action::Generate => {
            let config = state.config.read().await;
            let bitrate = config.streaming.remote_bitrate;
            drop(config);

            let result = generate_segments(&file_path, &cache_dir, bitrate).await;
            // Always remove from in-progress set, even on failure
            state.hls_generating.lock().await.remove(&id);
            result?;
        }
    }

    // Touch the directory so cleanup knows it was recently accessed
    let _ = filetime::set_file_mtime(&cache_dir, filetime::FileTime::now());

    // Serve the m3u8 playlist
    let content = tokio::fs::read_to_string(&playlist_path)
        .await
        .map_err(|e| AppError::Internal(format!("failed to read playlist: {e}")))?;

    Ok(Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(content))
        .unwrap())
}

/// GET /tracks/{id}/hls/{segment}
pub async fn serve_segment(
    Path((id, segment)): Path<(String, String)>,
) -> Result<Response, AppError> {
    // Validate segment filename — prevent path traversal
    if !segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return Err(AppError::BadRequest("invalid segment name".into()));
    }

    let cache_dir = hls_cache_dir(&id)?;
    let segment_path = cache_dir.join(&segment);

    let file = File::open(&segment_path)
        .await
        .map_err(|_| AppError::NotFound("segment not found".into()))?;

    let file_size = file
        .metadata()
        .await
        .map_err(|e| AppError::Internal(format!("metadata read failed: {e}")))?
        .len();

    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);

    Ok(Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap())
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

    let cutoff = std::time::SystemTime::now() - Duration::from_secs(86400);

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
