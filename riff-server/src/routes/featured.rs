use axum::{extract::{Query, State}, http::header, Extension, Json};
use chrono::Utc;
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::routes::albums::build_album_detail;
use crate::routes::artists::build_artist_detail;
use crate::AppState;

type CachedJson = ([(header::HeaderName, String); 1], Json<Value>);

fn daily_cache_header() -> [(header::HeaderName, String); 1] {
    let now = Utc::now();
    let end_of_day = (now + chrono::Duration::days(1))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let remaining = (end_of_day - now.naive_utc()).num_seconds().max(60);
    [(header::CACHE_CONTROL, format!("private, max-age={remaining}"))]
}

pub async fn get_featured_album(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<CachedJson, AppError> {
    let date_str = Utc::now().format("%Y-%m-%d").to_string();

    let album_id = riff_core::featured::pick_featured_album_id(
        &state.db,
        &claims.sub,
        &date_str,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let album_id = album_id.ok_or(AppError::NotFound("no albums in library".into()))?;

    let detail = build_album_detail(&state.db, &album_id, &claims.sub).await?;
    Ok((daily_cache_header(), Json(json!(detail))))
}

pub async fn get_featured_artist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<CachedJson, AppError> {
    let date_str = Utc::now().format("%Y-%m-%d").to_string();

    let artist_id = riff_core::featured::pick_featured_artist_id(
        &state.db,
        &claims.sub,
        &date_str,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let artist_id = artist_id.ok_or(AppError::NotFound("no artists in library".into()))?;

    let detail = build_artist_detail(&state.db, &artist_id, &claims.sub).await?;
    Ok((daily_cache_header(), Json(json!(detail))))
}

#[derive(Deserialize)]
pub struct FeaturedParams {
    pub count: Option<u8>,
}

pub async fn get_featured_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<FeaturedParams>,
) -> Result<CachedJson, AppError> {
    let count = (params.count.unwrap_or(1)).min(3) as usize;

    if count == 0 {
        return Ok((daily_cache_header(), Json(json!({ "albums": [] }))));
    }

    let date_str = Utc::now().format("%Y-%m-%d").to_string();

    let album_ids = riff_core::featured::pick_featured_album_ids(
        &state.db,
        &claims.sub,
        &date_str,
        count,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let futures: Vec<_> = album_ids
        .iter()
        .map(|id| build_album_detail(&state.db, id, &claims.sub))
        .collect();
    let albums: Vec<_> = futures::future::try_join_all(futures).await?;

    Ok((daily_cache_header(), Json(json!({ "albums": albums }))))
}

pub async fn get_featured_artists(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<FeaturedParams>,
) -> Result<CachedJson, AppError> {
    let count = (params.count.unwrap_or(1)).min(3) as usize;

    if count == 0 {
        return Ok((daily_cache_header(), Json(json!({ "artists": [] }))));
    }

    let date_str = Utc::now().format("%Y-%m-%d").to_string();

    let artist_ids = riff_core::featured::pick_featured_artist_ids(
        &state.db,
        &claims.sub,
        &date_str,
        count,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let futures: Vec<_> = artist_ids
        .iter()
        .map(|id| build_artist_detail(&state.db, id, &claims.sub))
        .collect();
    let artists: Vec<_> = futures::future::try_join_all(futures).await?;

    Ok((daily_cache_header(), Json(json!({ "artists": artists }))))
}
