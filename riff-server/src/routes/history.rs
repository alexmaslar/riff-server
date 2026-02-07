use axum::{
    extract::{Query, State},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::AppError;
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
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4().to_string();
    let completed = if body.completed { 1 } else { 0 };

    sqlx::query(
        "INSERT INTO play_history (id, user_id, track_id, completed) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&claims.sub)
    .bind(&body.track_id)
    .bind(completed)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({ "ok": true })))
}

/// GET /history/albums — Recently played albums (distinct, ordered by last played)
pub async fn recently_played_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(20);

    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at, a.play_count,
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
    .await?;

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
                "play_count": row.get::<i64, _>("play_count"),
            })
        })
        .collect();

    Ok(Json(json!({ "albums": albums })))
}

/// GET /history/continue — Albums with incomplete plays (started but not all tracks completed)
pub async fn continue_listening(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(20);

    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at, a.play_count,
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
    .await?;

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
                "play_count": row.get::<i64, _>("play_count"),
            })
        })
        .collect();

    Ok(Json(json!({ "albums": albums })))
}

/// GET /history/stats — Listening statistics for the authenticated user
/// Query params: ?period=week (default: all)
pub async fn listening_stats(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let is_weekly = params.get("period").map(|s| s.as_str()) == Some("week");

    // Run all stats queries concurrently
    let (top_artists, top_albums, genre_rows, totals) = if is_weekly {
        tokio::join!(
            sqlx::query(
                "SELECT ar.id, ar.name, ar.image_url, COUNT(*) as plays, SUM(t.duration_seconds) as listening_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE ph.user_id = ? AND ph.completed = 1 AND ph.played_at >= datetime('now', '-7 days')
                 GROUP BY ar.id
                 ORDER BY plays DESC
                 LIMIT 5",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT a.id, a.title, ar.name as artist_name, a.cover_art_path, COUNT(*) as plays
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE ph.user_id = ? AND ph.completed = 1 AND ph.played_at >= datetime('now', '-7 days')
                 GROUP BY a.id
                 ORDER BY plays DESC
                 LIMIT 5",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT j.value as genre_name, SUM(t.duration_seconds) as genre_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id,
                 json_each(CASE WHEN a.genre = '[]' OR a.genre IS NULL THEN '[\"Unknown\"]' ELSE a.genre END) j
                 WHERE ph.user_id = ? AND ph.completed = 1 AND ph.played_at >= datetime('now', '-7 days')
                 GROUP BY j.value
                 ORDER BY genre_seconds DESC",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT COUNT(*) as total_plays, COALESCE(SUM(t.duration_seconds), 0) as total_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 WHERE ph.user_id = ? AND ph.completed = 1 AND ph.played_at >= datetime('now', '-7 days')",
            )
            .bind(&claims.sub)
            .fetch_one(&state.db),
        )
    } else {
        tokio::join!(
            sqlx::query(
                "SELECT ar.id, ar.name, ar.image_url, COUNT(*) as plays, SUM(t.duration_seconds) as listening_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE ph.user_id = ? AND ph.completed = 1
                 GROUP BY ar.id
                 ORDER BY plays DESC
                 LIMIT 5",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT a.id, a.title, ar.name as artist_name, a.cover_art_path, COUNT(*) as plays
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id
                 JOIN artists ar ON a.artist_id = ar.id
                 WHERE ph.user_id = ? AND ph.completed = 1
                 GROUP BY a.id
                 ORDER BY plays DESC
                 LIMIT 5",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT j.value as genre_name, SUM(t.duration_seconds) as genre_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 JOIN albums a ON t.album_id = a.id,
                 json_each(CASE WHEN a.genre = '[]' OR a.genre IS NULL THEN '[\"Unknown\"]' ELSE a.genre END) j
                 WHERE ph.user_id = ? AND ph.completed = 1
                 GROUP BY j.value
                 ORDER BY genre_seconds DESC",
            )
            .bind(&claims.sub)
            .fetch_all(&state.db),
            sqlx::query(
                "SELECT COUNT(*) as total_plays, COALESCE(SUM(t.duration_seconds), 0) as total_seconds
                 FROM play_history ph
                 JOIN tracks t ON ph.track_id = t.id
                 WHERE ph.user_id = ? AND ph.completed = 1",
            )
            .bind(&claims.sub)
            .fetch_one(&state.db),
        )
    };

    let top_artists = top_artists?;
    let top_albums = top_albums?;
    let genre_rows = genre_rows?;
    let totals = totals?;

    let artists_json: Vec<Value> = top_artists
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "name": row.get::<String, _>("name"),
                "imageUrl": row.get::<Option<String>, _>("image_url"),
                "plays": row.get::<i64, _>("plays"),
                "listeningSeconds": row.get::<i64, _>("listening_seconds"),
            })
        })
        .collect();

    let albums_json: Vec<Value> = top_albums
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "title": row.get::<String, _>("title"),
                "artistName": row.get::<String, _>("artist_name"),
                "coverArtPath": row.get::<Option<String>, _>("cover_art_path"),
                "plays": row.get::<i64, _>("plays"),
            })
        })
        .collect();

    let genre_list: Vec<(String, i64)> = genre_rows
        .iter()
        .map(|row| {
            let name: String = row.get("genre_name");
            let seconds: i64 = row.get("genre_seconds");
            (name, seconds)
        })
        .collect();

    let total_genre_seconds: i64 = genre_list.iter().map(|(_, s)| *s).sum();

    let genres_json: Vec<Value> = genre_list
        .iter()
        .map(|(name, seconds)| {
            let pct = if total_genre_seconds > 0 {
                (*seconds as f64 / total_genre_seconds as f64) * 100.0
            } else {
                0.0
            };
            json!({
                "name": name,
                "percentage": (pct * 10.0).round() / 10.0,
                "listeningSeconds": seconds,
            })
        })
        .collect();

    let total_plays: i64 = totals.get("total_plays");
    let total_seconds: i64 = totals.get("total_seconds");

    Ok(Json(json!({
        "totalPlays": total_plays,
        "totalListeningSeconds": total_seconds,
        "topArtists": artists_json,
        "topAlbums": albums_json,
        "genreBreakdown": genres_json,
    })))
}
