use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use riff_core::auth::Claims;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{QueryBuilder, Row, Sqlite};
use std::sync::Arc;
use tokio::fs::File;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    pub sort: Option<String>,
    pub focus: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AlbumResponse {
    pub id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub cover_art_path: Option<String>,
    pub added_at: String,
}

#[derive(Debug, Serialize)]
pub struct AlbumDetailResponse {
    pub id: String,
    pub title: String,
    pub artist_id: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub cover_art_path: Option<String>,
    pub ai_summary: Option<String>,
    pub metadata_status: String,
    pub added_at: String,
    pub country: Option<String>,
    pub release_notes: Option<String>,
    pub all_labels: Vec<serde_json::Value>,
    pub is_compilation: bool,
    pub tracks: Vec<TrackSummary>,
    pub credits: Vec<CreditSummary>,
    pub is_favorited: bool,
}

#[derive(Debug, Serialize)]
pub struct CreditSummary {
    pub artist_name: String,
    pub role: String,
    pub discogs_artist_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TrackSummary {
    pub id: String,
    pub title: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub duration_seconds: i32,
    pub format: String,
    pub composer: Option<String>,
    pub bpm: Option<f64>,
    pub musical_key: Option<String>,
    pub loudness_lufs: Option<f64>,
    pub mood: Option<String>,
}

pub async fn list_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<ListParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let order_clause = match params.sort.as_deref() {
        Some("year") => "a.year DESC, a.title",
        Some("added") => "a.added_at DESC",
        Some("artist") => "ar.name, a.year, a.title",
        _ => "a.title",
    };

    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.cover_art_path, a.added_at
         FROM albums a JOIN artists ar ON a.artist_id = ar.id",
    );

    let mut has_where = false;

    if let Some(ref search) = params.search {
        builder.push(" WHERE (a.title LIKE ");
        builder.push_bind(format!("%{search}%"));
        builder.push(" OR ar.name LIKE ");
        builder.push_bind(format!("%{search}%"));
        builder.push(")");
        has_where = true;
    }

    if let Some(ref focus) = params.focus {
        for part in focus.split(',') {
            let Some((key, value)) = part.split_once(':') else {
                continue;
            };

            if has_where {
                builder.push(" AND ");
            } else {
                builder.push(" WHERE ");
                has_where = true;
            }

            match key {
                "genre" => {
                    builder.push("a.genre LIKE ");
                    builder.push_bind(format!("%\"{value}\"%"));
                }
                "style" => {
                    builder.push("a.style LIKE ");
                    builder.push_bind(format!("%\"{value}\"%"));
                }
                "decade" => {
                    if let Some(start_year) =
                        value.strip_suffix('s').and_then(|y| y.parse::<i32>().ok())
                    {
                        builder.push("(a.year >= ");
                        builder.push_bind(start_year);
                        builder.push(" AND a.year < ");
                        builder.push_bind(start_year + 10);
                        builder.push(")");
                    } else {
                        // Invalid decade format, skip with always-true condition
                        builder.push("1=1");
                    }
                }
                "format" => {
                    builder.push(
                        "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.format = ",
                    );
                    builder.push_bind(value.to_string());
                    builder.push(")");
                }
                "label" => {
                    builder.push("a.label = ");
                    builder.push_bind(value.to_string());
                }
                "added" => {
                    let interval = match value {
                        "last_day" => Some("-1 day"),
                        "last_week" => Some("-7 days"),
                        "last_month" => Some("-1 month"),
                        "last_year" => Some("-1 year"),
                        _ => None,
                    };
                    if let Some(interval) = interval {
                        builder.push(format!(
                            "a.added_at >= datetime('now', '{interval}')"
                        ));
                    } else {
                        builder.push("1=1");
                    }
                }
                "favorited" => {
                    if value == "true" {
                        builder.push(
                            "EXISTS (SELECT 1 FROM favorites f WHERE f.entity_id = a.id AND f.entity_type = 'album' AND f.user_id = ",
                        );
                        builder.push_bind(claims.sub.clone());
                        builder.push(")");
                    } else {
                        builder.push("1=1");
                    }
                }
                "played" => {
                    if value == "true" {
                        builder.push(
                            "EXISTS (SELECT 1 FROM play_history ph JOIN tracks t ON ph.track_id = t.id WHERE t.album_id = a.id AND ph.user_id = ",
                        );
                        builder.push_bind(claims.sub.clone());
                        builder.push(")");
                    } else {
                        builder.push("1=1");
                    }
                }
                "country" => {
                    builder.push("a.country = ");
                    builder.push_bind(value.to_string());
                }
                "bpm" => {
                    // Range format: "120-140"
                    if let Some((lo, hi)) = value.split_once('-') {
                        if let (Ok(lo), Ok(hi)) = (lo.parse::<f64>(), hi.parse::<f64>()) {
                            builder.push(
                                "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND COALESCE(t.bpm_tag, t.bpm_analyzed) >= ",
                            );
                            builder.push_bind(lo);
                            builder.push(" AND COALESCE(t.bpm_tag, t.bpm_analyzed) <= ");
                            builder.push_bind(hi);
                            builder.push(")");
                        } else {
                            builder.push("1=1");
                        }
                    } else {
                        builder.push("1=1");
                    }
                }
                "key" => {
                    builder.push(
                        "EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND COALESCE(t.musical_key, t.key_analyzed) = ",
                    );
                    builder.push_bind(value.to_string());
                    builder.push(")");
                }
                _ => {
                    builder.push("1=1");
                }
            }
        }
    }

    builder.push(" ORDER BY ");
    builder.push(order_clause);
    builder.push(" LIMIT ");
    builder.push_bind(limit);
    builder.push(" OFFSET ");
    builder.push_bind(offset);

    let result = builder.build().fetch_all(&state.db).await;

    match result {
        Ok(rows) => {
            let albums: Vec<AlbumResponse> = rows
                .iter()
                .map(|row| {
                    let genre_str: String = row.get(5);
                    let style_str: String = row.get(6);
                    AlbumResponse {
                        id: row.get(0),
                        title: row.get(1),
                        artist_id: row.get(2),
                        artist_name: row.get(3),
                        year: row.get(4),
                        genre: serde_json::from_str(&genre_str).unwrap_or_default(),
                        style: serde_json::from_str(&style_str).unwrap_or_default(),
                        label: row.get(7),
                        cover_art_path: row.get(8),
                        added_at: row.get(9),
                    }
                })
                .collect();
            Json(json!({ "albums": albums }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

pub async fn get_album(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Json<Value> {
    let album_row = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.cover_art_path, a.ai_summary, a.metadata_status, a.added_at, a.country, a.release_notes, a.all_labels, a.is_compilation
         FROM albums a JOIN artists ar ON a.artist_id = ar.id
         WHERE a.id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let album_row = match album_row {
        Ok(Some(row)) => row,
        Ok(None) => return Json(json!({ "error": "album not found" })),
        Err(e) => return Json(json!({ "error": e.to_string() })),
    };

    let track_rows = sqlx::query(
        "SELECT id, title, track_number, disc_number, duration_seconds, format, composer, COALESCE(bpm_tag, bpm_analyzed) as bpm, COALESCE(musical_key, key_analyzed) as resolved_key, loudness_lufs, mood
         FROM tracks WHERE album_id = ? ORDER BY disc_number, track_number",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let track_summaries: Vec<TrackSummary> = track_rows
        .iter()
        .map(|row| TrackSummary {
            id: row.get(0),
            title: row.get(1),
            track_number: row.get(2),
            disc_number: row.get(3),
            duration_seconds: row.get(4),
            format: row.get(5),
            composer: row.get(6),
            bpm: row.get(7),
            musical_key: row.get(8),
            loudness_lufs: row.get(9),
            mood: row.get(10),
        })
        .collect();

    let credits = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT artist_name, role, discogs_artist_id
         FROM album_credits WHERE album_id = ? ORDER BY sort_order",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let credit_summaries: Vec<CreditSummary> = credits
        .into_iter()
        .map(|(artist_name, role, discogs_artist_id)| CreditSummary {
            artist_name,
            role,
            discogs_artist_id,
        })
        .collect();

    let is_favorited = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = 'album' AND entity_id = ?",
    )
    .bind(&claims.sub)
    .bind(&id)
    .fetch_one(&state.db)
    .await
    .map(|(count,)| count > 0)
    .unwrap_or(false);

    let genre_str: String = album_row.get(5);
    let style_str: String = album_row.get(6);
    let all_labels_str: String = album_row.get(15);
    let is_compilation_int: i32 = album_row.get(16);

    Json(json!(AlbumDetailResponse {
        id: album_row.get(0),
        title: album_row.get(1),
        artist_id: album_row.get(2),
        artist_name: album_row.get(3),
        year: album_row.get(4),
        genre: serde_json::from_str(&genre_str).unwrap_or_default(),
        style: serde_json::from_str(&style_str).unwrap_or_default(),
        label: album_row.get(7),
        catalog_number: album_row.get(8),
        cover_art_path: album_row.get(9),
        ai_summary: album_row.get(10),
        metadata_status: album_row.get(11),
        added_at: album_row.get(12),
        country: album_row.get(13),
        release_notes: album_row.get(14),
        all_labels: serde_json::from_str(&all_labels_str).unwrap_or_default(),
        is_compilation: is_compilation_int != 0,
        tracks: track_summaries,
        credits: credit_summaries,
        is_favorited,
    }))
}

pub async fn get_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT cover_art_path FROM albums WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await;

    let cover_path = match row {
        Ok(Some((Some(path),))) => path,
        Ok(Some((None,))) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "no cover art" })))
                .into_response()
        }
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "album not found" })))
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    };

    let mime = if cover_path.ends_with(".png") {
        "image/png"
    } else {
        "image/jpeg"
    };

    let file = match File::open(&cover_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("file open failed: {}", e) })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}
