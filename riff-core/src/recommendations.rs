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
    genres: HashSet<String>,
    styles: HashSet<String>,
    tags: HashSet<String>,
    credits: HashSet<String>,
    label: Option<String>,
    year: Option<i32>,
    bliss_centroid: Option<Vec<f64>>,
}

/// Generate album recommendations using metadata similarity (no AI needed).
/// Incremental: skips albums already in `album_recommendations`.
pub async fn generate_recommendations(pool: &SqlitePool) -> Result<RecommendResult> {
    generate_partitioned(pool, false).await
}

/// Generate album recommendations for ALL albums (force regenerate).
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
    // Fetch album metadata
    let query_str = format!(
        "SELECT a.id, a.artist_id, a.year, a.genre, a.style, a.label, \
                a.moods, a.descriptors, a.keywords \
         FROM albums a \
         JOIN libraries l ON a.library_id = l.id \
         WHERE {where_clause}"
    );

    let rows: Vec<(
        String,
        String,
        Option<i32>,
        String,
        String,
        Option<String>,
        String,
        String,
        String,
    )> = if let Some(param) = bind_param {
        sqlx::query_as(&query_str)
            .bind(param)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as(&query_str).fetch_all(pool).await?
    };

    // Fetch all credits grouped by album
    let credits_rows: Vec<(String, String)> =
        sqlx::query_as("SELECT album_id, LOWER(artist_name) FROM album_credits")
            .fetch_all(pool)
            .await?;
    let mut credits_map: HashMap<String, HashSet<String>> = HashMap::new();
    for (album_id, name) in credits_rows {
        credits_map.entry(album_id).or_default().insert(name);
    }

    // Fetch bliss centroids: average bliss_features per album
    let bliss_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT album_id, bliss_features FROM tracks WHERE bliss_features IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut bliss_map: HashMap<String, Vec<Vec<f64>>> = HashMap::new();
    for (album_id, features_json) in &bliss_rows {
        if let Ok(features) = serde_json::from_str::<Vec<f64>>(features_json) {
            if !features.is_empty() {
                bliss_map
                    .entry(album_id.clone())
                    .or_default()
                    .push(features);
            }
        }
    }

    let mut albums = Vec::with_capacity(rows.len());
    for (id, artist_id, year, genre_json, style_json, label, moods_json, descriptors_json, keywords_json) in rows
    {
        let genres = parse_json_set(&genre_json);
        let styles = parse_json_set(&style_json);

        let moods = parse_json_set(&moods_json);
        let descriptors = parse_json_set(&descriptors_json);
        let keywords = parse_json_set(&keywords_json);
        let mut tags = HashSet::new();
        tags.extend(moods);
        tags.extend(descriptors);
        tags.extend(keywords);

        let credits = credits_map.remove(&id).unwrap_or_default();

        let bliss_centroid = bliss_map.remove(&id).map(|vecs| {
            let n = vecs.len() as f64;
            let dim = vecs[0].len();
            let mut centroid = vec![0.0_f64; dim];
            for v in &vecs {
                for (i, val) in v.iter().enumerate() {
                    if i < dim {
                        centroid[i] += val;
                    }
                }
            }
            for c in &mut centroid {
                *c /= n;
            }
            centroid
        });

        albums.push(AlbumFeatures {
            id,
            artist_id,
            genres,
            styles,
            tags,
            credits,
            label: label.filter(|l| !l.is_empty()),
            year,
            bliss_centroid,
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
        "generating metadata-based recommendations for {} of {} albums",
        target_ids.len(),
        albums.len()
    );

    // Build a genre->album index for pre-filtering
    let mut genre_index: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut style_index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, album) in albums.iter().enumerate() {
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

        // Pre-filter: find candidate albums that share at least one genre or style
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
        // If no genre/style overlap at all, compare against all albums as fallback
        if candidates.is_empty() {
            candidates.extend(0..albums.len());
        }
        candidates.remove(&i); // remove self

        // Score each candidate
        let mut scored: Vec<(f64, String, usize)> = Vec::new();
        for &j in &candidates {
            let other = &albums[j];
            // Skip same artist
            if album.artist_id == other.artist_id {
                continue;
            }
            let (score, reason) = compute_similarity(album, other);
            if score >= 0.15 {
                scored.push((score, reason, j));
            }
        }

        // Sort by score descending, take top 6
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
        "recommendations complete: {} albums, {} recommendations",
        albums_with_recs, total_recs
    );

    Ok(RecommendResult {
        albums_processed: albums_with_recs,
        recommendations_generated: total_recs,
    })
}

/// Compute similarity score and reason between two albums.
/// Returns (score, reason) where score is 0.0–1.0.
fn compute_similarity(a: &AlbumFeatures, b: &AlbumFeatures) -> (f64, String) {
    let style_sim = jaccard(&a.styles, &b.styles);
    let genre_sim = jaccard(&a.genres, &b.genres);
    let tag_sim = jaccard(&a.tags, &b.tags);

    // Shared credits
    let credits_sim = if a.credits.is_empty() && b.credits.is_empty() {
        0.0
    } else {
        let shared: Vec<&String> = a.credits.intersection(&b.credits).collect();
        // Normalize: more shared credits = higher score, cap at 1.0
        (shared.len() as f64 / 3.0).min(1.0)
    };
    let shared_credits: Vec<&String> = a.credits.intersection(&b.credits).collect();

    // Year proximity
    let year_sim = match (a.year, b.year) {
        (Some(ya), Some(yb)) => (-(ya - yb).abs() as f64 / 10.0).exp(),
        _ => 0.0,
    };

    // Label match
    let label_sim = match (&a.label, &b.label) {
        (Some(la), Some(lb)) if la.to_lowercase() == lb.to_lowercase() => 1.0,
        _ => 0.0,
    };

    // Bliss audio similarity (cosine)
    let bliss_sim = match (&a.bliss_centroid, &b.bliss_centroid) {
        (Some(ca), Some(cb)) if ca.len() == cb.len() => cosine_similarity(ca, cb).max(0.0),
        _ => 0.0,
    };

    // Weighted score
    let score = (style_sim * 0.30
        + genre_sim * 0.20
        + tag_sim * 0.15
        + credits_sim * 0.15
        + year_sim * 0.10
        + label_sim * 0.05
        + bliss_sim * 0.05)
        .clamp(0.0, 1.0);

    // Build reason from top contributing signals
    let mut contributions: Vec<(f64, String)> = Vec::new();

    if style_sim > 0.0 {
        let shared: Vec<&String> = a.styles.intersection(&b.styles).collect();
        let names: Vec<&str> = shared.iter().map(|s| s.as_str()).take(3).collect();
        contributions.push((
            style_sim * 0.30,
            format!("Shares {} styles", names.join(", ")),
        ));
    }
    if genre_sim > 0.0 {
        let shared: Vec<&String> = a.genres.intersection(&b.genres).collect();
        let names: Vec<&str> = shared.iter().map(|s| s.as_str()).take(3).collect();
        contributions.push((
            genre_sim * 0.20,
            format!("Both {} genre", names.join(", ")),
        ));
    }
    if !shared_credits.is_empty() {
        let names: Vec<&str> = shared_credits.iter().map(|s| s.as_str()).take(2).collect();
        contributions.push((
            credits_sim * 0.15,
            format!("Common personnel: {}", names.join(", ")),
        ));
    }
    if label_sim > 0.0 {
        let label = a.label.as_deref().unwrap_or("");
        let era = match (a.year, b.year) {
            (Some(ya), Some(yb)) if (ya / 10) == (yb / 10) => {
                format!(" from the {}0s", ya / 10)
            }
            _ => String::new(),
        };
        contributions.push((label_sim * 0.05, format!("Both on {label}{era}")));
    }
    if tag_sim > 0.0 {
        contributions.push((tag_sim * 0.15, "Similar mood and sonic character".into()));
    }
    if bliss_sim > 0.3 {
        contributions.push((bliss_sim * 0.05, "Similar sonic profile".into()));
    }

    contributions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let reason = if contributions.is_empty() {
        "Related album".into()
    } else {
        // Take the top 1-2 reasons
        contributions
            .iter()
            .take(2)
            .map(|(_, desc)| desc.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    };

    (score, reason)
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

fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let mag_b: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

// ─── Artist Recommendations ─────────────────────────────────────────────────

struct ArtistFeatures {
    id: String,
    genres: HashSet<String>,
    styles: HashSet<String>,
    tags: HashSet<String>,
    credits: HashSet<String>,
    labels: HashSet<String>,
    year_range: Option<(i32, i32)>,
    bliss_centroid: Option<Vec<f64>>,
}

/// Generate artist recommendations using metadata similarity (no AI needed).
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
    // Get distinct artists from qualifying albums
    let artist_query = format!(
        "SELECT DISTINCT a.artist_id \
         FROM albums a \
         JOIN libraries l ON a.library_id = l.id \
         WHERE {where_clause}"
    );

    let artist_ids: Vec<(String,)> = if let Some(param) = bind_param {
        sqlx::query_as(&artist_query)
            .bind(param)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as(&artist_query).fetch_all(pool).await?
    };

    // Fetch all album metadata grouped by artist
    let album_rows: Vec<(String, String, String, Option<String>, Option<i32>, String, String, String)> =
        sqlx::query_as(
            "SELECT artist_id, genre, style, label, year, moods, descriptors, keywords \
             FROM albums"
        )
        .fetch_all(pool)
        .await?;

    let mut artist_genres: HashMap<String, HashSet<String>> = HashMap::new();
    let mut artist_styles: HashMap<String, HashSet<String>> = HashMap::new();
    let mut artist_tags: HashMap<String, HashSet<String>> = HashMap::new();
    let mut artist_labels: HashMap<String, HashSet<String>> = HashMap::new();
    let mut artist_years: HashMap<String, Vec<i32>> = HashMap::new();

    for (artist_id, genre_json, style_json, label, year, moods_json, descriptors_json, keywords_json) in &album_rows {
        let genres = parse_json_set(genre_json);
        let styles = parse_json_set(style_json);
        let moods = parse_json_set(moods_json);
        let descriptors = parse_json_set(descriptors_json);
        let keywords = parse_json_set(keywords_json);

        artist_genres.entry(artist_id.clone()).or_default().extend(genres);
        artist_styles.entry(artist_id.clone()).or_default().extend(styles);

        let entry = artist_tags.entry(artist_id.clone()).or_default();
        entry.extend(moods);
        entry.extend(descriptors);
        entry.extend(keywords);

        if let Some(l) = label {
            if !l.is_empty() {
                artist_labels
                    .entry(artist_id.clone())
                    .or_default()
                    .insert(l.to_lowercase());
            }
        }
        if let Some(y) = year {
            artist_years.entry(artist_id.clone()).or_default().push(*y);
        }
    }

    // Fetch credits: artist names that appear on each artist's albums
    let credits_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.artist_id, LOWER(c.artist_name) \
         FROM album_credits c \
         JOIN albums a ON c.album_id = a.id",
    )
    .fetch_all(pool)
    .await?;

    let mut artist_credits: HashMap<String, HashSet<String>> = HashMap::new();
    for (artist_id, credit_name) in credits_rows {
        artist_credits
            .entry(artist_id)
            .or_default()
            .insert(credit_name);
    }

    // Fetch bliss centroids per artist (average of all track features)
    let bliss_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.artist_id, t.bliss_features \
         FROM tracks t \
         JOIN albums a ON t.album_id = a.id \
         WHERE t.bliss_features IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut bliss_vecs: HashMap<String, Vec<Vec<f64>>> = HashMap::new();
    for (artist_id, features_json) in &bliss_rows {
        if let Ok(features) = serde_json::from_str::<Vec<f64>>(features_json) {
            if !features.is_empty() {
                bliss_vecs
                    .entry(artist_id.clone())
                    .or_default()
                    .push(features);
            }
        }
    }

    let target_ids: HashSet<&str> = artist_ids.iter().map(|(id,)| id.as_str()).collect();

    let mut artists = Vec::with_capacity(artist_ids.len());
    for (artist_id,) in &artist_ids {
        if !target_ids.contains(artist_id.as_str()) {
            continue;
        }

        let genres = artist_genres.remove(artist_id).unwrap_or_default();
        let styles = artist_styles.remove(artist_id).unwrap_or_default();
        let tags = artist_tags.remove(artist_id).unwrap_or_default();
        let credits = artist_credits.remove(artist_id).unwrap_or_default();
        let labels = artist_labels.remove(artist_id).unwrap_or_default();

        let year_range = artist_years.remove(artist_id).and_then(|years| {
            let min = years.iter().copied().min()?;
            let max = years.iter().copied().max()?;
            Some((min, max))
        });

        let bliss_centroid = bliss_vecs.remove(artist_id).map(|vecs| {
            let n = vecs.len() as f64;
            let dim = vecs[0].len();
            let mut centroid = vec![0.0_f64; dim];
            for v in &vecs {
                for (i, val) in v.iter().enumerate() {
                    if i < dim {
                        centroid[i] += val;
                    }
                }
            }
            for c in &mut centroid {
                *c /= n;
            }
            centroid
        });

        artists.push(ArtistFeatures {
            id: artist_id.clone(),
            genres,
            styles,
            tags,
            credits,
            labels,
            year_range,
            bliss_centroid,
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
        "generating metadata-based artist recommendations for {} of {} artists",
        target_ids.len(),
        artists.len()
    );

    // Build genre->artist index for pre-filtering
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

        // Pre-filter: find candidate artists that share at least one genre or style
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
        if candidates.is_empty() {
            candidates.extend(0..artists.len());
        }
        candidates.remove(&i);

        let mut scored: Vec<(f64, String, usize)> = Vec::new();
        for &j in &candidates {
            let other = &artists[j];
            let (score, reason) = compute_artist_similarity(artist, other);
            if score >= 0.15 {
                scored.push((score, reason, j));
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

/// Compute similarity between two artists using aggregated metadata from their albums.
fn compute_artist_similarity(a: &ArtistFeatures, b: &ArtistFeatures) -> (f64, String) {
    let style_sim = jaccard(&a.styles, &b.styles);
    let genre_sim = jaccard(&a.genres, &b.genres);
    let tag_sim = jaccard(&a.tags, &b.tags);

    // Shared credits (collaborators that appear on both artists' albums)
    let credits_sim = if a.credits.is_empty() && b.credits.is_empty() {
        0.0
    } else {
        let shared: Vec<&String> = a.credits.intersection(&b.credits).collect();
        (shared.len() as f64 / 3.0).min(1.0)
    };
    let shared_credits: Vec<&String> = a.credits.intersection(&b.credits).collect();

    // Era overlap: how much their active periods overlap
    let era_sim = match (&a.year_range, &b.year_range) {
        (Some((a_min, a_max)), Some((b_min, b_max))) => {
            let overlap_start = (*a_min).max(*b_min);
            let overlap_end = (*a_max).min(*b_max);
            let overlap = (overlap_end - overlap_start + 1).max(0) as f64;
            let span = ((*a_max).max(*b_max) - (*a_min).min(*b_min) + 1).max(1) as f64;
            (overlap / span).min(1.0)
        }
        _ => 0.0,
    };

    // Label overlap
    let label_sim = jaccard(&a.labels, &b.labels);

    // Bliss audio similarity
    let bliss_sim = match (&a.bliss_centroid, &b.bliss_centroid) {
        (Some(ca), Some(cb)) if ca.len() == cb.len() => cosine_similarity(ca, cb).max(0.0),
        _ => 0.0,
    };

    let score = (style_sim * 0.30
        + genre_sim * 0.20
        + tag_sim * 0.15
        + credits_sim * 0.15
        + era_sim * 0.10
        + label_sim * 0.05
        + bliss_sim * 0.05)
        .clamp(0.0, 1.0);

    // Build reason
    let mut contributions: Vec<(f64, String)> = Vec::new();

    if style_sim > 0.0 {
        let shared: Vec<&String> = a.styles.intersection(&b.styles).collect();
        let names: Vec<&str> = shared.iter().map(|s| s.as_str()).take(3).collect();
        contributions.push((
            style_sim * 0.30,
            format!("Shares {} styles", names.join(", ")),
        ));
    }
    if genre_sim > 0.0 {
        let shared: Vec<&String> = a.genres.intersection(&b.genres).collect();
        let names: Vec<&str> = shared.iter().map(|s| s.as_str()).take(3).collect();
        contributions.push((
            genre_sim * 0.20,
            format!("Both {} genre", names.join(", ")),
        ));
    }
    if !shared_credits.is_empty() {
        let names: Vec<&str> = shared_credits.iter().map(|s| s.as_str()).take(2).collect();
        contributions.push((
            credits_sim * 0.15,
            format!("Shared collaborators: {}", names.join(", ")),
        ));
    }
    if label_sim > 0.0 {
        let shared: Vec<&String> = a.labels.intersection(&b.labels).collect();
        let names: Vec<&str> = shared.iter().map(|s| s.as_str()).take(2).collect();
        contributions.push((label_sim * 0.05, format!("Both on {}", names.join(", "))));
    }
    if era_sim > 0.5 {
        contributions.push((era_sim * 0.10, "Active in the same era".into()));
    }
    if tag_sim > 0.0 {
        contributions.push((tag_sim * 0.15, "Similar mood and sonic character".into()));
    }
    if bliss_sim > 0.3 {
        contributions.push((bliss_sim * 0.05, "Similar sonic profile".into()));
    }

    contributions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let reason = if contributions.is_empty() {
        "Related artist".into()
    } else {
        contributions
            .iter()
            .take(2)
            .map(|(_, desc)| desc.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    };

    (score, reason)
}
