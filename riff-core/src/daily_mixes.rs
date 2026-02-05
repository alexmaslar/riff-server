use anyhow::Result;
use chrono::{Datelike, NaiveDate, Utc};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use uuid::Uuid;

const MAX_TRACKS_PER_MIX: usize = 25;
const MAX_TRACKS_PER_ALBUM: usize = 3;
const MAX_TRACKS_PER_ARTIST: usize = 5;
const MIN_DISTINCT_ARTISTS: usize = 3;

struct MixTrack {
    id: String,
    artist_id: String,
    album_id: String,
    bpm: Option<f64>,
    key: Option<String>,
    loudness: Option<f64>,
}

/// Generate all 4 daily mixes for a single user.
pub async fn generate_daily_mixes(pool: &SqlitePool, user_id: &str, date: NaiveDate) -> Result<()> {
    let date_str = date.format("%Y-%m-%d").to_string();

    // Clean up old cover files before deleting mixes
    let old_covers = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT cover_path FROM daily_mixes WHERE user_id = ? AND mix_date = ?",
    )
    .bind(user_id)
    .bind(&date_str)
    .fetch_all(pool)
    .await?;

    for (cover_path,) in old_covers {
        if let Some(path) = cover_path {
            let _ = std::fs::remove_file(&path);
        }
    }

    // Delete any existing mixes for this user+date
    sqlx::query("DELETE FROM daily_mixes WHERE user_id = ? AND mix_date = ?")
        .bind(user_id)
        .bind(&date_str)
        .execute(pool)
        .await?;

    let day_of_year = date.ordinal() as usize;

    // Collect IDs already used across mixes today (for cross-mix dedup)
    let mut used_track_ids: Vec<String> = Vec::new();

    // Mix 1: Artist Mix
    if let Err(e) = generate_artist_mix(pool, user_id, &date_str, day_of_year, &mut used_track_ids).await {
        warn!("artist mix generation failed for {user_id}: {e}");
    }

    // Mix 2: Genre Mix
    if let Err(e) = generate_genre_mix(pool, user_id, &date_str, day_of_year, &mut used_track_ids).await {
        warn!("genre mix generation failed for {user_id}: {e}");
    }

    // Mix 3: Deep Cuts
    if let Err(e) = generate_deep_cuts_mix(pool, user_id, &date_str, day_of_year, &mut used_track_ids).await {
        warn!("deep cuts mix generation failed for {user_id}: {e}");
    }

    // Mix 4: Decade Mix
    if let Err(e) = generate_decade_mix(pool, user_id, &date_str, day_of_year, &mut used_track_ids).await {
        warn!("decade mix generation failed for {user_id}: {e}");
    }

    Ok(())
}

/// Generate daily mixes for all users.
pub async fn generate_all_daily_mixes(pool: &SqlitePool) -> Result<()> {
    let date = Utc::now().date_naive();

    let users = sqlx::query_as::<_, (String,)>("SELECT id FROM users")
        .fetch_all(pool)
        .await?;

    info!("generating daily mixes for {} users", users.len());

    for (user_id,) in &users {
        // Skip if mixes already exist for today
        let existing = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM daily_mixes WHERE user_id = ? AND mix_date = ?",
        )
        .bind(user_id)
        .bind(date.format("%Y-%m-%d").to_string())
        .fetch_one(pool)
        .await?;

        if existing.0 > 0 {
            // Backfill covers for mixes missing artwork
            let uncovered = sqlx::query_as::<_, (String,)>(
                "SELECT id FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND cover_path IS NULL",
            )
            .bind(user_id)
            .bind(date.format("%Y-%m-%d").to_string())
            .fetch_all(pool)
            .await?;

            for (mix_id,) in uncovered {
                if let Err(e) = generate_mix_cover(pool, &mix_id).await {
                    warn!("backfill cover failed for mix {mix_id}: {e}");
                }
            }
            continue;
        }

        if let Err(e) = generate_daily_mixes(pool, user_id, date).await {
            warn!("daily mix generation failed for user {user_id}: {e}");
        }
    }

    info!("daily mix generation complete");
    Ok(())
}

// ─── Artist Mix ──────────────────────────────────────────────────────────────

async fn generate_artist_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    day_of_year: usize,
    used_track_ids: &mut Vec<String>,
) -> Result<()> {
    // Find top-played artists for this user
    let top_artists = sqlx::query(
        "SELECT ar.id, ar.name, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ? AND ph.completed = 1
         GROUP BY ar.id
         ORDER BY plays DESC
         LIMIT 20",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if top_artists.is_empty() {
        // Fallback: pick from artists with highest-rated albums
        let fallback_artists = sqlx::query(
            "SELECT ar.id, ar.name
             FROM artists ar
             JOIN albums a ON a.artist_id = ar.id
             WHERE a.ai_rating IS NOT NULL
             ORDER BY a.ai_rating DESC
             LIMIT 20",
        )
        .fetch_all(pool)
        .await?;

        if fallback_artists.is_empty() {
            return Ok(());
        }

        let idx = day_of_year % fallback_artists.len();
        let artist_id: String = fallback_artists[idx].get("id");
        let artist_name: String = fallback_artists[idx].get("name");

        return build_artist_mix(pool, user_id, date_str, &artist_id, &artist_name, used_track_ids).await;
    }

    let idx = day_of_year % top_artists.len();
    let artist_id: String = top_artists[idx].get("id");
    let artist_name: String = top_artists[idx].get("name");

    build_artist_mix(pool, user_id, date_str, &artist_id, &artist_name, used_track_ids).await
}

async fn build_artist_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    seed_artist_id: &str,
    seed_artist_name: &str,
    used_track_ids: &mut Vec<String>,
) -> Result<()> {
    // Get the seed artist's genres/styles
    let artist_genres = sqlx::query(
        "SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.genre) j
         WHERE a.artist_id = ?
         UNION
         SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.style) j
         WHERE a.artist_id = ?",
    )
    .bind(seed_artist_id)
    .bind(seed_artist_id)
    .fetch_all(pool)
    .await?;

    let genres: Vec<String> = artist_genres.iter().map(|r| r.get("genre")).collect();

    // Find tracks from the same genre/style family
    let candidate_tracks = if genres.is_empty() {
        // Fallback: all tracks from this artist + random others
        sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    COALESCE(a.ai_rating, 5.0) as rating,
                    a.play_count
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             ORDER BY CASE WHEN a.artist_id = ? THEN 0 ELSE 1 END, rating DESC
             LIMIT 100",
        )
        .bind(seed_artist_id)
        .fetch_all(pool)
        .await?
    } else {
        // Build genre match condition
        let placeholders: Vec<String> = genres.iter().enumerate().map(|(i, _)| format!("?{}", i + 2)).collect();
        let genre_list = placeholders.join(", ");

        let query = format!(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    COALESCE(a.ai_rating, 5.0) as rating,
                    a.play_count
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE a.id IN (
                 SELECT DISTINCT a2.id FROM albums a2, json_each(a2.genre) jg
                 WHERE jg.value IN ({genre_list})
                 UNION
                 SELECT DISTINCT a3.id FROM albums a3, json_each(a3.style) js
                 WHERE js.value IN ({genre_list})
             )
             ORDER BY CASE WHEN a.artist_id = ? THEN 0 ELSE 1 END, rating DESC
             LIMIT 200"
        );

        let mut q = sqlx::query(&query).bind(seed_artist_id);
        for genre in &genres {
            q = q.bind(genre);
        }
        q = q.bind(seed_artist_id);
        q.fetch_all(pool).await?
    };

    let title = format!("{} Mix", seed_artist_name);
    let description = if genres.is_empty() {
        format!("Tracks inspired by {}", seed_artist_name)
    } else {
        genres.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
    };

    let selected = score_and_select(
        &candidate_tracks,
        pool,
        user_id,
        used_track_ids,
    )
    .await?;

    if selected.is_empty() {
        return Ok(());
    }

    insert_mix(
        pool,
        user_id,
        date_str,
        "artist",
        &title,
        &description,
        &format!("artist:{seed_artist_id}"),
        &selected,
    )
    .await?;

    used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
    Ok(())
}

// ─── Genre Mix ───────────────────────────────────────────────────────────────

async fn generate_genre_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    day_of_year: usize,
    used_track_ids: &mut Vec<String>,
) -> Result<()> {
    // Find most-played genres
    let genre_rows = sqlx::query(
        "SELECT j.value as genre_name, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id,
         json_each(CASE WHEN a.genre = '[]' OR a.genre IS NULL THEN '[\"Unknown\"]' ELSE a.genre END) j
         WHERE ph.user_id = ? AND ph.completed = 1
         GROUP BY j.value
         ORDER BY plays DESC
         LIMIT 20",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let genres: Vec<String> = if genre_rows.is_empty() {
        // Fallback: most common genres in library
        let lib_genres = sqlx::query(
            "SELECT j.value as genre_name, COUNT(*) as cnt
             FROM albums a, json_each(a.genre) j
             GROUP BY j.value
             ORDER BY cnt DESC
             LIMIT 20",
        )
        .fetch_all(pool)
        .await?;
        lib_genres.iter().map(|r| r.get("genre_name")).collect()
    } else {
        genre_rows.iter().map(|r| r.get("genre_name")).collect()
    };

    if genres.is_empty() {
        return Ok(());
    }

    let idx = day_of_year % genres.len();
    let seed_genre = &genres[idx];

    // Get tracks from this genre, diverse artists
    let candidate_tracks = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                COALESCE(a.ai_rating, 5.0) as rating,
                a.play_count
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE a.id IN (
             SELECT DISTINCT a2.id FROM albums a2, json_each(a2.genre) jg
             WHERE jg.value = ?
             UNION
             SELECT DISTINCT a3.id FROM albums a3, json_each(a3.style) js
             WHERE js.value = ?
         )
         ORDER BY rating DESC
         LIMIT 200",
    )
    .bind(seed_genre)
    .bind(seed_genre)
    .fetch_all(pool)
    .await?;

    let selected = score_and_select(&candidate_tracks, pool, user_id, used_track_ids).await?;

    if selected.is_empty() {
        return Ok(());
    }

    let title = format!("{} Mix", seed_genre);
    let description = format!("A mix of {} from your library", seed_genre);

    insert_mix(
        pool, user_id, date_str, "genre", &title, &description,
        &format!("genre:{seed_genre}"), &selected,
    )
    .await?;

    used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
    Ok(())
}

// ─── Deep Cuts ───────────────────────────────────────────────────────────────

async fn generate_deep_cuts_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    _day_of_year: usize,
    used_track_ids: &mut Vec<String>,
) -> Result<()> {
    // Tracks from well-rated albums that have never been played by this user
    let candidate_tracks = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                COALESCE(a.ai_rating, 5.0) as rating,
                a.play_count
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id NOT IN (
             SELECT track_id FROM play_history WHERE user_id = ?
         )
         AND a.ai_rating IS NOT NULL
         AND a.ai_rating >= 6.0
         ORDER BY a.ai_rating DESC, RANDOM()
         LIMIT 200",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if candidate_tracks.is_empty() {
        // Fallback: rarely played tracks
        let fallback = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    COALESCE(a.ai_rating, 5.0) as rating,
                    a.play_count
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE a.play_count <= 1
             ORDER BY COALESCE(a.ai_rating, 5.0) DESC, RANDOM()
             LIMIT 200",
        )
        .fetch_all(pool)
        .await?;

        let selected = score_and_select(&fallback, pool, user_id, used_track_ids).await?;
        if selected.is_empty() {
            return Ok(());
        }

        insert_mix(
            pool, user_id, date_str, "deep_cuts", "Deep Cuts",
            "Hidden gems from your library", "deep_cuts:", &selected,
        )
        .await?;

        used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
        return Ok(());
    }

    let selected = score_and_select(&candidate_tracks, pool, user_id, used_track_ids).await?;
    if selected.is_empty() {
        return Ok(());
    }

    insert_mix(
        pool, user_id, date_str, "deep_cuts", "Deep Cuts",
        "Hidden gems from your library", "deep_cuts:", &selected,
    )
    .await?;

    used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
    Ok(())
}

// ─── Decade Mix ──────────────────────────────────────────────────────────────

async fn generate_decade_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    day_of_year: usize,
    used_track_ids: &mut Vec<String>,
) -> Result<()> {
    // Find most-played decades
    let decade_rows = sqlx::query(
        "SELECT (a.year / 10 * 10) as decade, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE ph.user_id = ? AND ph.completed = 1 AND a.year IS NOT NULL
         GROUP BY decade
         ORDER BY plays DESC
         LIMIT 10",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let decades: Vec<i32> = if decade_rows.is_empty() {
        // Fallback: most common decades in library
        let lib_decades = sqlx::query(
            "SELECT (year / 10 * 10) as decade, COUNT(*) as cnt
             FROM albums
             WHERE year IS NOT NULL
             GROUP BY decade
             ORDER BY cnt DESC
             LIMIT 10",
        )
        .fetch_all(pool)
        .await?;
        lib_decades.iter().map(|r| r.get("decade")).collect()
    } else {
        decade_rows.iter().map(|r| r.get("decade")).collect()
    };

    if decades.is_empty() {
        return Ok(());
    }

    let idx = day_of_year % decades.len();
    let seed_decade = decades[idx];
    let decade_end = seed_decade + 9;

    let candidate_tracks = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                COALESCE(a.ai_rating, 5.0) as rating,
                a.play_count
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE a.year >= ? AND a.year <= ?
         ORDER BY rating DESC
         LIMIT 200",
    )
    .bind(seed_decade)
    .bind(decade_end)
    .fetch_all(pool)
    .await?;

    let selected = score_and_select(&candidate_tracks, pool, user_id, used_track_ids).await?;
    if selected.is_empty() {
        return Ok(());
    }

    let title = format!("{}s Mix", seed_decade);
    let description = format!("The best of the {}s from your library", seed_decade);

    insert_mix(
        pool, user_id, date_str, "decade", &title, &description,
        &format!("decade:{seed_decade}s"), &selected,
    )
    .await?;

    used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
    Ok(())
}

// ─── Scoring & Selection ─────────────────────────────────────────────────────

struct ScoredTrack {
    id: String,
    album_id: String,
    artist_id: String,
    score: f64,
    bpm: Option<f64>,
    key: Option<String>,
    loudness: Option<f64>,
}

async fn score_and_select(
    candidate_rows: &[sqlx::sqlite::SqliteRow],
    pool: &SqlitePool,
    user_id: &str,
    used_track_ids: &[String],
) -> Result<Vec<MixTrack>> {
    if candidate_rows.is_empty() {
        return Ok(Vec::new());
    }

    // Pre-fetch user's favorited album and artist IDs
    let fav_albums: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entity_id FROM favorites WHERE user_id = ? AND entity_type = 'album'",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let fav_artists: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entity_id FROM favorites WHERE user_id = ? AND entity_type = 'artist'",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    // Pre-fetch played track IDs
    let played_tracks: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT DISTINCT track_id FROM play_history WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let played_set: std::collections::HashSet<&str> = played_tracks.iter().map(|s| s.as_str()).collect();
    let fav_album_set: std::collections::HashSet<&str> = fav_albums.iter().map(|s| s.as_str()).collect();
    let fav_artist_set: std::collections::HashSet<&str> = fav_artists.iter().map(|s| s.as_str()).collect();
    let used_set: std::collections::HashSet<&str> = used_track_ids.iter().map(|s| s.as_str()).collect();

    let mut scored: Vec<ScoredTrack> = candidate_rows
        .iter()
        .map(|row| {
            let id: String = row.get("id");
            let album_id: String = row.get("album_id");
            let artist_id: String = row.get("artist_id");
            let rating: f64 = row.get("rating");
            let bpm_analyzed: Option<f64> = row.try_get("bpm_analyzed").ok().flatten();
            let bpm_tag: Option<f64> = row.try_get("bpm_tag").ok().flatten();
            let bpm = bpm_analyzed.or(bpm_tag);
            let key: Option<String> = row.try_get("key_analyzed").ok().flatten();
            let loudness: Option<f64> = row.try_get("loudness_lufs").ok().flatten();

            // Base score from AI rating (0-10 normalized)
            let mut score = rating;

            // Bonus for favorited album/artist
            if fav_album_set.contains(album_id.as_str()) {
                score += 2.0;
            }
            if fav_artist_set.contains(artist_id.as_str()) {
                score += 2.0;
            }

            // Bonus for unplayed tracks (discovery)
            if !played_set.contains(id.as_str()) {
                score += 1.0;
            }

            // Penalty for already used in another mix today
            if used_set.contains(id.as_str()) {
                score -= 5.0;
            }

            ScoredTrack { id, album_id, artist_id, score, bpm, key, loudness }
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Apply diversity constraints
    let mut selected: Vec<MixTrack> = Vec::new();
    let mut album_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut artist_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut artist_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    for track in &scored {
        if selected.len() >= MAX_TRACKS_PER_MIX {
            break;
        }

        let ac = album_count.get(&track.album_id).copied().unwrap_or(0);
        if ac >= MAX_TRACKS_PER_ALBUM {
            continue;
        }

        let arc = artist_count.get(&track.artist_id).copied().unwrap_or(0);
        if arc >= MAX_TRACKS_PER_ARTIST {
            continue;
        }

        *album_count.entry(track.album_id.clone()).or_insert(0) += 1;
        *artist_count.entry(track.artist_id.clone()).or_insert(0) += 1;
        artist_set.insert(track.artist_id.clone());

        selected.push(MixTrack {
            id: track.id.clone(),
            artist_id: track.artist_id.clone(),
            album_id: track.album_id.clone(),
            bpm: track.bpm,
            key: track.key.clone(),
            loudness: track.loudness,
        });
    }

    // Enforce minimum distinct artists
    if artist_set.len() < MIN_DISTINCT_ARTISTS && selected.len() < MIN_DISTINCT_ARTISTS {
        // Not enough diversity, but still return what we have
        return Ok(selected);
    }

    order_for_flow(&mut selected);

    Ok(selected)
}

// ─── Flow Ordering (greedy nearest-neighbor) ─────────────────────────────────

fn order_for_flow(tracks: &mut Vec<MixTrack>) {
    if tracks.len() < 2 {
        return;
    }

    // Find median BPM for the starting track
    let mut bpms: Vec<f64> = tracks.iter().filter_map(|t| t.bpm).collect();
    let median_bpm = if bpms.is_empty() {
        120.0 // sensible default
    } else {
        bpms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        bpms[bpms.len() / 2]
    };

    // Find the starting track: closest to median BPM
    let start_idx = tracks
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (a.bpm.unwrap_or(median_bpm) - median_bpm).abs();
            let db = (b.bpm.unwrap_or(median_bpm) - median_bpm).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0);

    let total = tracks.len();
    let mut ordered: Vec<MixTrack> = Vec::with_capacity(total);
    let mut remaining: Vec<MixTrack> = std::mem::take(tracks);

    // Move start track to ordered
    ordered.push(remaining.swap_remove(start_idx));

    // Compute min/max BPM for the energy arc target curve
    let min_bpm = bpms.first().copied().unwrap_or(100.0);
    let max_bpm = bpms.last().copied().unwrap_or(140.0);

    // Greedy: pick lowest-cost next track
    while !remaining.is_empty() {
        let pos_frac = ordered.len() as f64 / total as f64;
        let prev = ordered.last().unwrap();
        let second_prev = if ordered.len() >= 2 { Some(&ordered[ordered.len() - 2]) } else { None };

        // Target BPM from energy arc curve
        let target_bpm = if pos_frac < 0.4 {
            // Ramp median → high
            let t = pos_frac / 0.4;
            median_bpm + t * (max_bpm - median_bpm)
        } else if pos_frac < 0.7 {
            // Hold high (peak)
            max_bpm
        } else {
            // Ramp high → low
            let t = (pos_frac - 0.7) / 0.3;
            max_bpm - t * (max_bpm - min_bpm)
        };

        let mut best_idx = 0;
        let mut best_cost = f64::MAX;

        for (i, candidate) in remaining.iter().enumerate() {
            let mut cost = 0.0;

            // BPM distance (weight 1.0)
            if let (Some(prev_bpm), Some(cand_bpm)) = (prev.bpm, candidate.bpm) {
                cost += (prev_bpm - cand_bpm).abs() / 20.0;
            }

            // Key distance (weight 0.5)
            if let (Some(prev_key), Some(cand_key)) = (&prev.key, &candidate.key) {
                cost += 0.5 * camelot_distance(prev_key, cand_key);
            }

            // Loudness distance (weight 0.3)
            if let (Some(prev_lufs), Some(cand_lufs)) = (prev.loudness, candidate.loudness) {
                cost += 0.3 * (prev_lufs - cand_lufs).abs() / 10.0;
            }

            // Artist adjacency penalties
            if candidate.artist_id == prev.artist_id {
                cost += 10.0;
            } else if let Some(sp) = second_prev {
                if candidate.artist_id == sp.artist_id {
                    cost += 3.0;
                }
            }

            // Album adjacency penalties
            if candidate.album_id == prev.album_id {
                cost += 8.0;
            } else if let Some(sp) = second_prev {
                if candidate.album_id == sp.album_id {
                    cost += 2.0;
                }
            }

            // Energy arc bias (weight 0.2) — reward matching the target curve
            if let Some(cand_bpm) = candidate.bpm {
                cost += 0.2 * (cand_bpm - target_bpm).abs() / 20.0;
            }

            if cost < best_cost {
                best_cost = cost;
                best_idx = i;
            }
        }

        ordered.push(remaining.swap_remove(best_idx));
    }

    *tracks = ordered;
}

// ─── Camelot Wheel Key Compatibility ─────────────────────────────────────────

/// Map a key string to (camelot_number 1-12, is_minor).
/// Returns None for unrecognized keys.
fn camelot_position(key: &str) -> Option<(u8, bool)> {
    // Major keys → B ring
    // Minor keys → A ring (same number as relative major)
    match key {
        "C"   => Some((8, false)),
        "C#"  => Some((3, false)),
        "D"   => Some((10, false)),
        "Eb"  => Some((5, false)),
        "E"   => Some((12, false)),
        "F"   => Some((7, false)),
        "F#"  => Some((2, false)),
        "G"   => Some((9, false)),
        "Ab"  => Some((4, false)),
        "A"   => Some((11, false)),
        "Bb"  => Some((6, false)),
        "B"   => Some((1, false)),
        "Cm"  => Some((5, true)),
        "C#m" => Some((12, true)),
        "Dm"  => Some((7, true)),
        "Ebm" => Some((2, true)),
        "Em"  => Some((9, true)),
        "Fm"  => Some((4, true)),
        "F#m" => Some((11, true)),
        "Gm"  => Some((6, true)),
        "Abm" => Some((1, true)),
        "Am"  => Some((8, true)),
        "Bbm" => Some((3, true)),
        "Bm"  => Some((10, true)),
        _ => None,
    }
}

/// Distance between two keys on the Camelot wheel, normalized to 0.0–1.0.
/// Adjacent keys (±1 on wheel) = low distance. Opposite = high distance.
/// Crossing major↔minor adds +1 step (except relative major/minor = 0 steps + 1 crossing).
fn camelot_distance(key_a: &str, key_b: &str) -> f64 {
    let (pos_a, minor_a) = match camelot_position(key_a) {
        Some(p) => p,
        None => return 0.5, // neutral score for unknown keys
    };
    let (pos_b, minor_b) = match camelot_position(key_b) {
        Some(p) => p,
        None => return 0.5,
    };

    // Circular distance on the 12-position wheel
    let diff = (pos_a as i8 - pos_b as i8).unsigned_abs();
    let circular = diff.min(12 - diff) as f64;

    // Crossing major↔minor costs +1 (but relative major/minor at same number = just the crossing)
    let mode_penalty = if minor_a != minor_b { 1.0 } else { 0.0 };

    // Total distance, normalize: max possible = 6 (half wheel) + 1 (mode) = 7
    (circular + mode_penalty) / 7.0
}

// ─── Cover Generation ────────────────────────────────────────────────────────

async fn generate_mix_cover(pool: &SqlitePool, mix_id: &str) -> Result<Option<PathBuf>> {
    // Get the mix type for signature color tinting
    let mix_type: String = sqlx::query_as::<_, (String,)>(
        "SELECT mix_type FROM daily_mixes WHERE id = ?",
    )
    .bind(mix_id)
    .fetch_one(pool)
    .await?
    .0;

    // Get distinct album cover paths from the mix's tracks (up to 4)
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT DISTINCT a.cover_art_path
         FROM daily_mix_tracks dmt
         JOIN tracks t ON dmt.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE dmt.mix_id = ? AND a.cover_art_path IS NOT NULL
         ORDER BY dmt.sort_order
         LIMIT 4",
    )
    .bind(mix_id)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let cover_paths: Vec<PathBuf> = rows.iter().map(|(p,)| PathBuf::from(p)).collect();

    let mix_id_owned = mix_id.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let path_refs: Vec<&Path> = cover_paths.iter().map(|p| p.as_path()).collect();
        crate::artwork::generate_mix_mosaic(&path_refs, &mix_type)
    })
    .await??;

    // Save as JPEG to cache dir
    let cache_dir = crate::artwork::cache::get_cache_dir()?;
    let filename = format!("mix_{}.jpg", mix_id_owned);
    let file_path = cache_dir.join(&filename);

    let file = std::fs::File::create(&file_path)?;
    let mut buf = std::io::BufWriter::new(file);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    result.write_with_encoder(encoder)?;

    // Update the mix row with the cover path
    sqlx::query("UPDATE daily_mixes SET cover_path = ? WHERE id = ?")
        .bind(file_path.to_str().unwrap())
        .bind(&mix_id_owned)
        .execute(pool)
        .await?;

    info!("generated mosaic cover for mix {mix_id_owned}");
    Ok(Some(file_path))
}

// ─── DB Insert ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn insert_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    mix_type: &str,
    title: &str,
    description: &str,
    seed_value: &str,
    tracks: &[MixTrack],
) -> Result<()> {
    let mix_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO daily_mixes (id, user_id, mix_date, mix_type, title, description, seed_value)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&mix_id)
    .bind(user_id)
    .bind(date_str)
    .bind(mix_type)
    .bind(title)
    .bind(description)
    .bind(seed_value)
    .execute(pool)
    .await?;

    for (i, track) in tracks.iter().enumerate() {
        sqlx::query(
            "INSERT INTO daily_mix_tracks (mix_id, track_id, sort_order) VALUES (?, ?, ?)",
        )
        .bind(&mix_id)
        .bind(&track.id)
        .bind(i as i64)
        .execute(pool)
        .await?;
    }

    info!("created {mix_type} mix '{title}' with {} tracks for user {user_id}", tracks.len());

    // Generate mosaic cover art
    if let Err(e) = generate_mix_cover(pool, &mix_id).await {
        warn!("failed to generate cover for mix {mix_id}: {e}");
    }

    Ok(())
}
