use std::collections::HashMap;
use std::sync::Arc;

use riff_core::plugin::wasm_host::WasmPluginInstance;
use riff_core::plugin::wasm_streaming::WasmStreamingProvider;
use riff_core::plugin::{Capability, Plugin as _};
use serde::Serialize;

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct PluginLoadResult {
    pub loaded: bool,
    pub healthy: bool,
    pub message: Option<String>,
}

/// Reload all WASM plugins based on the remote catalog and current config.
///
/// For each WASM plugin in the remote catalog:
/// - Unregisters it from the registry (clean slate).
/// - If enabled in config: downloads if not installed, then loads into the registry.
/// - If disabled: just leaves it unregistered.
///
/// Returns a map of plugin_id -> load/health result for enabled plugins.
pub async fn reload_wasm_plugins(state: &AppState) -> HashMap<String, PluginLoadResult> {
    let config = state.config.read().await;
    let plugin_dir = config.plugin_directory();
    let remote_catalog = state.remote_catalog.read().await;

    let mut registry = state.plugin_registry.write().await;
    let mut results = HashMap::new();

    for entry in remote_catalog.iter() {
        let plugin_id = &entry.name;

        let plugin_config = config.plugins.get(plugin_id.as_str());
        let should_be_enabled = plugin_config.map(|p| p.enabled).unwrap_or(false);
        let is_installed = plugin_dir.join(plugin_id).join("plugin.wasm").exists();

        // Always unregister first (clean slate for this plugin)
        registry.unregister_by_name(plugin_id);

        if !should_be_enabled {
            continue;
        }

        // Download if not installed
        if !is_installed {
            match riff_core::plugin::wasm_loader::download_wasm_plugin(
                plugin_id,
                &entry.wasm_url,
                &entry.manifest_url,
                &plugin_dir,
            )
            .await
            {
                Ok(()) => tracing::info!("downloaded wasm plugin: {plugin_id}"),
                Err(e) => {
                    tracing::warn!("failed to download wasm plugin {plugin_id}: {e}");
                    results.insert(plugin_id.clone(), PluginLoadResult {
                        loaded: false,
                        healthy: false,
                        message: Some(format!("Download failed: {e}")),
                    });
                    continue;
                }
            }
        }

        // Load from disk
        let wasm_path = plugin_dir.join(plugin_id).join("plugin.wasm");
        let manifest_path = plugin_dir.join(plugin_id).join("manifest.json");
        let Ok(wasm_bytes) = std::fs::read(&wasm_path) else {
            tracing::warn!("failed to read wasm bytes for {plugin_id}");
            results.insert(plugin_id.clone(), PluginLoadResult {
                loaded: false,
                healthy: false,
                message: Some("Failed to read plugin file".to_string()),
            });
            continue;
        };
        let Ok(manifest_str) = std::fs::read_to_string(&manifest_path) else {
            tracing::warn!("failed to read manifest for {plugin_id}");
            results.insert(plugin_id.clone(), PluginLoadResult {
                loaded: false,
                healthy: false,
                message: Some("Failed to read manifest".to_string()),
            });
            continue;
        };
        let Ok(manifest) = serde_json::from_str(&manifest_str) else {
            tracing::warn!("failed to parse manifest for {plugin_id}");
            results.insert(plugin_id.clone(), PluginLoadResult {
                loaded: false,
                healthy: false,
                message: Some("Failed to parse manifest".to_string()),
            });
            continue;
        };

        let settings = plugin_config
            .map(|pc| pc.settings.clone())
            .unwrap_or_default();

        match WasmPluginInstance::load(&wasm_bytes, manifest, settings) {
            Ok(instance) => {
                let instance = Arc::new(instance);

                // Run health check before registering
                let health = instance.health_check().await;
                if !health.healthy {
                    let msg = health.message.unwrap_or_else(|| "Health check failed".to_string());
                    tracing::warn!("plugin {plugin_id} loaded but unhealthy: {msg}");
                    results.insert(plugin_id.clone(), PluginLoadResult {
                        loaded: true,
                        healthy: false,
                        message: Some(msg),
                    });
                }

                if instance.capabilities().contains(&Capability::Streaming) {
                    registry.register_streaming(Arc::new(WasmStreamingProvider::new(
                        instance.clone(),
                    )));
                }
                registry.register_base(instance);
                tracing::info!("hot-loaded wasm plugin: {plugin_id}");

                // Only insert healthy result if we didn't already insert unhealthy
                results.entry(plugin_id.clone()).or_insert(PluginLoadResult {
                    loaded: true,
                    healthy: true,
                    message: None,
                });
            }
            Err(e) => {
                tracing::warn!("failed to load wasm plugin {plugin_id}: {e}");
                results.insert(plugin_id.clone(), PluginLoadResult {
                    loaded: false,
                    healthy: false,
                    message: Some(format!("Failed to load: {e}")),
                });
            }
        }
    }

    results
}
