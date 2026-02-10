use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::error::AppError;
use crate::middleware::IsRemote;
use crate::transcode;
use crate::AppState;

fn mime_for_format(format: &str) -> &'static str {
    match format {
        "FLAC" => "audio/flac",
        "ALAC" => "audio/mp4",
        "WAV" => "audio/wav",
        "AIFF" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

fn ext_for_format(format: &str) -> &'static str {
    match format {
        "FLAC" => "flac",
        "ALAC" => "m4a",
        "WAV" => "wav",
        "AIFF" => "aiff",
        _ => "bin",
    }
}

pub async fn stream_track(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, AppError> {
    let is_remote = request
        .extensions()
        .get::<IsRemote>()
        .map(|r| r.0)
        .unwrap_or(false);
    let headers = request.headers().clone();

    let (file_path, format, file_size) = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT file_path, format, file_size_bytes FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("track not found".into()))?;

    // Remote + FFmpeg + lossless → transcode to AAC
    if is_remote && state.ffmpeg_available && transcode::is_lossless(&format) {
        let config = state.config.read().await;
        let bitrate = config.streaming.remote_bitrate;
        drop(config);

        match transcode::transcode_to_aac(&file_path, bitrate).await {
            Ok(body) => {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "audio/aac")
                    .header("X-Riff-Transcoded", "true")
                    // No Accept-Ranges, Content-Length, or ETag — stream is not seekable
                    .body(body)
                    .unwrap());
            }
            Err(e) => {
                tracing::warn!("transcode failed, falling back to raw: {e}");
                // fall through to serve raw file
            }
        }
    }

    let mime = mime_for_format(&format);

    // Generate ETag from file size + mtime for resume validation
    let metadata = tokio::fs::metadata(&file_path)
        .await
        .map_err(|e| AppError::Internal(format!("metadata failed: {e}")))?;
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("\"{:x}-{:x}\"", file_size, mtime);

    // If-Range validation: if the client sends If-Range and it doesn't match
    // the current ETag, ignore the Range header and serve the full file.
    let if_range_valid = headers
        .get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|ir| ir == etag)
        .unwrap_or(true); // no If-Range header → always honor Range

    // Parse Range header (only if If-Range is valid or absent)
    let range = if if_range_valid {
        headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| parse_range(s, file_size as u64))
    } else {
        None
    };

    // Use larger buffer for remote connections
    let buf_size = if is_remote { 256 * 1024 } else { 64 * 1024 };

    match range {
        Some((start, end)) => {
            // Partial content (206)
            let length = end - start + 1;
            let mut file = File::open(&file_path)
                .await
                .map_err(|e| AppError::Internal(format!("file open failed: {e}")))?;

            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| AppError::Internal(format!("seek failed: {e}")))?;

            let stream = tokio_util::io::ReaderStream::with_capacity(file.take(length), buf_size);

            Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, length.to_string())
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, file_size),
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, &etag)
                .body(Body::from_stream(stream))
                .unwrap())
        }
        None => {
            // Full content (200)
            let file = File::open(&file_path)
                .await
                .map_err(|e| AppError::Internal(format!("file open failed: {e}")))?;

            let stream = tokio_util::io::ReaderStream::with_capacity(file, buf_size);

            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, file_size.to_string())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, &etag)
                .body(Body::from_stream(stream))
                .unwrap())
        }
    }
}

pub async fn download_track(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let (file_path, format, title, file_size) = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT file_path, format, title, file_size_bytes FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("track not found".into()))?;

    let mime = mime_for_format(&format);
    let ext = ext_for_format(&format);
    let filename = format!("{}.{}", title, ext);

    let file = File::open(&file_path)
        .await
        .map_err(|e| AppError::Internal(format!("file open failed: {e}")))?;

    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Parse a Range header value like "bytes=0-1023" or "bytes=1024-"
fn parse_range(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
    let range_str = range_header.strip_prefix("bytes=")?;
    let parts: Vec<&str> = range_str.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start: u64 = if parts[0].is_empty() {
        // Suffix range: "-500" means last 500 bytes
        let suffix_len: u64 = parts[1].parse().ok()?;
        file_size.saturating_sub(suffix_len)
    } else {
        parts[0].parse().ok()?
    };

    let end: u64 = if parts[1].is_empty() {
        file_size - 1
    } else {
        parts[1].parse().ok()?
    };

    if start > end || start >= file_size {
        return None;
    }

    Some((start, end.min(file_size - 1)))
}
