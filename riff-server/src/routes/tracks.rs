use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

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

pub async fn stream_track(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let track = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT file_path, format, file_size_bytes FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let (file_path, format, file_size) = match track {
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

    let mime = mime_for_format(&format);

    // Generate ETag from file size + mtime for resume validation
    let metadata = match tokio::fs::metadata(&file_path).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("metadata failed: {}", e) })),
            )
                .into_response()
        }
    };
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

    match range {
        Some((start, end)) => {
            // Partial content (206)
            let length = end - start + 1;
            let mut file = match File::open(&file_path).await {
                Ok(f) => f,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("file open failed: {}", e) })),
                    )
                        .into_response()
                }
            };

            if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("seek failed: {}", e) })),
                )
                    .into_response();
            }

            let stream = tokio_util::io::ReaderStream::with_capacity(file.take(length), 64 * 1024);

            Response::builder()
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
                .unwrap()
        }
        None => {
            // Full content (200)
            let file = match File::open(&file_path).await {
                Ok(f) => f,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("file open failed: {}", e) })),
                    )
                        .into_response()
                }
            };

            let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CONTENT_LENGTH, file_size.to_string())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, &etag)
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

pub async fn download_track(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let track = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT file_path, format, title, file_size_bytes FROM tracks WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let (file_path, format, title, file_size) = match track {
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

    let mime = mime_for_format(&format);
    let ext = match format.as_str() {
        "FLAC" => "flac",
        "ALAC" => "m4a",
        "WAV" => "wav",
        "AIFF" => "aiff",
        _ => "bin",
    };
    let filename = format!("{}.{}", title, ext);

    let file = match File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("file open failed: {}", e) })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(Body::from_stream(stream))
        .unwrap()
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
