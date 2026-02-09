use axum::{extract::State, http::header, Json};
use riff_core::config::AiProvider;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
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
                "discogs": {
                    "api_token": config.metadata.discogs.api_token.as_deref().map(mask_secret),
                    "auto_enrich": config.metadata.discogs.auto_enrich,
                    "download_covers": config.metadata.discogs.download_covers,
                },
                "ai": {
                    "enabled": config.metadata.ai.enabled,
                    "provider": provider_to_str(&config.metadata.ai.provider),
                    "api_key": config.metadata.ai.api_key.as_deref().map(mask_secret),
                    "model": config.metadata.ai.model,
                    "fast_model": config.metadata.ai.fast_model,
                    "base_url": config.metadata.ai.base_url,
                    "album_summaries": config.metadata.ai.album_summaries,
                    "album_ratings": config.metadata.ai.album_ratings,
                    "album_recommendations": config.metadata.ai.album_recommendations,
                    "artist_bios": config.metadata.ai.artist_bios,
                    "artist_recommendations": config.metadata.ai.artist_recommendations,
                    "playlist_generation": config.metadata.ai.playlist_generation,
                }
            }
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub server: Option<ServerUpdate>,
    pub library: Option<LibraryUpdate>,
    pub metadata: Option<MetadataUpdate>,
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
    pub discogs: Option<DiscogsUpdate>,
    pub ai: Option<AiUpdate>,
}

#[derive(Debug, Deserialize)]
pub struct DiscogsUpdate {
    pub api_token: Option<String>,
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
    pub album_summaries: Option<bool>,
    pub album_ratings: Option<bool>,
    pub album_recommendations: Option<bool>,
    pub artist_bios: Option<bool>,
    pub artist_recommendations: Option<bool>,
    pub playlist_generation: Option<bool>,
}

pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> Result<Json<Value>, AppError> {
    // Clone config under write lock, apply mutations, then drop lock before disk I/O
    let (config_snapshot, any_newly_enabled, https_port_changed) = {
        let mut config = state.config.write().await;

        // Snapshot AI flags before mutations
        let old_ai_enabled = config.metadata.ai.enabled;
        let old_album_summaries = config.metadata.ai.album_summaries;
        let old_album_ratings = config.metadata.ai.album_ratings;
        let old_album_recommendations = config.metadata.ai.album_recommendations;
        let old_artist_bios = config.metadata.ai.artist_bios;
        let old_artist_recommendations = config.metadata.ai.artist_recommendations;

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
            if let Some(discogs) = meta.discogs {
                if let Some(token) = discogs.api_token {
                    config.metadata.discogs.api_token = if token.is_empty() {
                        None
                    } else {
                        Some(token)
                    };
                }
                if let Some(auto_enrich) = discogs.auto_enrich {
                    config.metadata.discogs.auto_enrich = auto_enrich;
                }
                if let Some(download_covers) = discogs.download_covers {
                    config.metadata.discogs.download_covers = download_covers;
                }
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
                if let Some(v) = ai.album_summaries {
                    config.metadata.ai.album_summaries = v;
                }
                if let Some(v) = ai.album_ratings {
                    config.metadata.ai.album_ratings = v;
                }
                if let Some(v) = ai.album_recommendations {
                    config.metadata.ai.album_recommendations = v;
                }
                if let Some(v) = ai.artist_bios {
                    config.metadata.ai.artist_bios = v;
                }
                if let Some(v) = ai.artist_recommendations {
                    config.metadata.ai.artist_recommendations = v;
                }
                if let Some(v) = ai.playlist_generation {
                    config.metadata.ai.playlist_generation = v;
                }
            }
        }

        let any_newly_enabled =
            (!old_ai_enabled && config.metadata.ai.enabled) ||
            (!old_album_summaries && config.metadata.ai.album_summaries) ||
            (!old_album_ratings && config.metadata.ai.album_ratings) ||
            (!old_album_recommendations && config.metadata.ai.album_recommendations) ||
            (!old_artist_bios && config.metadata.ai.artist_bios) ||
            (!old_artist_recommendations && config.metadata.ai.artist_recommendations);

        if any_newly_enabled {
            let mut enabled = Vec::new();
            if !old_ai_enabled && config.metadata.ai.enabled { enabled.push("AI"); }
            if !old_album_summaries && config.metadata.ai.album_summaries { enabled.push("album summaries"); }
            if !old_album_ratings && config.metadata.ai.album_ratings { enabled.push("album ratings"); }
            if !old_album_recommendations && config.metadata.ai.album_recommendations { enabled.push("album recommendations"); }
            if !old_artist_bios && config.metadata.ai.artist_bios { enabled.push("artist bios"); }
            if !old_artist_recommendations && config.metadata.ai.artist_recommendations { enabled.push("artist recommendations"); }
            tracing::info!("AI features enabled: {}, starting generation", enabled.join(", "));
        }

        let snapshot = config.clone();
        (snapshot, any_newly_enabled, https_port_changed)
    }; // write lock dropped before disk I/O

    // Save to disk outside the lock
    config_snapshot.save().map_err(|e| AppError::Internal(format!("failed to save config: {e}")))?;

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
            "discogs": {
                "api_token": config_snapshot.metadata.discogs.api_token.as_deref().map(mask_secret),
                "auto_enrich": config_snapshot.metadata.discogs.auto_enrich,
                "download_covers": config_snapshot.metadata.discogs.download_covers,
            },
            "ai": {
                "enabled": config_snapshot.metadata.ai.enabled,
                "provider": provider_to_str(&config_snapshot.metadata.ai.provider),
                "api_key": config_snapshot.metadata.ai.api_key.as_deref().map(mask_secret),
                "model": config_snapshot.metadata.ai.model,
                "fast_model": config_snapshot.metadata.ai.fast_model,
                "base_url": config_snapshot.metadata.ai.base_url,
                "album_summaries": config_snapshot.metadata.ai.album_summaries,
                "album_ratings": config_snapshot.metadata.ai.album_ratings,
                "album_recommendations": config_snapshot.metadata.ai.album_recommendations,
                "artist_bios": config_snapshot.metadata.ai.artist_bios,
                "artist_recommendations": config_snapshot.metadata.ai.artist_recommendations,
                "playlist_generation": config_snapshot.metadata.ai.playlist_generation,
            }
        }
    });

    if any_newly_enabled {
        let spawn_state = state.clone();
        tokio::spawn(async move {
            super::library::maybe_spawn_summarization(&spawn_state).await;
        });
    }

    Ok(Json(response))
}
