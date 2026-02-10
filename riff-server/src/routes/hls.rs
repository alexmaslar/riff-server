use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::header,
    response::Response,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub quality: Option<String>,
}

/// Low-bitrate tier (kbps) — always 64 for poor connections.
const LO_BITRATE: u32 = 64;

/// HLS cache base directory.
fn hls_base_dir() -> Result<PathBuf, AppError> {
    dirs::data_dir()
        .map(|d| d.join("riff").join("hls_cache"))
        .ok_or_else(|| AppError::Internal("could not determine data directory".into()))
}

fn valid_variant(v: &str) -> bool {
    v == "hi" || v == "lo"
}

/// Find a segment file in either bitrate-keyed (e.g. `hi_256/`) or legacy (`hi/`) dirs.
fn find_segment(
    track_dir: &std::path::Path,
    variant: &str,
    segment: &str,
) -> Result<PathBuf, AppError> {
    // Check bitrate-keyed dirs (hi_256, hi_128, lo_64, etc.)
    if let Ok(entries) = std::fs::read_dir(track_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(&format!("{variant}_")) && entry.path().is_dir() {
                let candidate = entry.path().join(segment);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }
    // Fall back to legacy plain variant dir
    let legacy = track_dir.join(variant).join(segment);
    if legacy.exists() {
        return Ok(legacy);
    }
    Err(AppError::NotFound("segment not found".into()))
}

fn variant_bitrate(variant: &str, config_bitrate: u32) -> u32 {
    match variant {
        "hi" => config_bitrate,
        "lo" => LO_BITRATE,
        _ => config_bitrate,
    }
}

/// GET /tracks/{id}/hls/playlist.m3u8
///
/// Returns a multi-variant master playlist. No segments are generated here —
/// AVPlayer selects a variant and requests its playlist on-demand.
pub async fn master_playlist(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<StreamParams>,
) -> Result<Response, AppError> {
    if !state.ffmpeg_available {
        return Err(AppError::Internal("ffmpeg not available".into()));
    }

    // Verify track exists
    sqlx::query_scalar::<_, String>("SELECT id FROM tracks WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("track not found".into()))?;

    let config = state.config.read().await;
    let config_bitrate = config.streaming.remote_bitrate;
    drop(config);

    // Map quality param to hi-tier bitrate
    let hi_bitrate = match params.quality.as_deref() {
        Some("lossless") => config_bitrate, // Can't serve raw via HLS, best-effort
        Some("high") => 256,
        Some("normal") => 128,
        _ => config_bitrate, // Legacy behavior
    };

    // Propagate quality param to variant URLs
    let quality_param = params
        .quality
        .as_ref()
        .map(|q| format!("?quality={q}"))
        .unwrap_or_default();

    // BANDWIDTH in bits/sec with ~12% overhead for framing
    let hi_bw = (hi_bitrate as u64) * 1120;
    let lo_bw = (LO_BITRATE as u64) * 1120;

    let master = format!(
        "#EXTM3U\n\
         #EXT-X-STREAM-INF:BANDWIDTH={hi_bw},CODECS=\"mp4a.40.2\"\n\
         hi/playlist.m3u8{quality_param}\n\
         #EXT-X-STREAM-INF:BANDWIDTH={lo_bw},CODECS=\"mp4a.40.2\"\n\
         lo/playlist.m3u8{quality_param}\n"
    );

    Ok(Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(master))
        .unwrap())
}

/// GET /tracks/{id}/hls/{variant}/playlist.m3u8
///
/// Generates segments on-demand for the requested variant (hi or lo),
/// then returns the variant's m3u8 playlist.
pub async fn variant_playlist(
    State(state): State<Arc<AppState>>,
    Path((id, variant)): Path<(String, String)>,
    Query(params): Query<StreamParams>,
) -> Result<Response, AppError> {
    if !state.ffmpeg_available {
        return Err(AppError::Internal("ffmpeg not available".into()));
    }
    if !valid_variant(&variant) {
        return Err(AppError::BadRequest("invalid HLS variant".into()));
    }

    let (file_path, _format) = sqlx::query_as::<_, (String, String)>(
        "SELECT file_path, format FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("track not found".into()))?;

    let config = state.config.read().await;
    let config_bitrate = config.streaming.remote_bitrate;
    drop(config);

    // Derive bitrate from quality param or fall back to config
    let bitrate = match params.quality.as_deref() {
        Some("high") => variant_bitrate(&variant, 256),
        Some("normal") => variant_bitrate(&variant, 128),
        _ => variant_bitrate(&variant, config_bitrate),
    };

    // Include bitrate in cache path to separate different quality levels
    let track_dir = hls_base_dir()?.join(&id);
    let variant_dir = track_dir.join(format!("{}_{}", variant, bitrate));
    let playlist_path = variant_dir.join("playlist.m3u8");

    // Key includes track_id + variant + bitrate to allow concurrent generation
    let gen_key = format!("{id}/{}_{}", variant, bitrate);

    enum Action {
        Cached,
        Generate,
        Wait,
    }
    let action = {
        let mut generating = state.hls_generating.lock().await;
        if playlist_path.exists() {
            Action::Cached
        } else if generating.contains(&gen_key) {
            Action::Wait
        } else {
            generating.insert(gen_key.clone());
            Action::Generate
        }
    };

    match action {
        Action::Cached => {}
        Action::Wait => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
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
            let codec = if state.ffmpeg_has_fdk_aac {
                "libfdk_aac"
            } else {
                "aac"
            };

            // Acquire transcode semaphore to limit concurrent FFmpeg processes
            let _permit = state
                .transcode_semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AppError::Internal("transcode semaphore closed".into()))?;

            let result = generate_segments(&file_path, &variant_dir, bitrate, codec).await;
            state.hls_generating.lock().await.remove(&gen_key);
            result?;
        }
    }

    // Touch the track dir so cleanup knows it was recently accessed
    let _ = filetime::set_file_mtime(&track_dir, filetime::FileTime::now());

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

/// GET /tracks/{id}/hls/{variant}/{segment}
pub async fn serve_segment(
    Path((id, variant, segment)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    if !valid_variant(&variant) {
        return Err(AppError::BadRequest("invalid HLS variant".into()));
    }
    // Validate segment filename — prevent path traversal
    if !segment
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return Err(AppError::BadRequest("invalid segment name".into()));
    }

    // Try bitrate-keyed dirs first, then fall back to legacy plain variant dirs
    let base = hls_base_dir()?.join(&id);
    let segment_path = find_segment(&base, &variant, &segment)?;

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

/// Run FFmpeg to generate HLS segments for a variant tier.
async fn generate_segments(
    file_path: &str,
    variant_dir: &std::path::Path,
    bitrate_kbps: u32,
    codec: &str,
) -> Result<(), AppError> {
    tokio::fs::create_dir_all(variant_dir)
        .await
        .map_err(|e| AppError::Internal(format!("failed to create HLS dir: {e}")))?;

    let seg_pattern = variant_dir.join("seg%05d.ts");
    let playlist_path = variant_dir.join("playlist.m3u8");

    let result = tokio::process::Command::new("ffmpeg")
        .args([
            "-i",
            file_path,
            "-vn",
            "-codec:a",
            codec,
            "-b:a",
            &format!("{}k", bitrate_kbps),
            "-f",
            "hls",
            "-hls_time",
            "10",
            "-hls_list_size",
            "0",
            "-hls_segment_filename",
            seg_pattern.to_str().unwrap_or("seg%05d.ts"),
            playlist_path.to_str().unwrap_or("playlist.m3u8"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| AppError::Internal(format!("ffmpeg spawn failed: {e}")))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let _ = tokio::fs::remove_dir_all(variant_dir).await;
        return Err(AppError::Internal(format!(
            "ffmpeg HLS failed ({}): {}",
            result.status,
            stderr.lines().last().unwrap_or("unknown error")
        )));
    }

    if !result.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        if let Some(last) = stderr.lines().last() {
            tracing::debug!("ffmpeg HLS: {last}");
        }
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
