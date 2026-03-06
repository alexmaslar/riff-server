use axum::Json;
use axum::Extension;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use riff_core::auth::Claims;

#[derive(Debug, Deserialize)]
pub struct PlaybackLogBody {
    pub message: String,
    pub context: Option<Value>,
}

pub async fn playback_log(
    Extension(claims): Extension<Claims>,
    Json(body): Json<PlaybackLogBody>,
) -> Result<Json<Value>, AppError> {
    tracing::warn!(user = %claims.sub, "[CLIENT] {}: {:?}", body.message, body.context);
    Ok(Json(json!({"ok": true})))
}
