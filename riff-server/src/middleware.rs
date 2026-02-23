use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use riff_core::auth::{self, Claims};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::AppState;

/// Marker inserted into request extensions to indicate whether the client
/// is remote (public internet) or local (private LAN / loopback).
#[derive(Debug, Clone, Copy)]
pub struct IsRemote(pub bool);

/// Client IP address extracted from ConnectInfo or X-Forwarded-For header.
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

fn extract_client_ip(req: &Request<Body>) -> String {
    // Try ConnectInfo first (set by axum::serve / into_make_service_with_connect_info)
    if let Some(ci) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return ci.0.ip().to_string();
    }
    // Fall back to X-Forwarded-For for reverse proxy setups
    if let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "unknown".to_string()
}

fn is_private_ip(ip: &str) -> bool {
    use std::net::IpAddr;
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

pub async fn mark_remote(mut req: Request<Body>, next: Next) -> Response {
    let ip = extract_client_ip(&req);
    let is_remote = !is_private_ip(&ip);
    if !is_remote {
        tracing::debug!("HTTPS client {ip} is on private network, treating as local");
    }
    req.extensions_mut().insert(IsRemote(is_remote));
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

pub async fn mark_local(mut req: Request<Body>, next: Next) -> Response {
    // Allow reverse proxy users to force remote mode via header
    let is_remote = req
        .headers()
        .get("X-Riff-Remote")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "true" || v == "1");
    req.extensions_mut().insert(IsRemote(is_remote));
    let ip = extract_client_ip(&req);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// Middleware that requires a valid JWT token.
/// Injects the Claims into request extensions.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let token = match token {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "missing authorization header" })),
            )
                .into_response()
        }
    };

    match auth::validate_token(token, &state.jwt_secret) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or expired token" })),
        )
            .into_response(),
    }
}

/// Middleware that requires the authenticated user to have admin role.
/// Must be applied after require_auth.
pub async fn require_admin(req: Request<Body>, next: Next) -> Response {
    let claims = req.extensions().get::<Claims>();

    match claims {
        Some(c) if c.role == "admin" => next.run(req).await,
        _ => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "admin access required" })),
        )
            .into_response(),
    }
}
