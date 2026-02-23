use axum::{
    extract::{Query, State},
    Extension, Json,
};
use chrono::Utc;
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
    pub library: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DownloadRecommendationsParams {
    pub limit: Option<i64>,
    pub exclude: Option<String>,
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

    // Increment album play count for completed plays
    if body.completed {
        sqlx::query(
            "UPDATE albums SET play_count = play_count + 1
             WHERE id = (SELECT album_id FROM tracks WHERE id = ?)",
        )
        .bind(&body.track_id)
        .execute(&state.db)
        .await?;
    }

    // Emit event for plugins (scrobbling, etc.)
    if body.completed {
        if let Ok(row) = sqlx::query(
            "SELECT t.title, t.duration_seconds, ar.name as artist, al.title as album
             FROM tracks t
             JOIN albums al ON t.album_id = al.id
             JOIN artists ar ON al.artist_id = ar.id
             WHERE t.id = ?",
        )
        .bind(&body.track_id)
        .fetch_one(&state.db)
        .await
        {
            use riff_core::plugin::events::ServerEvent;
            state.event_bus.emit(ServerEvent::TrackCompleted {
                user_id: claims.sub.clone(),
                track_id: body.track_id.clone(),
                title: row.get("title"),
                artist: row.get("artist"),
                album: row.get("album"),
                duration_secs: row.get::<i32, _>("duration_seconds") as u32,
                played_at: Utc::now().timestamp(),
            });
        }
    }

    Ok(Json(json!({ "ok": true })))
}

/// GET /history/albums — Recently played albums (distinct, ordered by last played)
pub async fn recently_played_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(20);
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at, a.play_count,
                MAX(ph.played_at) as last_played
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ?
         AND a.library_id IN (SELECT value FROM json_each(?))
         GROUP BY a.id
         ORDER BY last_played DESC
         LIMIT ?",
    )
    .bind(&claims.sub)
    .bind(&library_ids)
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
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let rows = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style,
                a.label, a.cover_art_path, a.added_at, a.play_count,
                MAX(ph.played_at) as last_played
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ?
         AND a.library_id IN (SELECT value FROM json_each(?))
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
    .bind(&library_ids)
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
    let date_filter = match params.get("period").map(|s| s.as_str()) {
        Some("week") => "AND ph.played_at >= datetime('now', '-7 days')",
        Some("month") => "AND ph.played_at >= datetime('now', '-30 days')",
        Some("year") => "AND ph.played_at >= datetime('now', '-1 year')",
        _ => "", // "all" or missing = no date filter
    };

    let library = params.get("library").map(|s| s.as_str());
    let library_ids = riff_core::db::resolve_library_ids(&state.db, library).await?;

    // Run all stats queries concurrently
    let artists_sql = format!(
        "SELECT ar.id, ar.name, ar.image_url, COUNT(*) as plays, SUM(t.duration_seconds) as listening_seconds
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ? AND ph.completed = 1 {date_filter}
         AND a.library_id IN (SELECT value FROM json_each(?))
         GROUP BY ar.id
         ORDER BY listening_seconds DESC
         LIMIT 5"
    );
    let albums_sql = format!(
        "SELECT a.id, a.title, ar.name as artist_name, a.cover_art_path, COUNT(*) as plays, SUM(t.duration_seconds) as listening_seconds
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ? AND ph.completed = 1 {date_filter}
         AND a.library_id IN (SELECT value FROM json_each(?))
         GROUP BY a.id
         ORDER BY listening_seconds DESC
         LIMIT 5"
    );
    let genres_sql = format!(
        "SELECT j.value as genre_name, SUM(t.duration_seconds) as genre_seconds
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id,
         json_each(CASE WHEN a.genre = '[]' OR a.genre IS NULL THEN '[\"Unknown\"]' ELSE a.genre END) j
         WHERE ph.user_id = ? AND ph.completed = 1 {date_filter}
         AND a.library_id IN (SELECT value FROM json_each(?))
         GROUP BY j.value
         ORDER BY genre_seconds DESC"
    );
    let totals_sql = format!(
        "SELECT COUNT(*) as total_plays, COALESCE(SUM(t.duration_seconds), 0) as total_seconds
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE ph.user_id = ? AND ph.completed = 1 {date_filter}
         AND a.library_id IN (SELECT value FROM json_each(?))"
    );

    let (top_artists, top_albums, genre_rows, totals) = tokio::join!(
        sqlx::query(&artists_sql).bind(&claims.sub).bind(&library_ids).fetch_all(&state.db),
        sqlx::query(&albums_sql).bind(&claims.sub).bind(&library_ids).fetch_all(&state.db),
        sqlx::query(&genres_sql).bind(&claims.sub).bind(&library_ids).fetch_all(&state.db),
        sqlx::query(&totals_sql).bind(&claims.sub).bind(&library_ids).fetch_one(&state.db),
    );

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
                "listeningSeconds": row.get::<i64, _>("listening_seconds"),
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

/// GET /recommendations/downloads — Albums recommended for offline download
/// Scoring: play_count * 10, +50 if favorited, +30 if played in last 7 days, +15 if played in last 30 days
/// Excludes album IDs passed in `exclude` param (comma-separated, already downloaded)
pub async fn download_recommendations(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<DownloadRecommendationsParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(10);

    let exclude_ids: Vec<String> = params
        .exclude
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Build exclude placeholders for SQL
    let exclude_clause = if exclude_ids.is_empty() {
        String::new()
    } else {
        let placeholders: Vec<&str> = exclude_ids.iter().map(|_| "?").collect();
        format!("AND a.id NOT IN ({})", placeholders.join(","))
    };

    let query_str = format!(
        "SELECT a.id, a.title, ar.name as artist_name, a.play_count,
                COUNT(t.id) as track_count,
                COALESCE(SUM(t.file_size_bytes), 0) as estimated_size,
                CASE WHEN f.entity_id IS NOT NULL THEN 50 ELSE 0 END as fav_score,
                CASE
                    WHEN last_play.last_played >= datetime('now', '-7 days') THEN 30
                    WHEN last_play.last_played >= datetime('now', '-30 days') THEN 15
                    ELSE 0
                END as recency_score
         FROM albums a
         JOIN artists ar ON a.artist_id = ar.id
         JOIN tracks t ON t.album_id = a.id
         LEFT JOIN favorites f ON f.entity_id = a.id AND f.entity_type = 'album' AND f.user_id = ?
         LEFT JOIN (
             SELECT t2.album_id, MAX(ph.played_at) as last_played
             FROM play_history ph
             JOIN tracks t2 ON ph.track_id = t2.id
             WHERE ph.user_id = ?
             GROUP BY t2.album_id
         ) last_play ON last_play.album_id = a.id
         WHERE a.play_count > 0 {exclude_clause}
         GROUP BY a.id
         ORDER BY (a.play_count * 10 + CASE WHEN f.entity_id IS NOT NULL THEN 50 ELSE 0 END +
                   CASE
                       WHEN last_play.last_played >= datetime('now', '-7 days') THEN 30
                       WHEN last_play.last_played >= datetime('now', '-30 days') THEN 15
                       ELSE 0
                   END) DESC
         LIMIT ?"
    );

    let mut query = sqlx::query(&query_str)
        .bind(&claims.sub)
        .bind(&claims.sub);

    for id in &exclude_ids {
        query = query.bind(id);
    }

    let rows = query.bind(limit).fetch_all(&state.db).await?;

    let albums: Vec<Value> = rows
        .iter()
        .map(|row| {
            let play_count: i64 = row.get("play_count");
            let fav_score: i64 = row.get("fav_score");
            let recency_score: i64 = row.get("recency_score");
            let score = play_count * 10 + fav_score + recency_score;

            json!({
                "id": row.get::<String, _>("id"),
                "title": row.get::<String, _>("title"),
                "artistName": row.get::<String, _>("artist_name"),
                "estimatedSize": row.get::<i64, _>("estimated_size"),
                "trackCount": row.get::<i64, _>("track_count"),
                "score": score,
            })
        })
        .collect();

    Ok(Json(json!({ "albums": albums })))
}
