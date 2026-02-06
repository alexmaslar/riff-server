use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use riff_core::auth::{self, Claims};
use serde_json::json;
use std::sync::Arc;

use crate::AppState;

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

    let jwt_secret = state.config.read().await.auth.jwt_secret.clone();
    match auth::validate_token(token, &jwt_secret) {
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
