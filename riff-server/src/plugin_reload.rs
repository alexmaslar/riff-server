use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use riff_core::plugin::wasm_editorial::WasmEditorialProvider;
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
                &state.http_client,
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
                        state.http_client.clone(),
                    )));
                }
                if instance.capabilities().contains(&Capability::Editorial) {
                    registry.register_editorial(Arc::new(WasmEditorialProvider::new(
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

/// Load all dev plugins from config `dev_plugins` paths.
///
/// For each dev plugin entry:
/// - Reads manifest.json + plugin.wasm from the specified directory.
/// - Loads into the registry using the same WASM host as catalog plugins.
/// - Returns a map of plugin_id -> load result, plus the set of dev plugin names.
pub async fn reload_dev_plugins(
    state: &AppState,
) -> (HashMap<String, PluginLoadResult>, HashSet<String>) {
    let config = state.config.read().await;
    let dev_entries: Vec<(String, std::path::PathBuf)> = config
        .dev_plugins
        .iter()
        .map(|(name, dc)| (name.clone(), dc.path.clone()))
        .collect();
    let plugin_configs = config.plugins.clone();
    drop(config);

    let mut results = HashMap::new();
    let mut dev_names = HashSet::new();

    for (plugin_id, dev_path) in dev_entries {
        dev_names.insert(plugin_id.clone());
        let result = load_dev_plugin_from_path(
            state,
            &plugin_id,
            &dev_path,
            plugin_configs.get(&plugin_id),
        )
        .await;
        results.insert(plugin_id, result);
    }

    (results, dev_names)
}

/// Reload a single dev plugin by name. Returns an error result if the
/// plugin is not in the `dev_plugins` config section.
pub async fn reload_single_dev_plugin(
    state: &AppState,
    name: &str,
) -> PluginLoadResult {
    let config = state.config.read().await;
    let dev_config = config.dev_plugins.get(name).cloned();
    let plugin_config = config.plugins.get(name).cloned();
    drop(config);

    let Some(dev_config) = dev_config else {
        return PluginLoadResult {
            loaded: false,
            healthy: false,
            message: Some(format!("'{name}' is not a dev plugin")),
        };
    };

    // Unregister first for clean reload
    {
        let mut registry = state.plugin_registry.write().await;
        registry.unregister_by_name(name);
    }

    load_dev_plugin_from_path(state, name, &dev_config.path, plugin_config.as_ref()).await
}

/// Shared logic: read WASM + manifest from a directory, load into registry.
async fn load_dev_plugin_from_path(
    state: &AppState,
    plugin_id: &str,
    dev_path: &std::path::Path,
    plugin_config: Option<&riff_core::config::PluginConfig>,
) -> PluginLoadResult {
    let should_be_enabled = plugin_config.map(|p| p.enabled).unwrap_or(false);
    if !should_be_enabled {
        tracing::debug!("dev plugin {plugin_id} not enabled in config, skipping");
        return PluginLoadResult {
            loaded: false,
            healthy: false,
            message: Some("Not enabled in plugins config".to_string()),
        };
    }

    // Unregister existing (clean slate)
    {
        let mut registry = state.plugin_registry.write().await;
        registry.unregister_by_name(plugin_id);
    }

    let wasm_path = dev_path.join("plugin.wasm");
    let manifest_path = dev_path.join("manifest.json");

    let Ok(wasm_bytes) = std::fs::read(&wasm_path) else {
        let msg = format!("Failed to read {}", wasm_path.display());
        tracing::warn!("dev plugin {plugin_id}: {msg}");
        return PluginLoadResult {
            loaded: false,
            healthy: false,
            message: Some(msg),
        };
    };

    let Ok(manifest_str) = std::fs::read_to_string(&manifest_path) else {
        let msg = format!("Failed to read {}", manifest_path.display());
        tracing::warn!("dev plugin {plugin_id}: {msg}");
        return PluginLoadResult {
            loaded: false,
            healthy: false,
            message: Some(msg),
        };
    };

    let Ok(manifest) = serde_json::from_str(&manifest_str) else {
        tracing::warn!("dev plugin {plugin_id}: failed to parse manifest");
        return PluginLoadResult {
            loaded: false,
            healthy: false,
            message: Some("Failed to parse manifest.json".to_string()),
        };
    };

    let settings = plugin_config
        .map(|pc| pc.settings.clone())
        .unwrap_or_default();

    match WasmPluginInstance::load(&wasm_bytes, manifest, settings) {
        Ok(instance) => {
            let instance = Arc::new(instance);

            let health = instance.health_check().await;
            let healthy = health.healthy;
            let health_msg = health.message;
            if !healthy {
                let msg = health_msg
                    .as_deref()
                    .unwrap_or("Health check failed");
                tracing::warn!("dev plugin {plugin_id} loaded but unhealthy: {msg}");
            }

            let mut registry = state.plugin_registry.write().await;
            if instance.capabilities().contains(&Capability::Streaming) {
                registry.register_streaming(Arc::new(WasmStreamingProvider::new(
                    instance.clone(),
                    state.http_client.clone(),
                )));
            }
            if instance.capabilities().contains(&Capability::Editorial) {
                registry.register_editorial(Arc::new(WasmEditorialProvider::new(
                    instance.clone(),
                )));
            }
            registry.register_base(instance);
            tracing::info!("loaded dev plugin: {plugin_id} (from {})", dev_path.display());

            PluginLoadResult {
                loaded: true,
                healthy,
                message: if healthy { None } else { health_msg },
            }
        }
        Err(e) => {
            tracing::warn!("failed to load dev plugin {plugin_id}: {e}");
            PluginLoadResult {
                loaded: false,
                healthy: false,
                message: Some(format!("Failed to load: {e}")),
            }
        }
    }
}
