use axum::{extract::State, http::header, Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::middleware::ClientIp;
use crate::AppState;

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
            "streaming": {
                "remote_bitrate": config.streaming.remote_bitrate,
                "remote_format": config.streaming.remote_format,
                "ffmpeg_available": state.ffmpeg_available,
            },
            "plugins": {},
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ConfigUpdate {
    pub server: Option<ServerUpdate>,
    pub library: Option<LibraryUpdate>,
    pub streaming: Option<StreamingUpdate>,
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

pub async fn update_config(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Extension(client_ip): Extension<ClientIp>,
    Json(update): Json<ConfigUpdate>,
) -> Result<Json<Value>, AppError> {
    let (config_snapshot, https_port_changed) = {
        let mut config = state.config.write().await;

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

        let snapshot = config.clone();
        (snapshot, https_port_changed)
    };

    config_snapshot.save().map_err(|e| AppError::Internal(format!("failed to save config: {e}")))?;

    // Audit log
    {
        let details = if https_port_changed {
            format!("config updated: https_port={}", config_snapshot.server.https_port)
        } else {
            "config updated".to_string()
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
        tracing::info!(port = config_snapshot.server.https_port, "HTTPS port changed, restarting server");
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
        "streaming": {
            "remote_bitrate": config_snapshot.streaming.remote_bitrate,
            "remote_format": config_snapshot.streaming.remote_format,
            "ffmpeg_available": state.ffmpeg_available,
        },
        "plugins": {},
    });

    Ok(Json(response))
}
