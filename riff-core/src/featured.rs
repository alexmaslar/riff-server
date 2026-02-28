use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::hash::{Hash, Hasher};

// Scoring constants
const SCORE_FAVORITE: f64 = 3.0;
const SCORE_PLAY_CAP: f64 = 2.0;
const SCORE_HAS_COVER: f64 = 1.0;
const SCORE_NEWNESS_MAX: f64 = 2.0;
const SCORE_NEWNESS_DAYS: f64 = 30.0;
const SCORE_COMPILATION_PENALTY: f64 = 1.5;
const SCORE_SKIP_PENALTY_MAX: f64 = 2.0;
const SCORE_SKIP_MIN_PLAYS: i64 = 5;
const SCORE_COOLDOWN_MAX: f64 = 2.0;
const SCORE_COOLDOWN_DAYS: f64 = 3.0;
const SCORE_DEFAULT_RATING: f64 = 5.0;
const TOP_N: usize = 10;

/// Score all albums for a user, returning (album_id, score) sorted descending, truncated to TOP_N.
/// `library_ids_json` is a JSON array of library IDs to scope the query (from `resolve_library_ids`).
async fn score_albums(pool: &SqlitePool, user_id: &str, library_ids_json: &str) -> Result<Vec<(String, f64)>> {
    let rows = sqlx::query(
        "SELECT
            a.id,
            a.rating,
            a.cover_art_path IS NOT NULL AS has_cover,
            a.is_compilation,
            julianday('now') - julianday(a.added_at) AS days_since_added,
            COALESCE(f.fav, 0) AS is_favorited,
            COALESCE(ph.play_count, 0) AS play_count,
            COALESCE(ph.skip_rate, 0.0) AS skip_rate,
            COALESCE(ph.days_since_last_play, 999.0) AS days_since_last_play
         FROM albums a
         LEFT JOIN (
             SELECT entity_id, 1 AS fav
             FROM favorites
             WHERE user_id = ? AND entity_type = 'album'
         ) f ON f.entity_id = a.id
         LEFT JOIN (
             SELECT t.album_id,
                    COUNT(*) AS play_count,
                    CAST(SUM(CASE WHEN ph.completed = 0 THEN 1 ELSE 0 END) AS REAL) / COUNT(*) AS skip_rate,
                    julianday('now') - julianday(MAX(ph.played_at)) AS days_since_last_play
             FROM play_history ph
             JOIN tracks t ON ph.track_id = t.id
             WHERE ph.user_id = ?
             GROUP BY t.album_id
         ) ph ON ph.album_id = a.id
         WHERE a.library_id IN (SELECT value FROM json_each(?))",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(vec![]);
    }

    let mut scored: Vec<(String, f64)> = rows
        .iter()
        .map(|row| {
            let id: String = row.get("id");
            let album_rating: Option<f64> = row.get("rating");
            let has_cover: bool = row.get("has_cover");
            let is_compilation: bool = row.get("is_compilation");
            let days_since_added: f64 = row.get("days_since_added");
            let is_favorited: i32 = row.get("is_favorited");
            let play_count: i64 = row.get("play_count");
            let skip_rate: f64 = row.get("skip_rate");
            let days_since_last_play: f64 = row.get("days_since_last_play");

            let mut score = album_rating.unwrap_or(SCORE_DEFAULT_RATING);

            if is_favorited != 0 {
                score += SCORE_FAVORITE;
            }

            // Play engagement: ln(1 + play_count) capped
            score += ((1.0 + play_count as f64).ln()).min(SCORE_PLAY_CAP);

            if has_cover {
                score += SCORE_HAS_COVER;
            }

            // Newness bonus
            score += SCORE_NEWNESS_MAX
                * (1.0 - days_since_added / SCORE_NEWNESS_DAYS).max(0.0);

            if is_compilation {
                score -= SCORE_COMPILATION_PENALTY;
            }

            // Skip penalty (only if enough plays)
            if play_count >= SCORE_SKIP_MIN_PLAYS {
                score -= skip_rate * SCORE_SKIP_PENALTY_MAX;
            }

            // Cooldown: penalize recently played albums
            score -= SCORE_COOLDOWN_MAX
                * (1.0 - days_since_last_play / SCORE_COOLDOWN_DAYS).max(0.0);

            (id, score)
        })
        .collect();

    // Sort descending by score
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top N candidates
    scored.truncate(TOP_N);

    Ok(scored)
}

// Artist scoring constants
const ARTIST_BASE_SCORE: f64 = 5.0;
const ARTIST_SCORE_HAS_BIO: f64 = 1.5;
const ARTIST_SCORE_HAS_IMAGE: f64 = 1.0;
const ARTIST_SCORE_ALBUM_DEPTH: f64 = 0.5;
const ARTIST_ALBUM_DEPTH_MIN: i64 = 3;

/// Score all artists for a user, returning (artist_id, score) sorted descending, truncated to TOP_N.
/// `library_ids_json` is a JSON array of library IDs to scope the query.
async fn score_artists(pool: &SqlitePool, user_id: &str, library_ids_json: &str) -> Result<Vec<(String, f64)>> {
    let rows = sqlx::query(
        "SELECT
            ar.id,
            ar.bio IS NOT NULL AS has_bio,
            ar.image_url IS NOT NULL AS has_image,
            COUNT(DISTINCT a.id) AS album_count,
            MIN(julianday('now') - julianday(a.added_at)) AS min_days_since_added,
            COALESCE(f.fav, 0) AS is_favorited,
            COALESCE(ph.play_count, 0) AS play_count,
            COALESCE(ph.skip_rate, 0.0) AS skip_rate,
            COALESCE(ph.days_since_last_play, 999.0) AS days_since_last_play
         FROM artists ar
         JOIN albums a ON a.artist_id = ar.id
         LEFT JOIN (
             SELECT entity_id, 1 AS fav
             FROM favorites
             WHERE user_id = ? AND entity_type = 'artist'
         ) f ON f.entity_id = ar.id
         LEFT JOIN (
             SELECT ar2.id AS artist_id,
                    COUNT(*) AS play_count,
                    CAST(SUM(CASE WHEN ph.completed = 0 THEN 1 ELSE 0 END) AS REAL) / COUNT(*) AS skip_rate,
                    julianday('now') - julianday(MAX(ph.played_at)) AS days_since_last_play
             FROM play_history ph
             JOIN tracks t ON ph.track_id = t.id
             JOIN albums a2 ON t.album_id = a2.id
             JOIN artists ar2 ON a2.artist_id = ar2.id
             WHERE ph.user_id = ?
             GROUP BY ar2.id
         ) ph ON ph.artist_id = ar.id
         WHERE ar.library_id IN (SELECT value FROM json_each(?))
         GROUP BY ar.id",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(vec![]);
    }

    let mut scored: Vec<(String, f64)> = rows
        .iter()
        .map(|row| {
            let id: String = row.get("id");
            let has_bio: bool = row.get("has_bio");
            let has_image: bool = row.get("has_image");
            let album_count: i64 = row.get("album_count");
            let min_days_since_added: Option<f64> = row.get("min_days_since_added");
            let is_favorited: i32 = row.get("is_favorited");
            let play_count: i64 = row.get("play_count");
            let skip_rate: f64 = row.get("skip_rate");
            let days_since_last_play: f64 = row.get("days_since_last_play");

            let mut score = ARTIST_BASE_SCORE;

            if is_favorited != 0 {
                score += SCORE_FAVORITE;
            }

            // Play engagement: ln(1 + total_track_plays) capped
            score += ((1.0 + play_count as f64).ln()).min(SCORE_PLAY_CAP);

            if has_bio {
                score += ARTIST_SCORE_HAS_BIO;
            }
            if has_image {
                score += ARTIST_SCORE_HAS_IMAGE;
            }

            // Newness bonus based on most recently added album
            if let Some(days) = min_days_since_added {
                score += SCORE_NEWNESS_MAX * (1.0 - days / SCORE_NEWNESS_DAYS).max(0.0);
            }

            // Skip penalty (only if enough plays)
            if play_count >= SCORE_SKIP_MIN_PLAYS {
                score -= skip_rate * SCORE_SKIP_PENALTY_MAX;
            }

            // Cooldown: penalize recently played artists
            score -= SCORE_COOLDOWN_MAX
                * (1.0 - days_since_last_play / SCORE_COOLDOWN_DAYS).max(0.0);

            // Album depth: reward well-represented artists
            if album_count >= ARTIST_ALBUM_DEPTH_MIN {
                score += ARTIST_SCORE_ALBUM_DEPTH;
            }

            (id, score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(TOP_N);

    Ok(scored)
}

/// Pick N unique items from scored candidates using deterministic per-slot hashing.
fn pick_n_from_candidates(
    candidates: &[(String, f64)],
    user_id: &str,
    date_str: &str,
    count: usize,
) -> Vec<String> {
    let want = count.min(candidates.len());
    let mut picked_indices: Vec<usize> = Vec::with_capacity(want);
    for slot in 0..want {
        let seed_str = format!("{}~{}", date_str, slot);
        let mut idx = seed_index(user_id, &seed_str, candidates.len());
        let mut attempts = 0;
        while picked_indices.contains(&idx) && attempts < candidates.len() {
            idx = (idx + 1) % candidates.len();
            attempts += 1;
        }
        picked_indices.push(idx);
    }
    picked_indices.iter().map(|&i| candidates[i].0.clone()).collect()
}

/// Pick N featured album IDs for a user on a given date.
/// `library_ids_json` scopes which libraries to consider.
pub async fn pick_featured_album_ids(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    count: usize,
    library_ids_json: &str,
) -> Result<Vec<String>> {
    if count == 0 {
        return Ok(vec![]);
    }
    let candidates = score_albums(pool, user_id, library_ids_json).await?;
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    Ok(pick_n_from_candidates(&candidates, user_id, date_str, count))
}

/// Pick the best featured album for a user on a given date.
/// Returns the album ID, or None if the library is empty.
pub async fn pick_featured_album_id(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    library_ids_json: &str,
) -> Result<Option<String>> {
    let ids = pick_featured_album_ids(pool, user_id, date_str, 1, library_ids_json).await?;
    Ok(ids.into_iter().next())
}

/// Pick N featured artist IDs for a user on a given date.
/// `library_ids_json` scopes which libraries to consider.
pub async fn pick_featured_artist_ids(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    count: usize,
    library_ids_json: &str,
) -> Result<Vec<String>> {
    if count == 0 {
        return Ok(vec![]);
    }
    let candidates = score_artists(pool, user_id, library_ids_json).await?;
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    Ok(pick_n_from_candidates(&candidates, user_id, date_str, count))
}

/// Pick the best featured artist for a user on a given date.
/// Returns the artist ID, or None if the library is empty.
pub async fn pick_featured_artist_id(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    library_ids_json: &str,
) -> Result<Option<String>> {
    let ids = pick_featured_artist_ids(pool, user_id, date_str, 1, library_ids_json).await?;
    Ok(ids.into_iter().next())
}

fn seed_index(user_id: &str, date_str: &str, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let mut hasher = std::hash::DefaultHasher::new();
    user_id.hash(&mut hasher);
    date_str.hash(&mut hasher);
    let h = hasher.finish();
    (h as usize) % len
}
