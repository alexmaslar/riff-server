use axum::{extract::{Query, State}, Extension, Json};
use chrono::Utc;
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::error::AppError;
use crate::routes::albums::build_album_detail;
use crate::routes::artists::build_artist_detail;
use crate::AppState;

pub async fn get_featured_album(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
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
    Ok(Json(json!(detail)))
}

pub async fn get_featured_artist(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
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
    Ok(Json(json!(detail)))
}

#[derive(Deserialize)]
pub struct FeaturedParams {
    pub count: Option<u8>,
}

pub async fn get_featured_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<FeaturedParams>,
) -> Result<Json<Value>, AppError> {
    let count = (params.count.unwrap_or(1)).min(3) as usize;

    if count == 0 {
        return Ok(Json(json!({ "albums": [] })));
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

    let mut albums = Vec::with_capacity(album_ids.len());
    for id in &album_ids {
        let detail = build_album_detail(&state.db, id, &claims.sub).await?;
        albums.push(detail);
    }

    Ok(Json(json!({ "albums": albums })))
}

pub async fn get_featured_artists(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<FeaturedParams>,
) -> Result<Json<Value>, AppError> {
    let count = (params.count.unwrap_or(1)).min(3) as usize;

    if count == 0 {
        return Ok(Json(json!({ "artists": [] })));
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

    let mut artists = Vec::with_capacity(artist_ids.len());
    for id in &artist_ids {
        let detail = build_artist_detail(&state.db, id, &claims.sub).await?;
        artists.push(detail);
    }

    Ok(Json(json!({ "artists": artists })))
}
