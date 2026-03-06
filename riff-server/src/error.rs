use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    NotFound(String),
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    Internal(String),
    TooManyRequests(String, u64),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::NotFound(msg) => write!(f, "not found: {msg}"),
            AppError::BadRequest(msg) => write!(f, "bad request: {msg}"),
            AppError::Unauthorized(msg) => write!(f, "unauthorized: {msg}"),
            AppError::Forbidden(msg) => write!(f, "forbidden: {msg}"),
            AppError::Internal(msg) => write!(f, "internal error: {msg}"),
            AppError::TooManyRequests(msg, _) => write!(f, "too many requests: {msg}"),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::TooManyRequests(msg, retry_after_secs) => {
                let mut response = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "error": msg })),
                )
                    .into_response();
                response.headers_mut().insert(
                    "Retry-After",
                    retry_after_secs.to_string().parse().expect("numeric string is valid header value"),
                );
                response
            }
            other => {
                let (status, message) = match other {
                    AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
                    AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
                    AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg),
                    AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg),
                    AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
                    AppError::TooManyRequests(..) => unreachable!(),
                };
                (status, Json(json!({ "error": message }))).into_response()
            }
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, error_type = "database", "database error");
        AppError::Internal("database error".to_string())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, error_type = "internal", "internal error");
        AppError::Internal(e.to_string())
    }
}
