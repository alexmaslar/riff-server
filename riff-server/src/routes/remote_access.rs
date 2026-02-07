use axum::{extract::State, Json};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

pub async fn get_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let status = state.tunnel_manager.status.read().await;
    Ok(Json(json!({
        "enabled": status.enabled,
        "url": status.url,
        "status": status.status,
    })))
}

pub async fn enable(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let port = state.config.read().await.server.port;

    state
        .tunnel_manager
        .start(port)
        .await
        .map_err(|e| AppError::Internal(format!("failed to start tunnel: {e}")))?;

    // Update config to persist the enabled state
    {
        let mut config = state.config.write().await;
        config.remote_access.enabled = true;
        if let Err(e) = config.save() {
            tracing::warn!("failed to save config: {e}");
        }
    }

    let status = state.tunnel_manager.status.read().await;
    Ok(Json(json!({
        "enabled": status.enabled,
        "url": status.url,
        "status": status.status,
    })))
}

pub async fn disable(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    state.tunnel_manager.stop().await;

    // Update config to persist the disabled state
    {
        let mut config = state.config.write().await;
        config.remote_access.enabled = false;
        if let Err(e) = config.save() {
            tracing::warn!("failed to save config: {e}");
        }
    }

    Ok(Json(json!({
        "enabled": false,
        "url": null,
        "status": "stopped",
    })))
}
