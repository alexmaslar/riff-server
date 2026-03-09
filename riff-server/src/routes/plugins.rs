use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// GET /plugins/status — list editorial providers
pub async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let registry = state.plugin_registry.read().await;
    let editorial: Vec<Value> = registry
        .editorial_providers()
        .iter()
        .map(|p| {
            json!({
                "name": p.provider_name(),
                "type": "editorial",
            })
        })
        .collect();
    Ok(Json(json!({ "providers": editorial })))
}

/// GET /plugins/catalog — list available editorial providers (all built-in)
pub async fn catalog(State(state): State<Arc<AppState>>) -> Json<Value> {
    let registry = state.plugin_registry.read().await;
    let plugins: Vec<Value> = registry
        .editorial_providers()
        .iter()
        .map(|p| {
            json!({
                "name": p.provider_name(),
                "type": "editorial",
                "builtin": true,
            })
        })
        .collect();
    Json(json!({ "plugins": plugins }))
}

/// GET /plugins/{name}/icon — serve the plugin's icon image.
pub async fn get_plugin_icon(
    State(_state): State<Arc<AppState>>,
    Path(_name): Path<String>,
) -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("response builder with valid headers")
}
