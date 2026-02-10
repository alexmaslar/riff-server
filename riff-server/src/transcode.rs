use axum::body::Body;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::error::AppError;

/// Returns true if the format is lossless and would benefit from transcoding.
pub fn is_lossless(format: &str) -> bool {
    matches!(format, "FLAC" | "ALAC" | "WAV" | "AIFF")
}

/// Spawns FFmpeg to transcode a lossless file to AAC, returning a streaming Body.
/// The child process is killed if the client disconnects (kill_on_drop).
pub async fn transcode_to_aac(file_path: &str, bitrate_kbps: u32) -> Result<Body, AppError> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-i",
            file_path,
            "-vn",            // strip embedded cover art
            "-codec:a",
            "aac",
            "-b:a",
            &format!("{}k", bitrate_kbps),
            "-f",
            "adts",           // streamable AAC frames — no seeking needed
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AppError::Internal(format!("failed to spawn ffmpeg: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Internal("ffmpeg stdout not captured".into()))?;

    // Spawn a task to wait on the child so its exit status is collected
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if !status.success() => {
                tracing::warn!("ffmpeg exited with status {status}");
            }
            Err(e) => {
                tracing::warn!("ffmpeg wait error: {e}");
            }
            _ => {}
        }
    });

    let stream = ReaderStream::with_capacity(stdout, 256 * 1024);
    Ok(Body::from_stream(stream))
}
