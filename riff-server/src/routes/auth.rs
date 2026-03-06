use axum::{extract::State, Extension, Json};
use riff_core::auth;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::ClientIp;
use crate::AppState;

// --- Rate limiting / account lockout ---

const IP_WINDOW: Duration = Duration::from_secs(900); // 15 min
const IP_MAX_ATTEMPTS: usize = 10;
const LOCKOUT_THRESHOLD_1: u32 = 5;
const LOCKOUT_THRESHOLD_2: u32 = 10;
const LOCKOUT_DURATION_1: Duration = Duration::from_secs(900); // 15 min
const LOCKOUT_DURATION_2: Duration = Duration::from_secs(3600); // 1 hour

pub struct LoginGuard {
    ip_attempts: HashMap<IpAddr, VecDeque<Instant>>,
    username_failures: HashMap<String, (u32, Instant)>,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self {
            ip_attempts: HashMap::new(),
            username_failures: HashMap::new(),
        }
    }

    /// Check rate limits before verifying credentials. Returns Err(message, retry_after_secs) if blocked.
    pub fn check(&mut self, ip: IpAddr, username: &str) -> Result<(), (String, u64)> {
        let now = Instant::now();
        self.cleanup(now);

        // IP throttle
        let attempts = self.ip_attempts.entry(ip).or_default();
        while attempts.front().is_some_and(|t| now.duration_since(*t) > IP_WINDOW) {
            attempts.pop_front();
        }
        if attempts.len() >= IP_MAX_ATTEMPTS {
            if let Some(oldest) = attempts.front() {
                let remaining = IP_WINDOW.saturating_sub(now.duration_since(*oldest));
                return Err((
                    "too many login attempts, try again later".into(),
                    remaining.as_secs().max(1),
                ));
            }
        }

        // Account lockout
        if let Some(&(count, last_failure)) = self.username_failures.get(username) {
            let lockout = if count >= LOCKOUT_THRESHOLD_2 {
                LOCKOUT_DURATION_2
            } else if count >= LOCKOUT_THRESHOLD_1 {
                LOCKOUT_DURATION_1
            } else {
                Duration::ZERO
            };

            if !lockout.is_zero() {
                let elapsed = now.duration_since(last_failure);
                if elapsed < lockout {
                    let remaining = lockout - elapsed;
                    return Err((
                        "account temporarily locked, try again later".into(),
                        remaining.as_secs().max(1),
                    ));
                }
            }
        }

        Ok(())
    }

    /// Record a login attempt result.
    pub fn record(&mut self, ip: IpAddr, username: &str, success: bool) {
        let now = Instant::now();

        // Always record IP attempt
        self.ip_attempts.entry(ip).or_default().push_back(now);

        if success {
            self.username_failures.remove(username);
        } else {
            let entry = self
                .username_failures
                .entry(username.to_string())
                .or_insert((0, now));

            // If the previous lockout period expired, reset counter
            let lockout = if entry.0 >= LOCKOUT_THRESHOLD_2 {
                LOCKOUT_DURATION_2
            } else if entry.0 >= LOCKOUT_THRESHOLD_1 {
                LOCKOUT_DURATION_1
            } else {
                Duration::ZERO
            };
            if !lockout.is_zero() && now.duration_since(entry.1) >= lockout {
                *entry = (0, now);
            }

            entry.0 += 1;
            entry.1 = now;
        }
    }

    fn cleanup(&mut self, now: Instant) {
        // Remove IP entries older than 1 hour
        let hour = Duration::from_secs(3600);
        self.ip_attempts.retain(|_, v| {
            v.retain(|t| now.duration_since(*t) < hour);
            !v.is_empty()
        });
        // Remove username entries whose lockout has fully expired
        self.username_failures.retain(|_, (count, last)| {
            let lockout = if *count >= LOCKOUT_THRESHOLD_2 {
                LOCKOUT_DURATION_2
            } else {
                LOCKOUT_DURATION_1
            };
            now.duration_since(*last) < lockout
        });
    }
}

// --- Handlers ---

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Parse an IpAddr from a string, falling back to loopback.
fn parse_ip(s: &str) -> IpAddr {
    s.parse().unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

#[tracing::instrument(skip(state, req))]
pub async fn login(
    State(state): State<Arc<AppState>>,
    Extension(client_ip): Extension<ClientIp>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<Value>, AppError> {
    let ip = parse_ip(&client_ip.0);
    let ip_str = &client_ip.0;

    // Check rate limits
    {
        let mut guard = state.login_guard.lock().await;
        if let Err((msg, retry_after)) = guard.check(ip, &req.username) {
            tracing::warn!(ip = %ip_str, username = %req.username, reason = %msg, "login rate-limited");
            return Err(AppError::TooManyRequests(msg, retry_after));
        }
    }

    let user = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(
        "SELECT id, username, password_hash, role, default_library_id FROM users WHERE username = ?",
    )
    .bind(&req.username)
    .fetch_optional(&state.db)
    .await;

    let (id_str, username, password_hash, role, default_library_id) = match user {
        Ok(Some(row)) => row,
        Ok(None) => {
            {
                let mut guard = state.login_guard.lock().await;
                guard.record(ip, &req.username, false);
            }
            tracing::warn!(ip = %ip_str, username = %req.username, reason = "user not found", "login failed");
            super::audit::log(&state.db, "", &req.username, "login_failed", Some("user not found"), Some(ip_str)).await;
            return Err(AppError::Unauthorized("invalid credentials".into()));
        }
        Err(e) => return Err(AppError::Internal(e.to_string())),
    };

    let password_ok = match auth::verify_password(&req.password, &password_hash) {
        Ok(ok) => ok,
        Err(e) => return Err(AppError::Internal(e.to_string())),
    };

    if !password_ok {
        {
            let mut guard = state.login_guard.lock().await;
            guard.record(ip, &req.username, false);
        }
        tracing::warn!(ip = %ip_str, username = %req.username, reason = "wrong password", "login failed");
        super::audit::log(&state.db, &id_str, &username, "login_failed", Some("wrong password"), Some(ip_str)).await;
        return Err(AppError::Unauthorized("invalid credentials".into()));
    }

    // Success — reset failure counter
    {
        let mut guard = state.login_guard.lock().await;
        guard.record(ip, &req.username, true);
    }

    let user_id = match Uuid::parse_str(&id_str) {
        Ok(id) => id,
        Err(e) => return Err(AppError::BadRequest(e.to_string())),
    };

    match auth::create_token(&user_id, &username, &role, &state.config.read().await.auth.jwt_secret) {
        Ok(token) => {
            super::audit::log(&state.db, &id_str, &username, "login", None, Some(ip_str)).await;
            Ok(Json(json!({
                "token": token,
                "user": {
                    "id": id_str,
                    "username": username,
                    "role": role,
                    "default_library_id": default_library_id,
                }
            })))
        }
        Err(e) => Err(AppError::Internal(e.to_string())),
    }
}

#[tracing::instrument(skip(state, headers))]
pub async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, AppError> {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => return Err(AppError::Unauthorized("missing authorization header".into())),
    };

    let jwt_secret = state.config.read().await.auth.jwt_secret.clone();

    let claims = match auth::validate_token(token, &jwt_secret) {
        Ok(c) => c,
        Err(e) => return Err(AppError::Unauthorized(format!("invalid token: {}", e))),
    };

    let user_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(e) => return Err(AppError::BadRequest(e.to_string())),
    };

    match auth::create_token(&user_id, &claims.username, &claims.role, &jwt_secret) {
        Ok(new_token) => Ok(Json(json!({ "token": new_token }))),
        Err(e) => Err(AppError::Internal(e.to_string())),
    }
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
}
