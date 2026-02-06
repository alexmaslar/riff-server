use axum::{extract::State, Json};
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
) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;

    Ok(Json(json!({
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
                "base_url": config.metadata.ai.base_url,
                "album_summaries": config.metadata.ai.album_summaries,
                "album_ratings": config.metadata.ai.album_ratings,
                "album_recommendations": config.metadata.ai.album_recommendations,
                "artist_bios": config.metadata.ai.artist_bios,
                "artist_recommendations": config.metadata.ai.artist_recommendations,
            }
        }
    })))
}

#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub library: Option<LibraryUpdate>,
    pub metadata: Option<MetadataUpdate>,
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
    pub base_url: Option<Option<String>>,
    pub album_summaries: Option<bool>,
    pub album_ratings: Option<bool>,
    pub album_recommendations: Option<bool>,
    pub artist_bios: Option<bool>,
    pub artist_recommendations: Option<bool>,
}

pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Json(update): Json<ConfigUpdate>,
) -> Result<Json<Value>, AppError> {
    let (response, any_newly_enabled) = {
        let mut config = state.config.write().await;

        // Snapshot AI flags before mutations
        let old_ai_enabled = config.metadata.ai.enabled;
        let old_album_summaries = config.metadata.ai.album_summaries;
        let old_album_ratings = config.metadata.ai.album_ratings;
        let old_album_recommendations = config.metadata.ai.album_recommendations;
        let old_artist_bios = config.metadata.ai.artist_bios;
        let old_artist_recommendations = config.metadata.ai.artist_recommendations;

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
            }
        }

        config.save().map_err(|e| AppError::Internal(format!("failed to save config: {e}")))?;

        let response = json!({
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
                    "base_url": config.metadata.ai.base_url,
                    "album_summaries": config.metadata.ai.album_summaries,
                    "album_ratings": config.metadata.ai.album_ratings,
                    "album_recommendations": config.metadata.ai.album_recommendations,
                    "artist_bios": config.metadata.ai.artist_bios,
                    "artist_recommendations": config.metadata.ai.artist_recommendations,
                }
            }
        });

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

        (response, any_newly_enabled)
    }; // write lock dropped

    if any_newly_enabled {
        let spawn_state = state.clone();
        tokio::spawn(async move {
            super::library::maybe_spawn_summarization(&spawn_state).await;
        });
    }

    Ok(Json(response))
}
