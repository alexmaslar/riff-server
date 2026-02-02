use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ArtistResponse {
    pub id: String,
    pub name: String,
    pub bio: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ArtistDetailResponse {
    pub id: String,
    pub name: String,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub albums: Vec<AlbumSummary>,
}

#[derive(Debug, Serialize)]
pub struct AlbumSummary {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub cover_art_path: Option<String>,
}

pub async fn list_artists(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let artists = if let Some(ref search) = params.search {
        let pattern = format!("%{}%", search);
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
            "SELECT id, name, bio, image_url FROM artists WHERE name LIKE ? ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    } else {
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
            "SELECT id, name, bio, image_url FROM artists ORDER BY name LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
    };

    match artists {
        Ok(rows) => {
            let artists: Vec<ArtistResponse> = rows
                .into_iter()
                .map(|(id, name, bio, image_url)| ArtistResponse {
                    id,
                    name,
                    bio,
                    image_url,
                })
                .collect();
            Json(json!({ "artists": artists }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn get_artist(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Json<Value> {
    let artist = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, name, bio, image_url FROM artists WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let artist = match artist {
        Ok(Some(row)) => row,
        Ok(None) => return Json(json!({ "error": "artist not found" })),
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    let albums = sqlx::query_as::<_, (String, String, Option<i32>, Option<String>)>(
        "SELECT id, title, year, cover_art_path FROM albums WHERE artist_id = ? ORDER BY year, title",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let album_summaries: Vec<AlbumSummary> = albums
        .into_iter()
        .map(|(id, title, year, cover_art_path)| AlbumSummary {
            id,
            title,
            year,
            cover_art_path,
        })
        .collect();

    Json(json!(ArtistDetailResponse {
        id: artist.0,
        name: artist.1,
        bio: artist.2,
        image_url: artist.3,
        albums: album_summaries,
    }))
}
