use axum::{
    extract::{Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;

use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleFavoriteBody {
    pub entity_type: String,
    pub entity_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFavoritesParams {
    pub r#type: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckFavoriteParams {
    pub entity_type: String,
    pub entity_id: String,
}

/// POST /favorites — Toggle favorite (check exists -> DELETE or INSERT)
pub async fn toggle_favorite(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<ToggleFavoriteBody>,
) -> Json<Value> {
    // Check if already favorited
    let exists = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&body.entity_type)
    .bind(&body.entity_id)
    .fetch_one(&state.db)
    .await;

    match exists {
        Ok((count,)) if count > 0 => {
            // Remove favorite
            let result = sqlx::query(
                "DELETE FROM favorites WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
            )
            .bind(&claims.sub)
            .bind(&body.entity_type)
            .bind(&body.entity_id)
            .execute(&state.db)
            .await;

            match result {
                Ok(_) => Json(json!({ "favorited": false })),
                Err(e) => Json(json!({ "error": e.to_string() })),
            }
        }
        Ok(_) => {
            // Add favorite
            let result = sqlx::query(
                "INSERT INTO favorites (user_id, entity_type, entity_id) VALUES (?, ?, ?)",
            )
            .bind(&claims.sub)
            .bind(&body.entity_type)
            .bind(&body.entity_id)
            .execute(&state.db)
            .await;

            match result {
                Ok(_) => Json(json!({ "favorited": true })),
                Err(e) => Json(json!({ "error": e.to_string() })),
            }
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /favorites?type=album — List favorites by entity_type
pub async fn list_favorites(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<ListFavoritesParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(50);

    let entity_type = match params.r#type.as_deref() {
        Some(t) => t,
        None => return Json(json!({ "error": "type parameter is required" })),
    };

    match entity_type {
        "album" => {
            let rows = sqlx::query(
                "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                        a.label, a.cover_art_path, a.added_at
                 FROM favorites f
                 JOIN albums a ON f.entity_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'album'
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(limit)
            .fetch_all(&state.db)
            .await;

            match rows {
                Ok(rows) => {
                    let albums: Vec<Value> = rows
                        .iter()
                        .map(|row| {
                            let genre_str: String = row.get("genre");
                            let style_str: String = row.get("style");
                            let genre: Vec<String> =
                                serde_json::from_str(&genre_str).unwrap_or_default();
                            let style: Vec<String> =
                                serde_json::from_str(&style_str).unwrap_or_default();
                            json!({
                                "id": row.get::<String, _>("id"),
                                "title": row.get::<String, _>("title"),
                                "artist_id": row.get::<String, _>("artist_id"),
                                "artist_name": row.get::<String, _>("name"),
                                "year": row.get::<Option<i32>, _>("year"),
                                "genre": genre,
                                "style": style,
                                "label": row.get::<Option<String>, _>("label"),
                                "cover_art_path": row.get::<Option<String>, _>("cover_art_path"),
                                "added_at": row.get::<String, _>("added_at"),
                            })
                        })
                        .collect();
                    Json(json!({ "albums": albums }))
                }
                Err(e) => Json(json!({ "error": e.to_string() })),
            }
        }
        "artist" => {
            let rows = sqlx::query(
                "SELECT ar.id, ar.name, ar.bio, ar.image_url
                 FROM favorites f
                 JOIN artists ar ON f.entity_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'artist'
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(limit)
            .fetch_all(&state.db)
            .await;

            match rows {
                Ok(rows) => {
                    let artists: Vec<Value> = rows
                        .iter()
                        .map(|row| {
                            json!({
                                "id": row.get::<String, _>("id"),
                                "name": row.get::<String, _>("name"),
                                "bio": row.get::<Option<String>, _>("bio"),
                                "image_url": row.get::<Option<String>, _>("image_url"),
                            })
                        })
                        .collect();
                    Json(json!({ "artists": artists }))
                }
                Err(e) => Json(json!({ "error": e.to_string() })),
            }
        }
        "track" => {
            let rows = sqlx::query(
                "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                        t.format, t.album_id, ar.name as artist_name
                 FROM favorites f
                 JOIN tracks t ON f.entity_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE f.user_id = ? AND f.entity_type = 'track'
                 ORDER BY f.created_at DESC
                 LIMIT ?",
            )
            .bind(&claims.sub)
            .bind(limit)
            .fetch_all(&state.db)
            .await;

            match rows {
                Ok(rows) => {
                    let tracks: Vec<Value> = rows
                        .iter()
                        .map(|row| {
                            json!({
                                "id": row.get::<String, _>("id"),
                                "title": row.get::<String, _>("title"),
                                "track_number": row.get::<i32, _>("track_number"),
                                "disc_number": row.get::<i32, _>("disc_number"),
                                "duration_seconds": row.get::<i32, _>("duration_seconds"),
                                "format": row.get::<String, _>("format"),
                                "album_id": row.get::<String, _>("album_id"),
                                "artist_name": row.get::<String, _>("artist_name"),
                            })
                        })
                        .collect();
                    Json(json!({ "tracks": tracks }))
                }
                Err(e) => Json(json!({ "error": e.to_string() })),
            }
        }
        _ => Json(json!({ "error": "invalid type, must be album, artist, or track" })),
    }
}

/// GET /favorites/check?entityType=album&entityId=uuid — Check if favorited
pub async fn check_favorite(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<CheckFavoriteParams>,
) -> Json<Value> {
    let exists = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&params.entity_type)
    .bind(&params.entity_id)
    .fetch_one(&state.db)
    .await;

    match exists {
        Ok((count,)) => Json(json!({ "favorited": count > 0 })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
