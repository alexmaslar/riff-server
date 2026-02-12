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
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::sync::Arc;
use tokio::fs::File;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct CoverParams {
    pub w: Option<u32>,
}

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
    pub play_count: i64,
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
    pub ai_rating: Option<f64>,
    pub metadata_status: String,
    pub added_at: String,
    pub country: Option<String>,
    pub release_notes: Option<String>,
    pub all_labels: Vec<serde_json::Value>,
    pub is_compilation: bool,
    pub play_count: i64,
    pub tracks: Vec<TrackSummary>,
    pub credits: Vec<CreditSummary>,
    pub is_favorited: bool,
    pub similar_albums: Vec<SimilarAlbum>,
}

#[derive(Debug, Serialize)]
pub struct SimilarAlbum {
    pub id: String,
    pub title: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub cover_art_path: Option<String>,
    pub reason: String,
    pub play_count: i64,
}

#[derive(Debug, Serialize)]
pub struct CreditSummary {
    pub artist_name: String,
    pub role: String,
    pub external_artist_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TrackSummary {
    pub id: String,
    pub title: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub duration_seconds: i32,
    pub format: String,
    pub album_id: String,
    pub artist_name: String,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub composer: Option<String>,
    pub bpm: Option<f64>,
    pub musical_key: Option<String>,
    pub loudness_lufs: Option<f64>,
    pub mood: Option<String>,
    pub file_size_bytes: Option<i64>,
    pub album_play_count: Option<i64>,
}

pub async fn list_albums(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(50);
    let offset = params.offset.unwrap_or(0);

    let order_clause = match params.sort.as_deref() {
        Some("year") => "a.year DESC, a.title",
        Some("added") => "a.added_at DESC",
        Some("artist") => "ar.name, a.year, a.title",
        _ => "a.title",
    };

    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.cover_art_path, a.added_at, a.play_count, COUNT(*) OVER() as total_count
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
                        "last_day" | "Last day" => Some("-1 day"),
                        "last_week" | "Last week" => Some("-7 days"),
                        "last_month" | "Last month" => Some("-1 month"),
                        "last_year" | "Last year" => Some("-1 year"),
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

    let rows = builder.build().fetch_all(&state.db).await?;

    let total: Option<i64> = rows.first().map(|r| r.get("total_count"));
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
                play_count: row.get(10),
            }
        })
        .collect();

    Ok(Json(json!({ "albums": albums, "total": total })))
}

pub async fn build_album_detail(
    db: &SqlitePool,
    album_id: &str,
    user_id: &str,
) -> Result<AlbumDetailResponse, AppError> {
    let album_row = sqlx::query(
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.cover_art_path, a.ai_summary, a.ai_rating, a.metadata_status, a.added_at, a.country, a.release_notes, a.all_labels, a.is_compilation, a.play_count
         FROM albums a JOIN artists ar ON a.artist_id = ar.id
         WHERE a.id = ?",
    )
    .bind(album_id)
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound("album not found".into()))?;

    // Run independent queries concurrently
    let (track_rows, credits, fav_result, similar_rows) = tokio::join!(
        sqlx::query(
            "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds, t.format,
                    t.album_id, ar.name as artist_name, t.sample_rate, t.bit_depth,
                    t.composer, COALESCE(t.bpm_tag, t.bpm_analyzed) as bpm,
                    COALESCE(t.musical_key, t.key_analyzed) as resolved_key, t.loudness_lufs, t.mood,
                    t.file_size_bytes, a.play_count as album_play_count
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.album_id = ? ORDER BY t.disc_number, t.track_number",
        )
        .bind(album_id)
        .fetch_all(db),
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT artist_name, role, external_artist_id
             FROM album_credits WHERE album_id = ? ORDER BY sort_order",
        )
        .bind(album_id)
        .fetch_all(db),
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM favorites WHERE user_id = ? AND entity_type = 'album' AND entity_id = ?",
        )
        .bind(user_id)
        .bind(album_id)
        .fetch_one(db),
        sqlx::query_as::<_, (String, String, String, Option<i32>, Option<String>, String, i64)>(
            "SELECT a.id, a.title, ar.name, a.year, a.cover_art_path, r.reason, a.play_count \
             FROM album_recommendations r \
             JOIN albums a ON r.recommended_album_id = a.id \
             JOIN artists ar ON a.artist_id = ar.id \
             WHERE r.album_id = ? \
             ORDER BY r.sort_order"
        )
        .bind(album_id)
        .fetch_all(db),
    );

    let track_rows = track_rows?;
    let credits = credits?;
    let is_favorited = fav_result.map(|(count,)| count > 0).unwrap_or(false);
    let similar_rows = similar_rows?;

    let track_summaries: Vec<TrackSummary> = track_rows
        .iter()
        .map(|row| TrackSummary {
            id: row.get(0),
            title: row.get(1),
            track_number: row.get(2),
            disc_number: row.get(3),
            duration_seconds: row.get(4),
            format: row.get(5),
            album_id: row.get(6),
            artist_name: row.get(7),
            sample_rate: row.get(8),
            bit_depth: row.get(9),
            composer: row.get(10),
            bpm: row.get(11),
            musical_key: row.get(12),
            loudness_lufs: row.get(13),
            mood: row.get(14),
            file_size_bytes: row.get(15),
            album_play_count: row.get(16),
        })
        .collect();

    let credit_summaries: Vec<CreditSummary> = credits
        .into_iter()
        .map(|(artist_name, role, external_artist_id)| CreditSummary {
            artist_name,
            role,
            external_artist_id,
        })
        .collect();

    let similar_albums: Vec<SimilarAlbum> = similar_rows
        .into_iter()
        .map(|(id, title, artist_name, year, cover_art_path, reason, play_count)| SimilarAlbum {
            id,
            title,
            artist_name,
            year,
            cover_art_path,
            reason,
            play_count,
        })
        .collect();

    let genre_str: String = album_row.get(5);
    let style_str: String = album_row.get(6);
    let all_labels_str: String = album_row.get(16);
    let is_compilation_int: i32 = album_row.get(17);
    let play_count: i64 = album_row.get(18);

    Ok(AlbumDetailResponse {
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
        ai_rating: album_row.get(11),
        metadata_status: album_row.get(12),
        added_at: album_row.get(13),
        country: album_row.get(14),
        release_notes: album_row.get(15),
        all_labels: serde_json::from_str(&all_labels_str).unwrap_or_default(),
        is_compilation: is_compilation_int != 0,
        play_count,
        tracks: track_summaries,
        credits: credit_summaries,
        is_favorited,
        similar_albums,
    })
}

pub async fn get_album(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let detail = build_album_detail(&state.db, &id, &claims.sub).await?;
    Ok((
        [(header::CACHE_CONTROL, "private, max-age=3600")],
        Json(json!(detail)),
    ))
}

pub async fn get_cover(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<CoverParams>,
) -> Response {
    // Fetch album cover path
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

    // If ?w= is specified, serve a cached thumbnail
    if let Some(w) = params.w {
        let w = w.clamp(50, 2000);
        return serve_thumbnail(&cover_path, &id, w).await;
    }

    // Serve original cover art
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

    let file_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("metadata read failed: {}", e) })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Serve a resized thumbnail, caching it to disk.
pub async fn serve_thumbnail(original_path: &str, entity_id: &str, width: u32) -> Response {
    let thumbs_dir = match dirs::data_dir() {
        Some(d) => d.join("riff").join("thumbs"),
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not determine data directory" })),
            )
                .into_response()
        }
    };

    let thumb_filename = format!("{}_{}.jpg", entity_id, width);
    let thumb_path = thumbs_dir.join(&thumb_filename);

    // Generate thumbnail if needed (all filesystem checks inside spawn_blocking)
    {
        let original = original_path.to_string();
        let dest = thumb_path.clone();
        let dir = thumbs_dir.clone();

        let result = tokio::task::spawn_blocking(move || {
            // Check if cached thumb exists and is newer than the original
            let needs_generate = if dest.exists() {
                match (
                    std::fs::metadata(&original).and_then(|m| m.modified()),
                    std::fs::metadata(&dest).and_then(|m| m.modified()),
                ) {
                    (Ok(orig_time), Ok(thumb_time)) => orig_time > thumb_time,
                    _ => true,
                }
            } else {
                true
            };

            if needs_generate {
                std::fs::create_dir_all(&dir)?;
                let img = image::open(&original)?;
                let resized = img.resize(width, width, image::imageops::FilterType::Lanczos3);
                let mut output = std::io::BufWriter::new(std::fs::File::create(&dest)?);
                resized.write_to(&mut output, image::ImageFormat::Jpeg)?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("thumbnail generation failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("thumbnail generation failed: {}", e) })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("thumbnail task failed: {}", e) })),
                )
                    .into_response();
            }
        }
    }

    // Serve the cached thumbnail
    let file = match File::open(&thumb_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("thumb file open failed: {}", e) })),
            )
                .into_response()
        }
    };

    let file_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("metadata read failed: {}", e) })),
            )
                .into_response()
        }
    };

    let stream = tokio_util::io::ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::CACHE_CONTROL, "public, max-age=86400")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub async fn list_filters(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
) -> Result<Json<Value>, AppError> {
    let (genre_rows, style_rows, label_rows, format_rows) = tokio::join!(
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT j.value FROM albums a, json_each(a.genre) j WHERE j.value IS NOT NULL ORDER BY j.value"
        )
        .fetch_all(&state.db),
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT j.value FROM albums a, json_each(a.style) j WHERE j.value IS NOT NULL ORDER BY j.value"
        )
        .fetch_all(&state.db),
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT a.label FROM albums a WHERE a.label IS NOT NULL AND a.label != '' ORDER BY a.label"
        )
        .fetch_all(&state.db),
        sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT t.format FROM tracks t WHERE t.format IS NOT NULL ORDER BY t.format"
        )
        .fetch_all(&state.db),
    );

    let genres: Vec<String> = genre_rows?.into_iter().map(|(v,)| v).collect();
    let styles: Vec<String> = style_rows?.into_iter().map(|(v,)| v).collect();
    let labels: Vec<String> = label_rows?.into_iter().map(|(v,)| v).collect();
    let formats: Vec<String> = format_rows?.into_iter().map(|(v,)| v).collect();

    Ok(Json(json!({
        "genres": genres,
        "styles": styles,
        "labels": labels,
        "formats": formats,
    })))
}

pub async fn increment_play_count(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let result = sqlx::query("UPDATE albums SET play_count = play_count + 1 WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await;

    match result {
        Ok(r) => {
            if r.rows_affected() == 0 {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "album not found" })),
                ));
            }
            Ok(Json(json!({ "success": true })))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )),
    }
}

