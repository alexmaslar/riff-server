use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;
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

/// GET /plugins/catalog — all known plugins from the community registry.
/// Compiled-in plugins (loaded in registry without a WASM file) get `wasm: false`.
/// WASM plugins get `wasm: true` with `installed` based on whether the file exists on disk.
pub async fn catalog(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    let plugin_dir = config.plugin_directory();
    drop(config);

    // Collect names of plugins currently loaded in the registry
    let registry = state.plugin_registry.read().await;
    let loaded_names: HashSet<String> = registry
        .all_plugins()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    drop(registry);

    let remote = state.remote_catalog.read().await;
    let plugins: Vec<Value> = remote
        .iter()
        .map(|entry| {
            let wasm_on_disk = plugin_dir.join(&entry.name).join("plugin.wasm").exists();
            let is_compiled_in = loaded_names.contains(&entry.name) && !wasm_on_disk;

            let (wasm, installed) = if is_compiled_in {
                (false, true)
            } else {
                (true, wasm_on_disk)
            };

            json!({
                "name": entry.name,
                "display_name": entry.display_name,
                "description": entry.description,
                "capabilities": entry.capabilities,
                "settings": entry.settings,
                "wasm": wasm,
                "installed": installed,
            })
        })
        .collect();

    Json(json!({ "plugins": plugins }))
}
