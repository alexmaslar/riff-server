use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use riff_core::{auth::Claims, musicbrainz};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::sync::Arc;
use tokio::fs::File;
use crate::error::AppError;
use crate::AppState;
use std::path::Path as StdPath;

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
    pub library: Option<String>,
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
    pub source: Option<String>,
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
    pub summary: Option<String>,
    pub summary_source: Option<String>,
    pub rating: Option<f64>,
    pub rating_sources: Vec<serde_json::Value>,
    pub moods: Vec<String>,
    pub descriptors: Vec<String>,
    pub keywords: Vec<String>,
    pub reviews: Vec<EditorialReview>,
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
    pub source: Option<String>,
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
pub struct EditorialReview {
    pub source: String,
    pub source_url: Option<String>,
    pub text: String,
    pub rating: Option<f64>,
    pub rating_count: Option<i64>,
    pub license: Option<String>,
    pub reviewer: Option<String>,
    pub review_date: Option<String>,
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
    pub isrc: Option<String>,
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
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.cover_art_path, a.added_at, a.play_count, COUNT(*) OVER() as total_count, a.source
         FROM albums a JOIN artists ar ON a.artist_id = ar.id",
    );

    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;
    builder.push(" WHERE a.library_id IN (SELECT value FROM json_each(");
    builder.push_bind(library_ids);
    builder.push("))");
    let mut has_where = true;

    if let Some(ref search) = params.search {
        builder.push(" AND (a.title LIKE ");
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
                source: row.get(12),
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
        "SELECT a.id, a.title, a.artist_id, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.cover_art_path, a.summary, a.rating, a.moods, a.descriptors, a.keywords, a.metadata_status, a.added_at, a.country, a.release_notes, a.all_labels, a.is_compilation, a.play_count, a.source, a.summary_source, a.rating_sources
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
                    t.file_size_bytes, t.isrc, a.play_count as album_play_count
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
             JOIN albums src ON src.id = r.album_id \
             JOIN libraries src_lib ON src.library_id = src_lib.id \
             JOIN libraries rec_lib ON a.library_id = rec_lib.id \
             WHERE r.album_id = ? \
             AND (src.library_id = a.library_id OR (src_lib.isolated = 0 AND rec_lib.isolated = 0)) \
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
            isrc: row.get(16),
            album_play_count: row.get(17),
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
    let moods_str: String = album_row.get(12);
    let descriptors_str: String = album_row.get(13);
    let keywords_str: String = album_row.get(14);
    let all_labels_str: String = album_row.get(19);
    let is_compilation_int: i32 = album_row.get(20);
    let play_count: i64 = album_row.get(21);
    let rating_sources_str: String = album_row.get(24);
    // Fetch editorial reviews for this album
    let review_rows = sqlx::query_as::<_, (String, Option<String>, String, Option<f64>, Option<i64>, Option<String>, Option<String>, Option<String>)>(
        "SELECT source, source_url, text, rating, rating_count, license, reviewer, review_date \
         FROM editorial_reviews WHERE entity_type = 'album' AND entity_id = ? AND text != '' AND hidden = 0 \
         ORDER BY created_at DESC"
    )
    .bind(album_id)
    .fetch_all(db)
    .await?;

    let reviews: Vec<EditorialReview> = review_rows
        .into_iter()
        .map(|(source, source_url, text, rating, rating_count, license, reviewer, review_date)| EditorialReview {
            source,
            source_url,
            text,
            rating,
            rating_count,
            license,
            reviewer,
            review_date,
        })
        .collect();

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
        summary: album_row.get(10),
        summary_source: album_row.get(23),
        rating: album_row.get(11),
        rating_sources: serde_json::from_str(&rating_sources_str).unwrap_or_default(),
        moods: serde_json::from_str(&moods_str).unwrap_or_default(),
        descriptors: serde_json::from_str(&descriptors_str).unwrap_or_default(),
        keywords: serde_json::from_str(&keywords_str).unwrap_or_default(),
        metadata_status: album_row.get(15),
        added_at: album_row.get(16),
        country: album_row.get(17),
        release_notes: album_row.get(18),
        all_labels: serde_json::from_str(&all_labels_str).unwrap_or_default(),
        is_compilation: is_compilation_int != 0,
        play_count,
        tracks: track_summaries,
        credits: credit_summaries,
        is_favorited,
        similar_albums,
        source: album_row.get(22),
        reviews,
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

    // For URL-based cover art (e.g. Apple Music), redirect to the source
    if cover_path.starts_with("http://") || cover_path.starts_with("https://") {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, &cover_path)
            .header(header::CACHE_CONTROL, "public, max-age=86400")
            .body(Body::empty())
            .unwrap();
    }

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

#[derive(Debug, Deserialize)]
pub struct ListFiltersParams {
    pub focus: Option<String>,
    pub library: Option<String>,
}

struct ParsedFilter {
    category: String,
    value: String,
}

fn parse_focus_filters(focus: &str) -> Vec<ParsedFilter> {
    focus
        .split(',')
        .filter_map(|part| {
            let (key, value) = part.split_once(':')?;
            Some(ParsedFilter {
                category: key.to_string(),
                value: value.to_string(),
            })
        })
        .collect()
}

/// Append focus filter conditions to a QueryBuilder, excluding one category.
/// All base queries already have a WHERE clause, so this always appends " AND ...".
fn append_focus_conditions(
    builder: &mut QueryBuilder<Sqlite>,
    filters: &[ParsedFilter],
    exclude_category: &str,
    user_id: &str,
) {
    for f in filters {
        if f.category == exclude_category {
            continue;
        }
        builder.push(" AND ");
        match f.category.as_str() {
            "genre" => {
                builder.push("a.genre LIKE ");
                builder.push_bind(format!("%\"{}\"%" , f.value));
            }
            "style" => {
                builder.push("a.style LIKE ");
                builder.push_bind(format!("%\"{}\"%" , f.value));
            }
            "decade" => {
                if let Some(start_year) =
                    f.value.strip_suffix('s').and_then(|y| y.parse::<i32>().ok())
                {
                    builder.push("(a.year >= ");
                    builder.push_bind(start_year);
                    builder.push(" AND a.year < ");
                    builder.push_bind(start_year + 10);
                    builder.push(")");
                } else {
                    builder.push("1=1");
                }
            }
            "format" => {
                builder.push(
                    "EXISTS (SELECT 1 FROM tracks t2 WHERE t2.album_id = a.id AND t2.format = ",
                );
                builder.push_bind(f.value.clone());
                builder.push(")");
            }
            "label" => {
                builder.push("a.label = ");
                builder.push_bind(f.value.clone());
            }
            "added" => {
                let interval = match f.value.as_str() {
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
                if f.value == "true" {
                    builder.push(
                        "EXISTS (SELECT 1 FROM favorites fav WHERE fav.entity_id = a.id AND fav.entity_type = 'album' AND fav.user_id = ",
                    );
                    builder.push_bind(user_id.to_string());
                    builder.push(")");
                } else {
                    builder.push("1=1");
                }
            }
            "played" => {
                if f.value == "true" {
                    builder.push(
                        "EXISTS (SELECT 1 FROM play_history ph2 JOIN tracks t2 ON ph2.track_id = t2.id WHERE t2.album_id = a.id AND ph2.user_id = ",
                    );
                    builder.push_bind(user_id.to_string());
                    builder.push(")");
                } else {
                    builder.push("1=1");
                }
            }
            "country" => {
                builder.push("a.country = ");
                builder.push_bind(f.value.clone());
            }
            _ => {
                builder.push("1=1");
            }
        }
    }
}

pub async fn list_filters(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<Claims>,
    Query(params): Query<ListFiltersParams>,
) -> Result<Json<Value>, AppError> {
    let filters = params
        .focus
        .as_deref()
        .map(parse_focus_filters)
        .unwrap_or_default();

    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    // Each category query applies all filters EXCEPT its own category,
    // so the user can still switch within a category while cross-category
    // options narrow to only what would return results.

    let genres: Vec<String> = {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT j.value FROM albums a \
             JOIN artists ar ON a.artist_id = ar.id, json_each(a.genre) j \
             WHERE j.value IS NOT NULL AND a.library_id IN (SELECT value FROM json_each(",
        );
        qb.push_bind(library_ids.clone());
        qb.push("))");
        append_focus_conditions(&mut qb, &filters, "genre", &claims.sub);
        qb.push(" ORDER BY j.value");
        let rows = qb.build().fetch_all(&state.db).await?;
        rows.iter().map(|r| r.get(0)).collect()
    };

    let styles: Vec<String> = {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT j.value FROM albums a \
             JOIN artists ar ON a.artist_id = ar.id, json_each(a.style) j \
             WHERE j.value IS NOT NULL AND a.library_id IN (SELECT value FROM json_each(",
        );
        qb.push_bind(library_ids.clone());
        qb.push("))");
        append_focus_conditions(&mut qb, &filters, "style", &claims.sub);
        qb.push(" ORDER BY j.value");
        let rows = qb.build().fetch_all(&state.db).await?;
        rows.iter().map(|r| r.get(0)).collect()
    };

    let labels: Vec<String> = {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT a.label FROM albums a \
             JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.label IS NOT NULL AND a.label != '' AND a.library_id IN (SELECT value FROM json_each(",
        );
        qb.push_bind(library_ids.clone());
        qb.push("))");
        append_focus_conditions(&mut qb, &filters, "label", &claims.sub);
        qb.push(" ORDER BY a.label");
        let rows = qb.build().fetch_all(&state.db).await?;
        rows.iter().map(|r| r.get(0)).collect()
    };

    let formats: Vec<String> = {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT t.format FROM tracks t \
             JOIN albums a ON t.album_id = a.id \
             JOIN artists ar ON a.artist_id = ar.id \
             WHERE t.format IS NOT NULL AND a.library_id IN (SELECT value FROM json_each(",
        );
        qb.push_bind(library_ids.clone());
        qb.push("))");
        append_focus_conditions(&mut qb, &filters, "format", &claims.sub);
        qb.push(" ORDER BY t.format");
        let rows = qb.build().fetch_all(&state.db).await?;
        rows.iter().map(|r| r.get(0)).collect()
    };

    let decades: Vec<String> = {
        let mut qb = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT (a.year / 10 * 10) as decade_start FROM albums a \
             JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.year IS NOT NULL AND a.library_id IN (SELECT value FROM json_each(",
        );
        qb.push_bind(library_ids.clone());
        qb.push("))");
        append_focus_conditions(&mut qb, &filters, "decade", &claims.sub);
        qb.push(" ORDER BY decade_start");
        let rows = qb.build().fetch_all(&state.db).await?;
        rows.iter()
            .map(|r| {
                let year: i32 = r.get(0);
                format!("{year}s")
            })
            .collect()
    };

    Ok(Json(json!({
        "genres": genres,
        "styles": styles,
        "labels": labels,
        "formats": formats,
        "decades": decades,
    })))
}

#[derive(Debug, Deserialize)]
pub struct GenresParams {
    pub library: Option<String>,
}

pub async fn list_genres(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Query(params): Query<GenresParams>,
) -> Result<Json<Value>, AppError> {
    let library_ids = riff_core::db::resolve_library_ids(&state.db, params.library.as_deref()).await?;

    let rows = sqlx::query(
        "SELECT j.value as name, COUNT(DISTINCT a.id) as album_count,
                (SELECT a2.id FROM albums a2, json_each(a2.genre) j2
                 WHERE j2.value = j.value AND a2.library_id IN (SELECT value FROM json_each(?1))
                 ORDER BY a2.play_count DESC LIMIT 1) as representative_album_id
         FROM albums a, json_each(a.genre) j
         WHERE j.value IS NOT NULL AND a.library_id IN (SELECT value FROM json_each(?1))
         GROUP BY j.value
         ORDER BY j.value"
    )
    .bind(&library_ids)
    .fetch_all(&state.db)
    .await?;

    let genres: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.get::<String, _>("name"),
                "album_count": row.get::<i64, _>("album_count"),
                "representative_album_id": row.get::<Option<String>, _>("representative_album_id"),
            })
        })
        .collect();

    Ok(Json(json!({ "genres": genres })))
}

pub async fn refresh_cover(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Look up album info
    let (external_id,): (Option<String>,) = sqlx::query_as(
        "SELECT external_id FROM albums WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("album not found".into()))?;

    let mut mbid = external_id.filter(|s| !s.is_empty());

    // If no MusicBrainz ID yet, try enriching the album first
    if mbid.is_none() {
        let _ = musicbrainz::enrich_album(&state.db, &id).await;
        let updated: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT external_id FROM albums WHERE id = ?",
        )
        .bind(&id)
        .fetch_optional(&state.db)
        .await?;
        mbid = updated.and_then(|(eid,)| eid).filter(|s| !s.is_empty());
    }

    // Try Cover Art Archive first (if we have a MusicBrainz ID)
    let mut cover_bytes: Option<Vec<u8>> = None;
    if let Some(ref mbid) = mbid {
        let caa_url = format!("https://coverartarchive.org/release/{}/front", mbid);
        if let Ok(resp) = reqwest::get(&caa_url).await {
            if resp.status().is_success() {
                if let Ok(bytes) = resp.bytes().await {
                    if !bytes.is_empty() {
                        cover_bytes = Some(bytes.to_vec());
                    }
                }
            }
        }
    }

    let bytes = cover_bytes.ok_or(AppError::NotFound("no cover art found".into()))?;

    // Determine album directory from first track's file_path
    let track_path: Option<(String,)> = sqlx::query_as(
        "SELECT file_path FROM tracks WHERE album_id = ? LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?;

    let album_dir = track_path
        .and_then(|(p,)| StdPath::new(&p).parent().map(|d| d.to_path_buf()))
        .ok_or(AppError::Internal(
            "could not determine album directory".to_string(),
        ))?;

    // Write cover.jpg to album directory
    let cover_path = album_dir.join("cover.jpg");
    tokio::fs::write(&cover_path, &bytes)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Update DB
    let cover_str = cover_path.to_string_lossy().to_string();
    sqlx::query("UPDATE albums SET cover_art_path = ? WHERE id = ?")
        .bind(&cover_str)
        .bind(&id)
        .execute(&state.db)
        .await?;

    // Delete cached thumbnails
    if let Some(data_dir) = dirs::data_dir() {
        let thumbs_dir = data_dir.join("riff").join("thumbs");
        if let Ok(mut entries) = tokio::fs::read_dir(&thumbs_dir).await {
            let prefix = format!("{}_", id);
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }

    Ok(Json(json!({"ok": true})))
}

pub async fn delete_album(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Verify album exists and get artist_id for orphan cleanup
    let (_, artist_id) = sqlx::query_as::<_, (String, String)>(
        "SELECT id, artist_id FROM albums WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound("album not found".into()))?;

    // Gather track file paths before deleting DB records
    let track_paths: Vec<(String,)> = sqlx::query_as(
        "SELECT file_path FROM tracks WHERE album_id = ?",
    )
    .bind(&id)
    .fetch_all(&state.db)
    .await?;

    let album_dir = track_paths
        .first()
        .and_then(|(p,)| StdPath::new(p).parent().map(|d| d.to_path_buf()));

    // Delete related rows (no FK cascade enforced at runtime)
    sqlx::query("DELETE FROM favorites WHERE entity_type = 'album' AND entity_id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query(
        "DELETE FROM play_history WHERE track_id IN (SELECT id FROM tracks WHERE album_id = ?)",
    )
    .bind(&id)
    .execute(&state.db)
    .await?;

    sqlx::query("DELETE FROM playlist_tracks WHERE track_id IN (SELECT id FROM tracks WHERE album_id = ?)")
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query("DELETE FROM daily_mix_tracks WHERE track_id IN (SELECT id FROM tracks WHERE album_id = ?)")
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query("DELETE FROM album_recommendations WHERE album_id = ? OR recommended_album_id = ?")
        .bind(&id)
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query("DELETE FROM tracks WHERE album_id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    sqlx::query("DELETE FROM albums WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    // Delete orphaned artist (no remaining albums)
    let (remaining,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM albums WHERE artist_id = ?",
    )
    .bind(&artist_id)
    .fetch_one(&state.db)
    .await?;

    if remaining == 0 {
        sqlx::query("DELETE FROM favorites WHERE entity_type = 'artist' AND entity_id = ?")
            .bind(&artist_id)
            .execute(&state.db)
            .await?;

        sqlx::query("DELETE FROM artists WHERE id = ?")
            .bind(&artist_id)
            .execute(&state.db)
            .await?;
    }

    // Delete album files from disk
    if let Some(dir) = album_dir {
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            tracing::warn!("failed to remove album directory {:?}: {}", dir, e);
        }
    }

    // Delete cached thumbnails
    if let Some(data_dir) = dirs::data_dir() {
        let thumbs_dir = data_dir.join("riff").join("thumbs");
        if let Ok(mut entries) = tokio::fs::read_dir(&thumbs_dir).await {
            let prefix = format!("{}_", id);
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }

    Ok(Json(json!({"ok": true})))
}

#[derive(Debug, Deserialize)]
pub struct UpdateSummaryRequest {
    pub summary: String,
}

pub async fn update_summary(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSummaryRequest>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query(
        "UPDATE albums SET summary = ? WHERE id = ?",
    )
    .bind(&body.summary)
    .bind(&id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("album not found".into()));
    }

    Ok(Json(json!({ "success": true })))
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

pub async fn toggle_review_visibility(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Path((album_id, source)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    // Toggle hidden between 0 and 1
    let result = sqlx::query(
        "UPDATE editorial_reviews SET hidden = CASE WHEN hidden = 0 THEN 1 ELSE 0 END \
         WHERE entity_type = 'album' AND entity_id = ? AND source = ?"
    )
    .bind(&album_id)
    .bind(&source)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("review not found".into()));
    }

    let row = sqlx::query_as::<_, (i32,)>(
        "SELECT hidden FROM editorial_reviews WHERE entity_type = 'album' AND entity_id = ? AND source = ?"
    )
    .bind(&album_id)
    .bind(&source)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "hidden": row.0 != 0 })))
}

pub async fn get_hidden_reviews(
    State(state): State<Arc<AppState>>,
    Extension(_claims): Extension<Claims>,
    Path(album_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query_as::<_, (String, Option<String>, String, Option<f64>, Option<i64>, Option<String>, Option<String>, Option<String>)>(
        "SELECT source, source_url, text, rating, rating_count, license, reviewer, review_date \
         FROM editorial_reviews WHERE entity_type = 'album' AND entity_id = ? AND hidden = 1 \
         ORDER BY created_at DESC"
    )
    .bind(&album_id)
    .fetch_all(&state.db)
    .await?;

    let reviews: Vec<EditorialReview> = rows
        .into_iter()
        .map(|(source, source_url, text, rating, rating_count, license, reviewer, review_date)| EditorialReview {
            source,
            source_url,
            text,
            rating,
            rating_count,
            license,
            reviewer,
            review_date,
        })
        .collect();

    Ok(Json(json!({ "reviews": reviews })))
}

