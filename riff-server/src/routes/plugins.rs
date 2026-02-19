use axum::extract::State;
use axum::Json;
use riff_core::plugin::catalog::plugin_catalog;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// GET /plugins/status — list loaded plugins + health
pub async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let registry = state.plugin_registry.read().await;
    let mut plugins = Vec::new();
    for p in registry.all_plugins() {
        let health = p.health_check().await;
        plugins.push(json!({
            "name": p.name(),
            "display_name": p.display_name(),
            "version": p.version(),
            "capabilities": p.capabilities(),
            "healthy": health.healthy,
            "message": health.message,
        }));
    }
    Ok(Json(json!({ "plugins": plugins })))
}

/// GET /plugins/catalog — static catalog of known plugins + their config schemas
pub async fn catalog() -> Json<Value> {
    let catalog = plugin_catalog();
    Json(json!({ "plugins": catalog }))
}
