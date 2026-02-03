use axum::{
    extract::{Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordPlayBody {
    pub track_id: String,
    pub completed: bool,
}

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    pub limit: Option<i64>,
}

/// POST /history — Record a play event
pub async fn record_play(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Json(body): Json<RecordPlayBody>,
) -> Json<Value> {
    let id = Uuid::new_v4().to_string();
    let completed = if body.completed { 1 } else { 0 };

    let result = sqlx::query(
        "INSERT INTO play_history (id, user_id, track_id, completed) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&claims.sub)
    .bind(&body.track_id)
    .bind(completed)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// GET /history/albums — Recently played albums (distinct, ordered by last played)
pub async fn recently_played_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HistoryParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(20);

    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at,
                MAX(ph.played_at) as last_played
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ?
         GROUP BY a.id
         ORDER BY last_played DESC
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
                    let genre: Vec<String> = serde_json::from_str(&genre_str).unwrap_or_default();
                    let style: Vec<String> = serde_json::from_str(&style_str).unwrap_or_default();
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

/// GET /history/continue — Albums with incomplete plays (started but not all tracks completed)
pub async fn continue_listening(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HistoryParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(20);

    // Albums where user has played some tracks but not completed all tracks
    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at,
                MAX(ph.played_at) as last_played
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ?
         AND a.id IN (
             SELECT a2.id
             FROM albums a2
             JOIN tracks t2 ON t2.album_id = a2.id
             LEFT JOIN (
                 SELECT track_id, MAX(completed) as was_completed
                 FROM play_history
                 WHERE user_id = ?
                 GROUP BY track_id
             ) ph2 ON ph2.track_id = t2.id
             GROUP BY a2.id
             HAVING COUNT(CASE WHEN ph2.was_completed = 1 THEN 1 END) < COUNT(t2.id)
                AND COUNT(ph2.track_id) > 0
         )
         GROUP BY a.id
         ORDER BY last_played DESC
         LIMIT ?",
    )
    .bind(&claims.sub)
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
                    let genre: Vec<String> = serde_json::from_str(&genre_str).unwrap_or_default();
                    let style: Vec<String> = serde_json::from_str(&style_str).unwrap_or_default();
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
