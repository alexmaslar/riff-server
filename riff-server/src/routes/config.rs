use axum::{extract::State, http::header, Extension, Json};
use riff_core::auth::Claims;
use riff_core::config::AiProvider;
use riff_core::plugin::catalog::RemotePluginEntry;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::AppError;
use crate::middleware::ClientIp;
use crate::AppState;

fn mask_secret(s: &str) -> String {
    if s.len() <= 6 {
        "*".repeat(s.len())
    } else {
        format!("{}...{}", &s[..3], &s[s.len() - 3..])
    }
}

fn provider_to_str(p: &AiProvider) -> &'static str {
    match p {
        AiProvider::OpenAi => "openai",
        AiProvider::Anthropic => "anthropic",
        AiProvider::Ollama => "ollama",
    }
}

pub async fn get_config(
    State(state): State<Arc<AppState>>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let config = state.config.read().await;
    let remote_catalog = state.remote_catalog.read().await;

    // Build plugins JSON with secret masking
    let plugins_json = build_plugins_json(&config.plugins, &remote_catalog);

    Ok((
        [(header::CACHE_CONTROL, "private, max-age=3600")],
        Json(json!({
            "server": {
                "https_port": config.server.https_port,
            },
            "library": {
                "path": config.library.path,
                "scan_interval": config.library.scan_interval,
            },
            "metadata": {
                "enrichment": {
                    "auto_enrich": config.metadata.enrichment.auto_enrich,
                    "download_covers": config.metadata.enrichment.download_covers,
                },
                "ai": {
                    "enabled": config.metadata.ai.enabled,
                    "provider": provider_to_str(&config.metadata.ai.provider),
                    "api_key": config.metadata.ai.api_key.as_deref().map(mask_secret),
                    "model": config.metadata.ai.model,
                    "fast_model": config.metadata.ai.fast_model,
                    "base_url": config.metadata.ai.base_url,
                    "playlist_generation": config.metadata.ai.playlist_generation,
                },
                "discogs_api_key": config.metadata.discogs_api_key.as_deref().map(mask_secret),
            },
            "streaming": {
                "remote_bitrate": config.streaming.remote_bitrate,
                "remote_format": config.streaming.remote_format,
                "ffmpeg_available": state.ffmpeg_available,
            },
            "plugins": plugins_json,
        })),
    ))
}

/// Build plugins JSON from config, masking secret fields using the remote catalog.
/// API format nests settings under a "settings" key (unlike the flat YAML format).
fn build_plugins_json(
    plugins: &HashMap<String, riff_core::config::PluginConfig>,
    remote_catalog: &[RemotePluginEntry],
) -> Value {
    // Build lookup: plugin_name -> set of secret field keys
    let secret_keys: HashMap<&str, Vec<String>> = remote_catalog
        .iter()
        .map(|entry| (entry.name.as_str(), entry.secret_keys()))
        .collect();

    let mut result = serde_json::Map::new();
    for (name, plugin_cfg) in plugins {
        let mut settings = serde_json::Map::new();
        let is_secret_field = |key: &str| -> bool {
            secret_keys
                .get(name.as_str())
                .map(|keys| keys.iter().any(|k| k == key))
                .unwrap_or(false)
        };
        for (key, value) in &plugin_cfg.settings {
            if is_secret_field(key) {
                if let Some(s) = value.as_str() {
                    if !s.is_empty() {
                        settings.insert(key.clone(), Value::String(mask_secret(s)));
                    }
                }
            } else {
                settings.insert(key.clone(), value.clone());
            }
        }
        result.insert(
            name.clone(),
            json!({
                "enabled": plugin_cfg.enabled,
                "settings": settings,
            }),
        );
    }
    Value::Object(result)
}

#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub server: Option<ServerUpdate>,
    pub library: Option<LibraryUpdate>,
    pub metadata: Option<MetadataUpdate>,
    pub streaming: Option<StreamingUpdate>,
    pub plugins: Option<HashMap<String, PluginUpdate>>,
}

#[derive(Debug, Deserialize)]
pub struct PluginUpdate {
    pub enabled: Option<bool>,
    pub settings: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct StreamingUpdate {
    pub remote_bitrate: Option<u32>,
    pub remote_format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerUpdate {
    pub https_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct LibraryUpdate {
    pub path: Option<String>,
    pub scan_interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct MetadataUpdate {
    pub enrichment: Option<EnrichmentUpdate>,
    pub ai: Option<AiUpdate>,
    pub discogs_api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EnrichmentUpdate {
    pub auto_enrich: Option<bool>,
    pub download_covers: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AiUpdate {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub fast_model: Option<String>,
    pub base_url: Option<Option<String>>,
    pub playlist_generation: Option<bool>,
}

pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Extension(client_ip): Extension<ClientIp>,
    Json(update): Json<ConfigUpdate>,
) -> Result<Json<Value>, AppError> {
    // Clone config under write lock, apply mutations, then drop lock before disk I/O
    let (config_snapshot, any_newly_enabled, https_port_changed, discogs_key_changed, plugins_changed) = {
        let mut config = state.config.write().await;

        // Snapshot AI flags before mutations
        let old_ai_enabled = config.metadata.ai.enabled;

        // Snapshot Discogs key before mutations
        let old_discogs_key = config.metadata.discogs_api_key.clone();

        let mut https_port_changed = false;
        if let Some(srv) = update.server {
            if let Some(port) = srv.https_port {
                if port != config.server.https_port {
                    config.server.https_port = port;
                    https_port_changed = true;
                }
            }
        }

        if let Some(lib) = update.library {
            if let Some(path) = lib.path {
                config.library.path = if path.is_empty() { None } else { Some(path) };
            }
            if let Some(interval) = lib.scan_interval {
                config.library.scan_interval = interval;
            }
        }

        if let Some(meta) = update.metadata {
            if let Some(enrichment) = meta.enrichment {
                if let Some(auto_enrich) = enrichment.auto_enrich {
                    config.metadata.enrichment.auto_enrich = auto_enrich;
                }
                if let Some(download_covers) = enrichment.download_covers {
                    config.metadata.enrichment.download_covers = download_covers;
                }
            }

            if let Some(key) = meta.discogs_api_key {
                config.metadata.discogs_api_key = if key.is_empty() { None } else { Some(key) };
            }

            if let Some(ai) = meta.ai {
                if let Some(enabled) = ai.enabled {
                    config.metadata.ai.enabled = enabled;
                }
                if let Some(provider_str) = ai.provider {
                    config.metadata.ai.provider = match provider_str.as_str() {
                        "anthropic" => AiProvider::Anthropic,
                        "ollama" => AiProvider::Ollama,
                        _ => AiProvider::OpenAi,
                    };
                }
                if let Some(key) = ai.api_key {
                    config.metadata.ai.api_key = if key.is_empty() { None } else { Some(key) };
                }
                if let Some(model) = ai.model {
                    config.metadata.ai.model = if model.is_empty() { None } else { Some(model) };
                }
                if let Some(fast_model) = ai.fast_model {
                    config.metadata.ai.fast_model = if fast_model.is_empty() { None } else { Some(fast_model) };
                }
                if let Some(base_url) = ai.base_url {
                    config.metadata.ai.base_url = base_url.filter(|s| !s.is_empty());
                }
                if let Some(v) = ai.playlist_generation {
                    config.metadata.ai.playlist_generation = v;
                }
            }

        }

        // Plugins
        let mut plugins_changed = false;
        if let Some(plugin_updates) = update.plugins {
            for (name, pu) in plugin_updates {
                let entry = config.plugins.entry(name).or_insert_with(|| {
                    riff_core::config::PluginConfig {
                        enabled: false,
                        settings: HashMap::new(),
                    }
                });
                if let Some(enabled) = pu.enabled {
                    if enabled != entry.enabled {
                        entry.enabled = enabled;
                        plugins_changed = true;
                    }
                }
                if let Some(new_settings) = pu.settings {
                    for (key, value) in new_settings {
                        // Empty string removes the key
                        if value.as_str() == Some("") {
                            if entry.settings.remove(&key).is_some() {
                                plugins_changed = true;
                            }
                        } else if entry.settings.get(&key) != Some(&value) {
                            entry.settings.insert(key, value);
                            plugins_changed = true;
                        }
                    }
                }
            }
        }

        if let Some(streaming) = update.streaming {
            if let Some(bitrate) = streaming.remote_bitrate {
                config.streaming.remote_bitrate = bitrate.clamp(64, 320);
            }
            if let Some(format) = streaming.remote_format {
                if format == "aac" || format == "opus" {
                    config.streaming.remote_format = format;
                }
            }
        }

        let discogs_key_changed = match (&old_discogs_key, &config.metadata.discogs_api_key) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) if old != new => true,
            _ => false,
        };

        let any_newly_enabled = !old_ai_enabled && config.metadata.ai.enabled;

        if any_newly_enabled {
            tracing::info!("AI features enabled");
        }

        let snapshot = config.clone();
        (snapshot, any_newly_enabled, https_port_changed, discogs_key_changed, plugins_changed)
    }; // write lock dropped before disk I/O

    // Save to disk outside the lock
    config_snapshot.save().map_err(|e| AppError::Internal(format!("failed to save config: {e}")))?;

    // Audit log
    {
        let mut changed = Vec::new();
        if https_port_changed {
            changed.push(format!("https_port={}", config_snapshot.server.https_port));
        }
        if any_newly_enabled {
            changed.push("ai_features_enabled".to_string());
        }
        if discogs_key_changed {
            changed.push("discogs_api_key".to_string());
        }
        if plugins_changed {
            changed.push("plugins".to_string());
        }
        let details = if changed.is_empty() {
            "config updated".to_string()
        } else {
            format!("config updated: {}", changed.join(", "))
        };
        super::audit::log(
            &state.db,
            &claims.sub,
            &claims.username,
            "config_update",
            Some(&details),
            Some(&client_ip.0),
        )
        .await;
    }

    if https_port_changed {
        tracing::info!(
            "HTTPS port changed to {} — restarting server",
            config_snapshot.server.https_port
        );
        // Signal restart after a short delay so the response flushes first
        let restart_state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            restart_state.restart.notify_one();
        });
    }

    let remote_catalog = state.remote_catalog.read().await;
    let plugins_json = build_plugins_json(&config_snapshot.plugins, &remote_catalog);
    drop(remote_catalog);

    // Reload plugins synchronously so we can report health in the response
    let plugin_results = if plugins_changed {
        let mut results = crate::plugin_reload::reload_wasm_plugins(&state).await;
        // Also reload dev plugins when plugin config changes
        let (dev_results, dev_names) = crate::plugin_reload::reload_dev_plugins(&state).await;
        *state.dev_plugin_names.write().await = dev_names;
        results.extend(dev_results);
        Some(results)
    } else {
        None
    };

    let response = json!({
        "restarting": https_port_changed,
        "server": {
            "https_port": config_snapshot.server.https_port,
        },
        "library": {
            "path": config_snapshot.library.path,
            "scan_interval": config_snapshot.library.scan_interval,
        },
        "metadata": {
            "enrichment": {
                "auto_enrich": config_snapshot.metadata.enrichment.auto_enrich,
                "download_covers": config_snapshot.metadata.enrichment.download_covers,
            },
            "ai": {
                "enabled": config_snapshot.metadata.ai.enabled,
                "provider": provider_to_str(&config_snapshot.metadata.ai.provider),
                "api_key": config_snapshot.metadata.ai.api_key.as_deref().map(mask_secret),
                "model": config_snapshot.metadata.ai.model,
                "fast_model": config_snapshot.metadata.ai.fast_model,
                "base_url": config_snapshot.metadata.ai.base_url,
                "playlist_generation": config_snapshot.metadata.ai.playlist_generation,
            },
            "discogs_api_key": config_snapshot.metadata.discogs_api_key.as_deref().map(mask_secret),
        },
        "streaming": {
            "remote_bitrate": config_snapshot.streaming.remote_bitrate,
            "remote_format": config_snapshot.streaming.remote_format,
            "ffmpeg_available": state.ffmpeg_available,
        },
        "plugins": plugins_json,
        "plugin_health": plugin_results,
    });

    // AI was enabled — no automatic content generation since editorial enrichment is used instead
    let _ = any_newly_enabled;

    if discogs_key_changed {
        if let Some(ref api_key) = config_snapshot.metadata.discogs_api_key {
            let api_key = api_key.clone();
            let db = state.db.clone();
            tokio::spawn(async move {
                // Clear all artist images so Discogs can re-fill them
                match sqlx::query("UPDATE artists SET image_url = NULL WHERE image_url IS NOT NULL")
                    .execute(&db)
                    .await
                {
                    Ok(r) => tracing::info!(
                        "cleared {} artist images for Discogs re-fetch",
                        r.rows_affected()
                    ),
                    Err(e) => {
                        tracing::warn!("failed to clear artist images: {e}");
                        return;
                    }
                }
                // Run Discogs enrichment with the new key
                match riff_core::musicbrainz::enrich_artist_images_discogs(&db, &api_key).await {
                    Ok(ids) => tracing::info!("discogs re-enrichment: {} artists updated", ids.len()),
                    Err(e) => tracing::warn!("discogs re-enrichment failed: {e}"),
                }
                // Deezer fallback for any remaining NULL image_urls
                match riff_core::musicbrainz::enrich_artist_top_tracks(&db).await {
                    Ok(ids) => tracing::info!("deezer fallback after discogs refresh: {} artists", ids.len()),
                    Err(e) => tracing::warn!("deezer fallback failed: {e}"),
                }
            });
        }
    }

    Ok(Json(response))
}
