use axum::{extract::State, Extension, Json};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::middleware::ClientIp;
use crate::AppState;

fn status_json(status: &crate::upnp::RemoteAccessStatus, https_port: u16) -> Value {
    let local_ip = local_ip_address::local_ip()
        .ok()
        .map(|ip| ip.to_string());
    json!({
        "enabled": status.enabled,
        "method": status.method,
        "status": status.status,
        "public_address": status.public_address,
        "external_url": status.external_url,
        "cert_fingerprint": status.cert_fingerprint,
        "error_message": status.error_message,
        "https_port": https_port,
        "local_ip": local_ip,
        "nat_type": status.nat_type,
        "port_reachable": status.port_reachable,
        "tailscale_fqdn": status.tailscale_fqdn,
    })
}

pub async fn get_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, AppError> {
    let status = state.remote_access.status.read().await;
    let https_port = state.config.read().await.server.https_port;
    Ok(Json(status_json(&status, https_port)))
}

pub async fn enable(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Extension(client_ip): Extension<ClientIp>,
) -> Result<Json<Value>, AppError> {
    let config = state.config.read().await;
    let external_url = config.remote_access.external_url.clone();
    let method = config.remote_access.method.clone();
    drop(config);

    if method == "tailscale" {
        // Tailscale listener is set up at startup — persist enabled and restart
        {
            let mut config = state.config.write().await;
            config.remote_access.enabled = true;
            if let Err(e) = config.save() {
                tracing::warn!("failed to save config: {e}");
            }
        }
        state.restart.notify_one();
    } else {
        // Start is best-effort — don't fail the request if remote access method isn't available
        let _ = state.remote_access.start(external_url, &method).await;
    }

    // Persist enabled state and cert fingerprint
    {
        let mut config = state.config.write().await;
        config.remote_access.enabled = true;
        let status = state.remote_access.status.read().await;
        config.remote_access.cert_fingerprint = status.cert_fingerprint.clone();
        if let Err(e) = config.save() {
            tracing::warn!("failed to save config: {e}");
        }
    }

    super::audit::log(
        &state.db,
        &claims.sub,
        &claims.username,
        "remote_access_enable",
        None,
        Some(&client_ip.0),
    )
    .await;

    let status = state.remote_access.status.read().await;
    let https_port = state.config.read().await.server.https_port;
    Ok(Json(status_json(&status, https_port)))
}

pub async fn disable(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Extension(client_ip): Extension<ClientIp>,
) -> Result<Json<Value>, AppError> {
    state.remote_access.stop().await;

    {
        let mut config = state.config.write().await;
        config.remote_access.enabled = false;
        if let Err(e) = config.save() {
            tracing::warn!("failed to save config: {e}");
        }
    }

    super::audit::log(
        &state.db,
        &claims.sub,
        &claims.username,
        "remote_access_disable",
        None,
        Some(&client_ip.0),
    )
    .await;

    let status = state.remote_access.status.read().await;
    let https_port = state.config.read().await.server.https_port;
    Ok(Json(status_json(&status, https_port)))
}

const VALID_METHODS: &[&str] = &["upnp", "port_forwarding", "external_url", "tailscale"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureRequest {
    pub method: Option<String>,
    pub external_url: Option<String>,
    pub tailscale_auth_key: Option<String>,
}

pub async fn configure(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Extension(client_ip): Extension<ClientIp>,
    Json(body): Json<ConfigureRequest>,
) -> Result<Json<Value>, AppError> {
    // Validate method if provided
    if let Some(ref method) = body.method {
        if !VALID_METHODS.contains(&method.as_str()) {
            return Err(AppError::BadRequest(format!(
                "invalid method '{}': must be one of {}",
                method,
                VALID_METHODS.join(", ")
            )));
        }
    }

    // Check if switching to/from tailscale (requires restart)
    let old_method = state.config.read().await.remote_access.method.clone();
    let needs_restart = body
        .method
        .as_deref()
        .is_some_and(|m| m == "tailscale" || old_method == "tailscale");

    // Save to config
    {
        let mut config = state.config.write().await;
        if let Some(ref method) = body.method {
            config.remote_access.method = method.clone();
        }
        if body.external_url.is_some() {
            config.remote_access.external_url = body.external_url.clone();
        }
        if body.tailscale_auth_key.is_some() {
            config.remote_access.tailscale_auth_key = body.tailscale_auth_key.clone();
        }
        if let Err(e) = config.save() {
            tracing::warn!("failed to save config: {e}");
        }
    }

    // If currently enabled, restart with new settings
    let is_enabled = state.remote_access.status.read().await.enabled;
    if is_enabled {
        if needs_restart {
            // Tailscale listener is set up at startup — trigger server restart
            state.restart.notify_one();
        } else {
            state.remote_access.stop().await;
            let config = state.config.read().await;
            let method = config.remote_access.method.clone();
            let external_url = config.remote_access.external_url.clone();
            drop(config);
            let _ = state.remote_access.start(external_url, &method).await;

            // Re-persist enabled state
            let mut config = state.config.write().await;
            config.remote_access.enabled = true;
            let _ = config.save();
        }
    }

    let details = format!(
        "method={}, external_url={}, tailscale_auth_key={}",
        body.method.as_deref().unwrap_or("unchanged"),
        body.external_url.as_deref().unwrap_or("unchanged"),
        if body.tailscale_auth_key.is_some() { "set" } else { "unchanged" },
    );
    super::audit::log(
        &state.db,
        &claims.sub,
        &claims.username,
        "remote_access_configure",
        Some(&details),
        Some(&client_ip.0),
    )
    .await;

    let status = state.remote_access.status.read().await;
    let https_port = state.config.read().await.server.https_port;
    Ok(Json(status_json(&status, https_port)))
}
