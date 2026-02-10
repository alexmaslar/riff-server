use std::path::PathBuf;

use axum::body::Body;
use tokio::process::Command;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

use crate::error::AppError;

/// Returns true if the format is lossless and would benefit from transcoding.
pub fn is_lossless(format: &str) -> bool {
    matches!(format, "FLAC" | "ALAC" | "WAV" | "AIFF")
}

/// Cache directory for transcoded files.
fn cache_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("riff").join("transcode_cache"))
}

/// Path for a cached transcode file.
fn cache_path(track_id: &str, bitrate_kbps: u32) -> Option<PathBuf> {
    cache_dir().map(|d| d.join(format!("{}_{}.aac", track_id, bitrate_kbps)))
}

/// Options for transcoding a track.
pub struct TranscodeOptions {
    pub file_path: String,
    pub track_id: String,
    pub bitrate_kbps: u32,
    pub use_fdk_aac: bool,
    pub semaphore: std::sync::Arc<tokio::sync::Semaphore>,
}

/// Result of a transcode operation.
pub enum TranscodeResult {
    /// Cached file ready to serve with full HTTP semantics (Content-Length, Range, ETag).
    Cached { path: PathBuf, size: u64 },
    /// Streaming body — no Content-Length or Range support.
    Streaming { body: Body },
}

/// Gets or creates a transcoded AAC version of a track.
///
/// 1. If a cached file exists, returns immediately.
/// 2. If cache dir is available, transcodes to file (atomic write), returns cached path.
/// 3. Fallback: streams via FFmpeg pipe (no seeking/Content-Length).
///
/// Respects the concurrency semaphore to prevent OOM on low-power hardware.
pub async fn transcode_to_aac(opts: TranscodeOptions) -> Result<TranscodeResult, AppError> {
    let codec = if opts.use_fdk_aac {
        "libfdk_aac"
    } else {
        "aac"
    };

    // 1. Check cache
    if let Some(ref path) = cache_path(&opts.track_id, opts.bitrate_kbps) {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            if meta.len() > 0 {
                tracing::debug!("transcode cache hit: {}", path.display());
                return Ok(TranscodeResult::Cached {
                    path: path.clone(),
                    size: meta.len(),
                });
            }
        }
    }

    // 2. Acquire semaphore (limits concurrent FFmpeg processes)
    let permit = opts
        .semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::Internal("transcode semaphore closed".into()))?;

    // 3. Try file-based transcode (preferred — enables Content-Length + Range)
    if let Some(path) = cache_path(&opts.track_id, opts.bitrate_kbps) {
        match transcode_to_file(&opts.file_path, &path, opts.bitrate_kbps, codec).await {
            Ok(size) => {
                drop(permit); // Release semaphore — FFmpeg is done
                return Ok(TranscodeResult::Cached { path, size });
            }
            Err(e) => {
                tracing::warn!("file-based transcode failed, trying stream: {e}");
                // Fall through to streaming
            }
        }
    }

    // 4. Fallback: stream via pipe (permit transfers to background task)
    let body = stream_transcode(&opts.file_path, opts.bitrate_kbps, codec, permit).await?;
    Ok(TranscodeResult::Streaming { body })
}

/// Transcode to a file on disk. Returns the file size on success.
/// Uses atomic write (tmp + rename) to prevent serving partial files.
async fn transcode_to_file(
    input: &str,
    output: &PathBuf,
    bitrate_kbps: u32,
    codec: &str,
) -> Result<u64, AppError> {
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AppError::Internal(format!("create cache dir failed: {e}")))?;
    }

    let tmp = output.with_extension("aac.tmp");
    let tmp_str = tmp
        .to_str()
        .ok_or_else(|| AppError::Internal("invalid cache path".into()))?;

    let result = Command::new("ffmpeg")
        .args([
            "-i", input, "-vn", "-codec:a", codec, "-b:a",
            &format!("{}k", bitrate_kbps),
            "-f", "adts", "-y", tmp_str,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| AppError::Internal(format!("ffmpeg spawn failed: {e}")))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AppError::Internal(format!(
            "ffmpeg exited with {}: {}",
            result.status,
            stderr.lines().last().unwrap_or("unknown error")
        )));
    }

    // Log stderr at debug level (progress info, codec details)
    if !result.stderr.is_empty() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        if let Some(last_line) = stderr.lines().last() {
            tracing::debug!("ffmpeg: {last_line}");
        }
    }

    // Atomic rename
    tokio::fs::rename(&tmp, output)
        .await
        .map_err(|e| AppError::Internal(format!("cache rename failed: {e}")))?;

    let size = tokio::fs::metadata(output)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    tracing::debug!("transcode cached: {} ({size} bytes)", output.display());
    Ok(size)
}

/// Stream transcode via FFmpeg pipe (fallback when file caching fails).
/// The semaphore permit is held until FFmpeg exits.
async fn stream_transcode(
    file_path: &str,
    bitrate_kbps: u32,
    codec: &str,
    permit: OwnedSemaphorePermit,
) -> Result<Body, AppError> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-i", file_path, "-vn", "-codec:a", codec, "-b:a",
            &format!("{}k", bitrate_kbps),
            "-f", "adts", "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AppError::Internal(format!("ffmpeg spawn failed: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal("ffmpeg stdout not captured".into()))?;

    let stderr = child.stderr.take();

    // Background task: wait for FFmpeg exit, log errors, hold semaphore permit
    tokio::spawn(async move {
        let _permit = permit; // Hold until FFmpeg exits
        match child.wait().await {
            Ok(status) if !status.success() => {
                if let Some(mut err) = stderr {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf).await;
                    let msg = String::from_utf8_lossy(&buf);
                    tracing::warn!(
                        "ffmpeg stream exited with {status}: {}",
                        msg.lines().last().unwrap_or("unknown")
                    );
                } else {
                    tracing::warn!("ffmpeg stream exited with {status}");
                }
            }
            Err(e) => tracing::warn!("ffmpeg wait error: {e}"),
            _ => {}
        }
    });

    let stream = ReaderStream::with_capacity(stdout, 256 * 1024);
    Ok(Body::from_stream(stream))
}

/// Remove cached transcode files older than `max_age`.
pub async fn cleanup_cache(max_age: std::time::Duration) {
    let Some(dir) = cache_dir() else { return };
    let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
        return;
    };

    let now = std::time::SystemTime::now();
    let mut removed = 0u32;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() > max_age
            && tokio::fs::remove_file(entry.path()).await.is_ok()
        {
            removed += 1;
        }
    }

    if removed > 0 {
        tracing::info!("transcode cache: pruned {removed} stale entries");
    }
}
