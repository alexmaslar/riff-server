use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

use crate::error::AppError;
use crate::AppState;

/// GET /plugins/status — list loaded plugins + health
pub async fn status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let dev_names = state.dev_plugin_names.read().await;
    let registry = state.plugin_registry.read().await;
    let mut plugins = Vec::new();
    for p in registry.all_plugins() {
        let is_dev = dev_names.contains(p.name());
        let health = p.health_check().await;
        plugins.push(json!({
            "name": p.name(),
            "display_name": p.display_name(),
            "version": p.version(),
            "capabilities": p.capabilities(),
            "healthy": health.healthy,
            "message": health.message,
            "dev": is_dev,
        }));
    }
    Ok(Json(json!({ "plugins": plugins })))
}

/// GET /plugins/catalog — all known plugins from the community registry,
/// plus dev plugins loaded from local paths.
/// Compiled-in plugins (loaded in registry without a WASM file) get `wasm: false`.
/// WASM plugins get `wasm: true` with `installed` based on whether the file exists on disk.
pub async fn catalog(State(state): State<Arc<AppState>>) -> Json<Value> {
    let config = state.config.read().await;
    let plugin_dir = config.plugin_directory();
    let dev_entries = config.dev_plugins.clone();
    drop(config);

    // Collect names of plugins currently loaded in the registry
    let registry = state.plugin_registry.read().await;
    let loaded_names: HashSet<String> = registry
        .all_plugins()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    drop(registry);

    let dev_names = state.dev_plugin_names.read().await;

    let remote = state.remote_catalog.read().await;
    let mut plugins: Vec<Value> = remote
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
                "dev": false,
            })
        })
        .collect();

    // Append dev plugins by reading their manifest.json
    for (name, dev_config) in &dev_entries {
        // Skip if already in the remote catalog
        if remote.iter().any(|e| e.name == *name) {
            continue;
        }
        let manifest_path = dev_config.path.join("manifest.json");
        if let Ok(manifest_str) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_str) {
                plugins.push(json!({
                    "name": name,
                    "display_name": manifest.get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(name),
                    "description": manifest.get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    "capabilities": manifest.get("capabilities")
                        .unwrap_or(&json!([])),
                    "settings": manifest.get("settings")
                        .unwrap_or(&json!([])),
                    "wasm": true,
                    "installed": dev_names.contains(name),
                    "dev": true,
                }));
            }
        }
    }

    Json(json!({ "plugins": plugins }))
}

/// POST /plugins/{name}/reload — reload a dev plugin from its local path.
/// Admin-only. Only works for plugins listed in dev_plugins config.
pub async fn reload_dev_plugin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let dev_names = state.dev_plugin_names.read().await;
    if !dev_names.contains(&name) {
        return Err(AppError::BadRequest(format!(
            "'{name}' is not a dev plugin"
        )));
    }
    drop(dev_names);

    let result = crate::plugin_reload::reload_single_dev_plugin(&state, &name).await;

    Ok(Json(json!({
        "name": name,
        "loaded": result.loaded,
        "healthy": result.healthy,
        "message": result.message,
    })))
}

/// GET /plugins/{name}/icon — serve the plugin's icon image.
/// Looks for icon.png, icon.jpg, or icon.webp in the plugin directory.
/// Also checks dev plugin paths.
pub async fn get_plugin_icon(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let config = state.config.read().await;
    let plugin_dir = config.plugin_directory();
    let dev_path = config.dev_plugins.get(&name).map(|d| d.path.clone());
    drop(config);

    let candidates = [
        ("image/png", "icon.png"),
        ("image/jpeg", "icon.jpg"),
        ("image/webp", "icon.webp"),
    ];

    // Check the installed plugin directory first, then dev plugin path
    let search_dirs: Vec<std::path::PathBuf> = std::iter::once(plugin_dir.join(&name))
        .chain(dev_path)
        .collect();

    for dir in &search_dirs {
        for (content_type, filename) in &candidates {
            let path = dir.join(filename);
            if path.exists() {
                if let Ok(data) = tokio::fs::read(&path).await {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, *content_type)
                        .header(header::CACHE_CONTROL, "public, max-age=86400")
                        .body(Body::from(data))
                        .expect("response builder with valid headers");
                }
            }
        }
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("response builder with valid headers")
}
