use axum::{Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingDiagBody {
    pub track_name: String,
    pub first_byte_ms: i64,
    pub file_opened_ms: i64,
    pub ready_ms: i64,
    pub bytes_at_ready: i64,
    pub buffers_at_ready: i32,
    pub prefetched: bool,
}

/// POST /diagnostics/streaming — Print streaming timing to stdout (no DB)
pub async fn record(
    Extension(_claims): Extension<Claims>,
    Json(body): Json<StreamingDiagBody>,
) -> Result<Json<Value>, AppError> {
    println!(
        "[streaming-diag] \"{}\" first_byte={}ms opened={}ms ready={}ms bytes={} buffers={} prefetched={}",
        body.track_name,
        body.first_byte_ms,
        body.file_opened_ms,
        body.ready_ms,
        body.bytes_at_ready,
        body.buffers_at_ready,
        body.prefetched,
    );
    Ok(Json(json!({ "ok": true })))
}
