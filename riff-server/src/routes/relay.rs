use axum::{Extension, Json, extract::State};
use riff_core::auth::Claims;
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::AppState;
use crate::error::AppError;

pub async fn relay_info(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    let relay = &config.relay;

    let relay_url = relay.server_id.as_ref().map(|id| {
        format!("https://{id}.relay.riff.audio")
    });

    Ok(Json(json!({
        "enabled": relay.enabled,
        "relay_url": relay_url,
        "server_id": relay.server_id,
        "connected": state.relay_connected.load(Ordering::Relaxed),
        "cert_fingerprint": state.cert_fingerprint,
    })))
}
