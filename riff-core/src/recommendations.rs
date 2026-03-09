use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use tracing::info;
use uuid::Uuid;

pub struct RecommendResult {
    pub albums_processed: u32,
    pub recommendations_generated: u32,
}

struct AlbumFeatures {
    id: String,
    artist_id: String,
    artist_external_id: Option<String>,
    genres: HashSet<String>,
    styles: HashSet<String>,
}

/// Generate album recommendations (ListenBrainz collaborative filtering primary, genre/style fallback).
/// Incremental: skips albums already in `album_recommendations`.
pub async fn generate_recommendations(pool: &SqlitePool) -> Result<RecommendResult> {
    generate_partitioned(pool, false).await
}

/// Generate album recommendations for ALL albums (force regenerate).
/// Uses ListenBrainz collaborative filtering as primary signal, genre/style Jaccard as fallback.
pub async fn generate_recommendations_force(pool: &SqlitePool) -> Result<RecommendResult> {
    generate_partitioned(pool, true).await
}

async fn generate_partitioned(pool: &SqlitePool, force_full: bool) -> Result<RecommendResult> {
    let mut total = RecommendResult {
        albums_processed: 0,
        recommendations_generated: 0,
    };

    // Non-isolated libraries grouped together
    let non_isolated = load_album_features(
        pool,
        "l.isolated = 0",
        None,
    )
    .await?;

    if !non_isolated.is_empty() {
        let r = generate_inner(pool, &non_isolated, force_full).await?;
        total.albums_processed += r.albums_processed;
        total.recommendations_generated += r.recommendations_generated;
    }

    // Each isolated library gets its own pass
    let isolated_libs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM libraries WHERE isolated = 1")
            .fetch_all(pool)
            .await?;

    for (lib_id,) in &isolated_libs {
        let albums = load_album_features(
            pool,
            "a.library_id = ?",
            Some(lib_id),
        )
        .await?;

        if !albums.is_empty() {
            let r = generate_inner(pool, &albums, force_full).await?;
            total.albums_processed += r.albums_processed;
            total.recommendations_generated += r.recommendations_generated;
        }
    }

    Ok(total)
}

async fn load_album_features(
    pool: &SqlitePool,
    where_clause: &str,
    bind_param: Option<&str>,
) -> Result<Vec<AlbumFeatures>> {
    let query_str = format!(
        "SELECT a.id, a.artist_id, ar.external_id, a.genre, a.style \
         FROM albums a \
         JOIN libraries l ON a.library_id = l.id \
         JOIN artists ar ON a.artist_id = ar.id \
         WHERE {where_clause}"
    );

    let rows: Vec<(String, String, Option<String>, String, String)> =
        if let Some(param) = bind_param {
            sqlx::query_as(&query_str)
                .bind(param)
                .fetch_all(pool)
                .await?
        } else {
            sqlx::query_as(&query_str).fetch_all(pool).await?
        };

    let mut albums = Vec::with_capacity(rows.len());
    for (id, artist_id, artist_external_id, genre_json, style_json) in rows {
        let genres = parse_json_set(&genre_json);
        let styles = parse_json_set(&style_json);

        albums.push(AlbumFeatures {
            id,
            artist_id,
            artist_external_id,
            genres,
            styles,
        });
    }

    Ok(albums)
}

fn parse_json_set(json_str: &str) -> HashSet<String> {
    serde_json::from_str::<Vec<String>>(json_str)
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect()
}

async fn generate_inner(
    pool: &SqlitePool,
    albums: &[AlbumFeatures],
    force_full: bool,
) -> Result<RecommendResult> {
    let album_ids: HashSet<&str> = albums.iter().map(|a| a.id.as_str()).collect();

    // Determine which albums need recommendations
    let target_ids: HashSet<&str> = if force_full {
        album_ids.clone()
    } else {
        let covered_rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT album_id FROM album_recommendations")
                .fetch_all(pool)
                .await?;
        let covered: HashSet<&str> = covered_rows.iter().map(|(id,)| id.as_str()).collect();
        album_ids
            .iter()
            .copied()
            .filter(|id| !covered.contains(id))
            .collect()
    };

    if target_ids.is_empty() {
        info!(
            "album recommendations up to date ({} albums covered), skipping",
            albums.len()
        );
        return Ok(RecommendResult {
            albums_processed: 0,
            recommendations_generated: 0,
        });
    }

    info!(
        "generating album recommendations for {} of {} albums",
        target_ids.len(),
        albums.len()
    );

    // Build lookup structures
    // artist external_id (MBID) → album indices whose artist has that MBID
    let mut ext_id_to_artist_idx: HashMap<&str, Vec<usize>> = HashMap::new();
    // artist_id → album indices
    let mut artist_id_to_album_idxs: HashMap<&str, Vec<usize>> = HashMap::new();
    // Genre/style indexes for fallback
    let mut genre_index: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut style_index: HashMap<&str, Vec<usize>> = HashMap::new();

    for (i, album) in albums.iter().enumerate() {
        if let Some(ref ext_id) = album.artist_external_id {
            ext_id_to_artist_idx
                .entry(ext_id.as_str())
                .or_default()
                .push(i);
        }
        artist_id_to_album_idxs
            .entry(album.artist_id.as_str())
            .or_default()
            .push(i);
        for g in &album.genres {
            genre_index.entry(g.as_str()).or_default().push(i);
        }
        for s in &album.styles {
            style_index.entry(s.as_str()).or_default().push(i);
        }
    }

    if force_full {
        sqlx::query("DELETE FROM album_recommendations")
            .execute(pool)
            .await?;
    }

    let mut total_recs: u32 = 0;
    let mut albums_with_recs: u32 = 0;

    for (i, album) in albums.iter().enumerate() {
        if !target_ids.contains(album.id.as_str()) {
            continue;
        }

        let mut scored: Vec<(f64, String, usize)> = Vec::new();

        // Primary path: LB similar artists → find their albums
        if let Some(ref ext_id) = album.artist_external_id {
            let lb_rows: Vec<(String, String, i32)> = sqlx::query_as(
                "SELECT similar_artist_mbid, COALESCE(similar_artist_name, ''), score \
                 FROM lb_similar_artists WHERE artist_mbid = ? ORDER BY score DESC LIMIT 20",
            )
            .bind(ext_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();

            for (similar_mbid, _similar_name, lb_score) in &lb_rows {
                // Find albums in library by artists with this MBID
                if let Some(album_idxs) = ext_id_to_artist_idx.get(similar_mbid.as_str()) {
                    let normalized = *lb_score as f64 / 100.0;
                    for &j in album_idxs {
                        if j == i { continue; }
                        // Skip same artist
                        if albums[j].artist_id == album.artist_id { continue; }
                        let reason = "Listeners also enjoy".to_string();
                        scored.push((normalized, reason, j));
                    }
                }
            }
        }

        // Fallback: genre/style overlap if LB yielded < 3 results
        if scored.len() < 3 {
            let mut candidates: HashSet<usize> = HashSet::new();
            for g in &album.genres {
                if let Some(idxs) = genre_index.get(g.as_str()) {
                    candidates.extend(idxs);
                }
            }
            for s in &album.styles {
                if let Some(idxs) = style_index.get(s.as_str()) {
                    candidates.extend(idxs);
                }
            }
            candidates.remove(&i);

            let lb_indices: HashSet<usize> = scored.iter().map(|(_, _, j)| *j).collect();
            for &j in &candidates {
                if lb_indices.contains(&j) { continue; }
                let other = &albums[j];
                if album.artist_id == other.artist_id { continue; }
                let score = jaccard(&album.genres, &other.genres) * 0.5
                    + jaccard(&album.styles, &other.styles) * 0.5;
                if score >= 0.15 {
                    let shared: Vec<&String> = album.genres.intersection(&other.genres)
                        .chain(album.styles.intersection(&other.styles))
                        .take(3)
                        .collect();
                    let names: Vec<&str> = shared.iter().map(|s| s.as_str()).collect();
                    let reason = if names.is_empty() {
                        "Similar style".to_string()
                    } else {
                        format!("Shares {}", names.join(", "))
                    };
                    scored.push((score, reason, j));
                }
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(6);

        if scored.is_empty() {
            continue;
        }

        albums_with_recs += 1;
        for (order, (score, reason, j)) in scored.iter().enumerate() {
            let other = &albums[*j];
            let id = Uuid::new_v4().to_string();

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO album_recommendations \
                 (id, album_id, recommended_album_id, reason, score, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&album.id)
            .bind(&other.id)
            .bind(reason)
            .bind(score)
            .bind(order as i32)
            .execute(pool)
            .await
            {
                tracing::warn!("failed to insert recommendation: {}", e);
                continue;
            }
            total_recs += 1;
        }
    }

    info!(
        "album recommendations complete: {} albums, {} recommendations",
        albums_with_recs, total_recs
    );

    Ok(RecommendResult {
        albums_processed: albums_with_recs,
        recommendations_generated: total_recs,
    })
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

// --- Artist Recommendations -------------------------------------------------

struct ArtistFeatures {
    id: String,
    external_id: Option<String>,
    genres: HashSet<String>,
    styles: HashSet<String>,
}

/// Generate artist recommendations (ListenBrainz collaborative filtering primary, genre/style fallback).
/// Incremental: skips artists already in `artist_recommendations`.
pub async fn generate_artist_recommendations(pool: &SqlitePool) -> Result<RecommendResult> {
    generate_artist_partitioned(pool, false).await
}

/// Generate artist recommendations for ALL artists (force regenerate).
pub async fn generate_artist_recommendations_force(pool: &SqlitePool) -> Result<RecommendResult> {
    generate_artist_partitioned(pool, true).await
}

async fn generate_artist_partitioned(
    pool: &SqlitePool,
    force_full: bool,
) -> Result<RecommendResult> {
    let mut total = RecommendResult {
        albums_processed: 0,
        recommendations_generated: 0,
    };

    // Non-isolated libraries grouped together
    let non_isolated = load_artist_features(
        pool,
        "l.isolated = 0",
        None,
    )
    .await?;

    if !non_isolated.is_empty() {
        let r = generate_artist_inner(pool, &non_isolated, force_full).await?;
        total.albums_processed += r.albums_processed;
        total.recommendations_generated += r.recommendations_generated;
    }

    // Each isolated library gets its own pass
    let isolated_libs: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM libraries WHERE isolated = 1")
            .fetch_all(pool)
            .await?;

    for (lib_id,) in &isolated_libs {
        let artists = load_artist_features(
            pool,
            "a.library_id = ?",
            Some(lib_id),
        )
        .await?;

        if !artists.is_empty() {
            let r = generate_artist_inner(pool, &artists, force_full).await?;
            total.albums_processed += r.albums_processed;
            total.recommendations_generated += r.recommendations_generated;
        }
    }

    Ok(total)
}

async fn load_artist_features(
    pool: &SqlitePool,
    where_clause: &str,
    bind_param: Option<&str>,
) -> Result<Vec<ArtistFeatures>> {
    // Get distinct artists with their external_ids from qualifying albums
    let artist_query = format!(
        "SELECT DISTINCT a.artist_id, ar.external_id \
         FROM albums a \
         JOIN libraries l ON a.library_id = l.id \
         JOIN artists ar ON a.artist_id = ar.id \
         WHERE {where_clause}"
    );

    let artist_ids: Vec<(String, Option<String>)> = if let Some(param) = bind_param {
        sqlx::query_as(&artist_query)
            .bind(param)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as(&artist_query).fetch_all(pool).await?
    };

    // Fetch album metadata only for qualifying artists (filtered by library partition)
    let album_query = format!(
        "SELECT a.artist_id, a.genre, a.style \
         FROM albums a \
         JOIN libraries l ON a.library_id = l.id \
         WHERE {where_clause}"
    );

    let album_rows: Vec<(String, String, String)> =
        if let Some(param) = bind_param {
            sqlx::query_as(&album_query)
                .bind(param)
                .fetch_all(pool)
                .await?
        } else {
            sqlx::query_as(&album_query).fetch_all(pool).await?
        };

    let mut artist_genres: HashMap<String, HashSet<String>> = HashMap::new();
    let mut artist_styles: HashMap<String, HashSet<String>> = HashMap::new();

    for (artist_id, genre_json, style_json) in &album_rows {
        let genres = parse_json_set(genre_json);
        let styles = parse_json_set(style_json);

        artist_genres.entry(artist_id.clone()).or_default().extend(genres);
        artist_styles.entry(artist_id.clone()).or_default().extend(styles);
    }

    let mut artists = Vec::with_capacity(artist_ids.len());
    for (artist_id, external_id) in &artist_ids {
        let genres = artist_genres.remove(artist_id).unwrap_or_default();
        let styles = artist_styles.remove(artist_id).unwrap_or_default();

        artists.push(ArtistFeatures {
            id: artist_id.clone(),
            external_id: external_id.clone(),
            genres,
            styles,
        });
    }

    Ok(artists)
}

async fn generate_artist_inner(
    pool: &SqlitePool,
    artists: &[ArtistFeatures],
    force_full: bool,
) -> Result<RecommendResult> {
    let artist_ids: HashSet<&str> = artists.iter().map(|a| a.id.as_str()).collect();

    let target_ids: HashSet<&str> = if force_full {
        artist_ids.clone()
    } else {
        let covered_rows: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT artist_id FROM artist_recommendations")
                .fetch_all(pool)
                .await?;
        let covered: HashSet<&str> = covered_rows.iter().map(|(id,)| id.as_str()).collect();
        artist_ids
            .iter()
            .copied()
            .filter(|id| !covered.contains(id))
            .collect()
    };

    if target_ids.is_empty() {
        info!(
            "artist recommendations up to date ({} artists covered), skipping",
            artists.len()
        );
        return Ok(RecommendResult {
            albums_processed: 0,
            recommendations_generated: 0,
        });
    }

    info!(
        "generating artist recommendations for {} of {} artists",
        target_ids.len(),
        artists.len()
    );

    // Build external_id → index lookup for matching LB results to library artists
    let ext_id_to_idx: HashMap<&str, usize> = artists
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.external_id.as_deref().map(|e| (e, i)))
        .collect();

    // Build genre->artist index for fallback pre-filtering
    let mut genre_index: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut style_index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, artist) in artists.iter().enumerate() {
        for g in &artist.genres {
            genre_index.entry(g.as_str()).or_default().push(i);
        }
        for s in &artist.styles {
            style_index.entry(s.as_str()).or_default().push(i);
        }
    }

    if force_full {
        sqlx::query("DELETE FROM artist_recommendations")
            .execute(pool)
            .await?;
    }

    let mut total_recs: u32 = 0;
    let mut artists_with_recs: u32 = 0;

    for (i, artist) in artists.iter().enumerate() {
        if !target_ids.contains(artist.id.as_str()) {
            continue;
        }

        let mut scored: Vec<(f64, String, usize)> = Vec::new();

        // Primary path: use LB similar artists
        if let Some(ref ext_id) = artist.external_id {
            let lb_rows: Vec<(String, String, i32)> = sqlx::query_as(
                "SELECT similar_artist_mbid, COALESCE(similar_artist_name, ''), score \
                 FROM lb_similar_artists WHERE artist_mbid = ? ORDER BY score DESC LIMIT 20",
            )
            .bind(ext_id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();

            for (similar_mbid, _similar_name, lb_score) in &lb_rows {
                // Match to library artist by external_id
                if let Some(&j) = ext_id_to_idx.get(similar_mbid.as_str()) {
                    if j == i { continue; }
                    let normalized = *lb_score as f64 / 100.0;
                    let reason = "Listeners also enjoy".to_string();
                    scored.push((normalized, reason, j));
                }
            }
        }

        // Fallback: simple genre/style overlap if LB yielded < 3 results
        if scored.len() < 3 {
            let mut candidates: HashSet<usize> = HashSet::new();
            for g in &artist.genres {
                if let Some(idxs) = genre_index.get(g.as_str()) {
                    candidates.extend(idxs);
                }
            }
            for s in &artist.styles {
                if let Some(idxs) = style_index.get(s.as_str()) {
                    candidates.extend(idxs);
                }
            }
            candidates.remove(&i);

            // Exclude artists already scored via LB
            let lb_indices: HashSet<usize> = scored.iter().map(|(_, _, j)| *j).collect();
            for &j in &candidates {
                if lb_indices.contains(&j) { continue; }
                let other = &artists[j];
                let score = jaccard(&artist.genres, &other.genres) * 0.5
                    + jaccard(&artist.styles, &other.styles) * 0.5;
                if score >= 0.15 {
                    let shared: Vec<&String> = artist.genres.intersection(&other.genres)
                        .chain(artist.styles.intersection(&other.styles))
                        .take(3)
                        .collect();
                    let names: Vec<&str> = shared.iter().map(|s| s.as_str()).collect();
                    let reason = if names.is_empty() {
                        "Similar style".to_string()
                    } else {
                        format!("Shares {}", names.join(", "))
                    };
                    scored.push((score, reason, j));
                }
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(6);

        if scored.is_empty() {
            continue;
        }

        artists_with_recs += 1;
        for (order, (score, reason, j)) in scored.iter().enumerate() {
            let other = &artists[*j];
            let id = Uuid::new_v4().to_string();

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO artist_recommendations \
                 (id, artist_id, recommended_artist_id, reason, score, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&artist.id)
            .bind(&other.id)
            .bind(reason)
            .bind(score)
            .bind(order as i32)
            .execute(pool)
            .await
            {
                tracing::warn!("failed to insert artist recommendation: {}", e);
                continue;
            }
            total_recs += 1;
        }
    }

    info!(
        "artist recommendations complete: {} artists, {} recommendations",
        artists_with_recs, total_recs
    );

    Ok(RecommendResult {
        albums_processed: artists_with_recs,
        recommendations_generated: total_recs,
    })
}
