use anyhow::Result;
use chrono::{NaiveDate, NaiveDateTime, Utc};
use image::RgbaImage;
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::analysis::dclap;

const MAX_TRACKS_PER_MIX: usize = 25;
const MAX_TRACKS_PER_ALBUM: usize = 2;
const MAX_TRACKS_PER_ARTIST: usize = 3;
const MIN_DISTINCT_ARTISTS: usize = 3;
const MIN_TRACKS_PER_MIX: usize = 20;
const MAX_SEED_RETRIES: usize = 3;
const SEED_ARTIST_TRACK_CAP: usize = 10;

// Scoring weights
const SCORE_FAV_TRACK: f64 = 3.0;
const SCORE_PLAY_FREQ_CAP: f64 = 2.0;
const SCORE_SKIP_MAX: f64 = 3.0;
const SCORE_RECENCY_MAX: f64 = 2.0;
const SCORE_RECENCY_DAYS: f64 = 7.0;
const SCORE_COOLDOWN_PENALTY: f64 = 3.0;
const SCORE_COOLDOWN_DAYS: i64 = 3;
const SCORE_COMPILATION_PENALTY: f64 = 1.5;
const SCORE_BLISS_MAX: f64 = 3.0;
const SCORE_BLISS_SCALE: f64 = 0.6;

// Flow ordering weights
const FLOW_BLISS_WEIGHT: f64 = 0.8;
const FLOW_MOOD_PENALTY: f64 = 0.3;
const FLOW_LOUDNESS_ARC_WEIGHT: f64 = 0.15;

// Genre families — groups of related genres for expanding candidate pools
const GENRE_FAMILIES: &[&[&str]] = &[
    &[
        "Rock", "Alternative Rock", "Indie Rock", "Post-Punk", "Punk", "Punk Rock",
        "New Wave", "Shoegaze", "Grunge", "Garage Rock", "Noise Rock", "Post-Rock",
        "Art Rock", "Psychedelic Rock", "Stoner Rock", "Surf Rock", "Britpop",
        "Dream Pop", "Noise Pop", "Krautrock", "Math Rock", "Emo", "Hardcore Punk",
        "Post-Hardcore",
    ],
    &[
        "Metal", "Heavy Metal", "Thrash Metal", "Death Metal", "Black Metal",
        "Doom Metal", "Power Metal", "Progressive Metal", "Symphonic Metal",
        "Sludge Metal", "Stoner Metal", "Speed Metal", "Folk Metal", "Nu Metal",
        "Gothic Metal", "Metalcore", "Grindcore", "Industrial Metal",
    ],
    &[
        "Electronic", "Ambient", "Techno", "House", "Drum And Bass", "Dubstep",
        "IDM", "Synthwave", "Downtempo", "Trip Hop", "Trance", "Breakbeat",
        "Electro", "Minimal", "Glitch", "Vaporwave", "UK Garage", "Jungle",
        "Deep House", "Acid House", "Progressive House", "Dub Techno",
        "Electronica", "Chillwave",
    ],
    &[
        "Hip Hop", "Rap", "Trap", "Boom Bap", "Abstract Hip Hop",
        "Conscious Hip Hop", "Gangsta Rap", "Southern Hip Hop", "Lo-Fi Hip Hop",
        "East Coast Hip Hop", "West Coast Hip Hop", "Instrumental Hip Hop",
    ],
    &[
        "Jazz", "Bebop", "Cool Jazz", "Free Jazz", "Fusion", "Smooth Jazz",
        "Modal Jazz", "Hard Bop", "Avant-Garde Jazz", "Latin Jazz", "Nu Jazz",
        "Jazz Funk", "Big Band", "Swing", "Jazz Fusion",
    ],
    &[
        "Classical", "Baroque", "Romantic", "Contemporary Classical", "Minimalism",
        "Chamber Music", "Opera", "Choral", "Neo-Classical", "Modern Classical",
        "Orchestral",
    ],
    &[
        "Pop", "Synth-Pop", "Electropop", "Indie Pop", "Chamber Pop", "Art Pop",
        "Baroque Pop", "Power Pop", "Dance-Pop", "Hyperpop",
    ],
    &[
        "R&B", "Soul", "Funk", "Neo Soul", "Contemporary R&B", "Motown",
        "New Jack Swing", "Quiet Storm", "Disco",
    ],
    &[
        "Country", "Bluegrass", "Americana", "Outlaw Country", "Alt-Country",
        "Country Rock", "Honky Tonk", "Western Swing", "Country Pop",
    ],
    &[
        "Folk", "Indie Folk", "Freak Folk", "Folk Punk", "Contemporary Folk",
        "Singer-Songwriter", "Traditional Folk", "Folk Rock",
    ],
    &[
        "Blues", "Delta Blues", "Chicago Blues", "Electric Blues", "Blues Rock",
        "Country Blues", "Rhythm And Blues",
    ],
    &[
        "Reggae", "Dub", "Ska", "Dancehall", "Roots Reggae", "Lovers Rock",
        "Rocksteady",
    ],
    &[
        "Latin", "Salsa", "Bossa Nova", "Reggaeton", "Cumbia", "Bachata",
        "Merengue", "Tango", "MPB", "Latin Pop", "Latin Rock",
    ],
    &[
        "World", "Afrobeat", "Highlife", "Fado", "Flamenco", "Celtic",
        "Afro-Cuban", "Afropop",
    ],
    &[
        "Experimental", "Noise", "Industrial", "Avant-Garde",
        "Musique Concrète", "Sound Collage", "Drone",
    ],
    &["Soundtrack", "Film Score", "Video Game Music", "Musical"],
    &["Gospel", "Christian", "Worship", "Spiritual"],
    &["New Age", "Meditation", "Healing", "Space Music"],
];

/// Returns all genres in the same family as `genre`, or empty if not found.
fn related_genres(genre: &str) -> Vec<&'static str> {
    for family in GENRE_FAMILIES {
        if family.iter().any(|g| g.eq_ignore_ascii_case(genre)) {
            return family.to_vec();
        }
    }
    vec![]
}

pub struct MixTrack {
    pub id: String,
    pub artist_id: String,
    pub album_id: String,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub loudness: Option<f64>,
    pub bliss: Option<Vec<f64>>,
    pub dclap: Option<Vec<f32>>,
    pub duration_seconds: Option<i32>,
    pub mood: Option<String>,
    pub album_moods: Vec<String>,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

pub fn parse_bliss(row: &sqlx::sqlite::SqliteRow) -> Option<Vec<f64>> {
    let json_str: Option<String> = row.try_get("bliss_features").ok().flatten();
    json_str.and_then(|s| serde_json::from_str(&s).ok())
}

fn bliss_euclidean_distance(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() {
        return f64::MAX;
    }
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f64>()
        .sqrt()
}

/// Deterministic seed index from (user_id, date_str) — avoids annual cycle repeats
/// and gives different users different seeds on the same day.
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

fn compute_centroid(vectors: &[&[f64]]) -> Option<Vec<f64>> {
    if vectors.is_empty() {
        return None;
    }
    let dim = vectors[0].len();
    if dim == 0 {
        return None;
    }
    let mut centroid = vec![0.0; dim];
    let mut count = 0usize;
    for v in vectors {
        if v.len() == dim {
            for (i, val) in v.iter().enumerate() {
                centroid[i] += val;
            }
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    for val in &mut centroid {
        *val /= count as f64;
    }
    Some(centroid)
}

/// Generate all 4 daily mixes for a single user within a library context.
pub async fn generate_daily_mixes(
    pool: &SqlitePool,
    user_id: &str,
    date: NaiveDate,
    library_ids_json: &str,
    library_id: Option<&str>,
) -> Result<()> {
    let date_str = date.format("%Y-%m-%d").to_string();

    // Clean up old cover files before deleting mixes (scoped by library context)
    let old_covers = if let Some(lib_id) = library_id {
        sqlx::query_as::<_, (Option<String>,)>(
            "SELECT cover_path FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id = ?",
        )
        .bind(user_id)
        .bind(&date_str)
        .bind(lib_id)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, (Option<String>,)>(
            "SELECT cover_path FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id IS NULL",
        )
        .bind(user_id)
        .bind(&date_str)
        .fetch_all(pool)
        .await?
    };

    for (cover_path,) in old_covers {
        if let Some(path) = cover_path {
            let _ = std::fs::remove_file(&path);
        }
    }

    // Delete any existing mixes for this user+date+library context
    if let Some(lib_id) = library_id {
        sqlx::query("DELETE FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id = ?")
            .bind(user_id)
            .bind(&date_str)
            .bind(lib_id)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("DELETE FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id IS NULL")
            .bind(user_id)
            .bind(&date_str)
            .execute(pool)
            .await?;
    }

    // Collect IDs already used across mixes today (for cross-mix dedup)
    let mut used_track_ids: Vec<String> = Vec::new();

    // Mix 1: Artist Mix
    if let Err(e) = generate_artist_mix(pool, user_id, &date_str, &mut used_track_ids, library_ids_json, library_id).await {
        warn!("artist mix generation failed for {user_id}: {e}");
    }

    // Mix 2: Genre Mix
    if let Err(e) = generate_genre_mix(pool, user_id, &date_str, &mut used_track_ids, library_ids_json, library_id).await {
        warn!("genre mix generation failed for {user_id}: {e}");
    }

    // Mix 3: Deep Cuts
    if let Err(e) = generate_deep_cuts_mix(pool, user_id, &date_str, &mut used_track_ids, library_ids_json, library_id).await {
        warn!("deep cuts mix generation failed for {user_id}: {e}");
    }

    // Mix 4: Decade Mix
    if let Err(e) = generate_decade_mix(pool, user_id, &date_str, &mut used_track_ids, library_ids_json, library_id).await {
        warn!("decade mix generation failed for {user_id}: {e}");
    }

    Ok(())
}

/// Generate daily mixes for all users across all library contexts.
pub async fn generate_all_daily_mixes(pool: &SqlitePool) -> Result<()> {
    let date = Utc::now().date_naive();
    let date_str = date.format("%Y-%m-%d").to_string();

    let users = sqlx::query_as::<_, (String,)>("SELECT id FROM users")
        .fetch_all(pool)
        .await?;

    // Build library contexts: default (non-isolated) + each isolated library
    let default_ids: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM libraries WHERE isolated = 0",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let isolated_ids: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM libraries WHERE isolated = 1",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    // Build contexts: (library_ids_json, library_id_for_mix)
    // For default context: library_id_for_mix = None (null in DB)
    // For isolated: library_id_for_mix = Some(id)
    let mut contexts: Vec<(String, Option<String>)> = Vec::new();
    if !default_ids.is_empty() {
        let json = serde_json::to_string(&default_ids).unwrap_or_else(|_| "[]".to_string());
        contexts.push((json, None));
    }
    for id in &isolated_ids {
        let json = serde_json::to_string(&[id]).unwrap_or_else(|_| "[]".to_string());
        contexts.push((json, Some(id.clone())));
    }

    info!(
        "generating daily mixes for {} users across {} library contexts",
        users.len(),
        contexts.len()
    );

    for (user_id,) in &users {
        for (library_ids_json, library_id) in &contexts {
            // Skip if 4 mixes already exist for this user+date+library context
            let existing = if let Some(lib_id) = library_id {
                sqlx::query_as::<_, (i64,)>(
                    "SELECT COUNT(*) FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id = ?",
                )
                .bind(user_id)
                .bind(&date_str)
                .bind(lib_id)
                .fetch_one(pool)
                .await?
            } else {
                sqlx::query_as::<_, (i64,)>(
                    "SELECT COUNT(*) FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id IS NULL",
                )
                .bind(user_id)
                .bind(&date_str)
                .fetch_one(pool)
                .await?
            };

            if existing.0 >= 4 {
                debug!(
                    "daily mixes: all 4 mixes exist for {user_id} (library={:?}), skipping",
                    library_id
                );
                // Backfill covers for mixes missing artwork
                let uncovered = if let Some(lib_id) = library_id {
                    sqlx::query_as::<_, (String,)>(
                        "SELECT id FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id = ? AND cover_path IS NULL",
                    )
                    .bind(user_id)
                    .bind(&date_str)
                    .bind(lib_id)
                    .fetch_all(pool)
                    .await?
                } else {
                    sqlx::query_as::<_, (String,)>(
                        "SELECT id FROM daily_mixes WHERE user_id = ? AND mix_date = ? AND library_id IS NULL AND cover_path IS NULL",
                    )
                    .bind(user_id)
                    .bind(&date_str)
                    .fetch_all(pool)
                    .await?
                };

                for (mix_id,) in uncovered {
                    if let Err(e) = generate_mix_cover(pool, &mix_id).await {
                        warn!("backfill cover failed for mix {mix_id}: {e}");
                    }
                }
                continue;
            }

            if let Err(e) =
                generate_daily_mixes(pool, user_id, date, library_ids_json, library_id.as_deref())
                    .await
            {
                warn!(
                    "daily mix generation failed for user {user_id} (library={:?}): {e}",
                    library_id
                );
            }
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
    used_track_ids: &mut Vec<String>,
    library_ids_json: &str,
    library_id: Option<&str>,
) -> Result<()> {
    // Find top-played artists for this user (scoped by library)
    let top_artists = sqlx::query(
        "SELECT ar.id, ar.name, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ? AND ph.completed = 1
           AND ar.library_id IN (SELECT value FROM json_each(?))
         GROUP BY ar.id
         ORDER BY plays DESC
         LIMIT 20",
    )
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    let artist_list = if top_artists.is_empty() {
        // Fallback: pick from artists with highest-rated albums
        let fallback_artists = sqlx::query(
            "SELECT ar.id, ar.name
             FROM artists ar
             JOIN albums a ON a.artist_id = ar.id
             WHERE a.rating IS NOT NULL
               AND ar.library_id IN (SELECT value FROM json_each(?))
             ORDER BY a.rating DESC
             LIMIT 20",
        )
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;

        if fallback_artists.is_empty() {
            info!("artist mix: no top-played or rated artists found for {user_id}, skipping");
            return Ok(());
        }
        fallback_artists
    } else {
        top_artists
    };

    let base_idx = seed_index(user_id, date_str, artist_list.len());
    let retries = MAX_SEED_RETRIES.min(artist_list.len());
    let mut best_result: Option<(String, String, String, Vec<MixTrack>)> = None;

    for attempt in 0..retries {
        let idx = (base_idx + attempt) % artist_list.len();
        let artist_id: String = artist_list[idx].get("id");
        let artist_name: String = artist_list[idx].get("name");

        let result = build_artist_mix_tracks(
            pool, user_id, date_str, &artist_id, used_track_ids, library_ids_json,
        ).await?;

        let seed_tag = format!("artist:{artist_id}");
        if result.len() >= MIN_TRACKS_PER_MIX {
            best_result = Some((artist_name, seed_tag, artist_id, result));
            break;
        }

        match &best_result {
            Some((_, _, _, prev)) if prev.len() >= result.len() => {}
            _ => { best_result = Some((artist_name, seed_tag, artist_id, result)); }
        }
    }

    let Some((artist_name, seed_tag, seed_artist_id, selected)) = best_result else {
        info!("artist mix: all seed retries produced no tracks for {user_id}, skipping");
        return Ok(());
    };

    if selected.len() < MIN_TRACKS_PER_MIX {
        info!("artist mix: best attempt only produced {} tracks for {user_id}, skipping", selected.len());
        return Ok(());
    }

    let genres: Vec<String> = sqlx::query(
        "SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.genre) j
         WHERE a.artist_id = ? AND a.library_id IN (SELECT value FROM json_each(?))
         LIMIT 3",
    )
    .bind(&seed_artist_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?
    .iter()
    .map(|r| r.get("genre"))
    .collect();

    let title = format!("{} Mix", artist_name);
    let description = if genres.is_empty() {
        format!("Tracks inspired by {}", artist_name)
    } else {
        genres.join(", ")
    };

    insert_mix(
        pool, user_id, date_str, "artist", &title, &description,
        &seed_tag, &selected, library_id,
    )
    .await?;

    used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
    Ok(())
}

/// Build artist mix tracks without inserting — returns selected tracks for retry evaluation.
async fn build_artist_mix_tracks(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    seed_artist_id: &str,
    used_track_ids: &[String],
    library_ids_json: &str,
) -> Result<Vec<MixTrack>> {
    // Get the seed artist's genres/styles (scoped by library)
    let artist_genres = sqlx::query(
        "SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.genre) j
         WHERE a.artist_id = ? AND a.library_id IN (SELECT value FROM json_each(?))
         UNION
         SELECT DISTINCT j.value as genre
         FROM albums a, json_each(a.style) j
         WHERE a.artist_id = ? AND a.library_id IN (SELECT value FROM json_each(?))",
    )
    .bind(seed_artist_id)
    .bind(library_ids_json)
    .bind(seed_artist_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    let genres: Vec<String> = artist_genres.iter().map(|r| r.get("genre")).collect();

    // Find tracks from the same genre/style family (scoped by library)
    let candidate_tracks = if genres.is_empty() {
        // Fallback: all tracks from this artist + random others
        sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.library_id IN (SELECT value FROM json_each(?))
             ORDER BY CASE WHEN a.artist_id = ? THEN 0 ELSE 1 END, rating DESC
             LIMIT 100",
        )
        .bind(library_ids_json)
        .bind(seed_artist_id)
        .fetch_all(pool)
        .await?
    } else {
        // Build genre match condition
        let placeholders: Vec<String> = genres.iter().enumerate().map(|(i, _)| format!("?{}", i + 3)).collect();
        let genre_list = placeholders.join(", ");

        let query = format!(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.library_id IN (SELECT value FROM json_each(?1))
               AND a.id IN (
                 SELECT DISTINCT a2.id FROM albums a2, json_each(a2.genre) jg
                 WHERE jg.value IN ({genre_list})
                 UNION
                 SELECT DISTINCT a3.id FROM albums a3, json_each(a3.style) js
                 WHERE js.value IN ({genre_list})
             )
             ORDER BY CASE WHEN a.artist_id = ? THEN 0 ELSE 1 END, rating DESC
             LIMIT 200"
        );

        let mut q = sqlx::query(&query)
            .bind(library_ids_json)
            .bind(seed_artist_id);
        for genre in &genres {
            q = q.bind(genre);
        }
        q = q.bind(seed_artist_id);
        q.fetch_all(pool).await?
    };

    let artist_centroid = compute_artist_bliss_centroid(pool, seed_artist_id).await?;
    let artist_dclap_centroid = compute_artist_dclap_centroid(pool, seed_artist_id).await?;
    let ctx = ScoringContext {
        pool,
        user_id,
        date_str,
        used_track_ids,
        compilation_penalty: SCORE_COMPILATION_PENALTY,
        bliss_centroid: artist_centroid.as_deref(),
        dclap_centroid: artist_dclap_centroid.as_deref(),
        max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
        seed_artist_id: Some(seed_artist_id),
    };
    score_and_select(&candidate_tracks, &ctx).await
}

// ─── Genre Mix ───────────────────────────────────────────────────────────────

async fn generate_genre_mix(
    pool: &SqlitePool,
    user_id: &str,
    date_str: &str,
    used_track_ids: &mut Vec<String>,
    library_ids_json: &str,
    library_id: Option<&str>,
) -> Result<()> {
    // Find most-played genres (scoped by library)
    let genre_rows = sqlx::query(
        "SELECT j.value as genre_name, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id,
         json_each(a.genre) j
         WHERE ph.user_id = ? AND ph.completed = 1
           AND t.library_id IN (SELECT value FROM json_each(?))
           AND a.genre IS NOT NULL AND a.genre != '[]'
           AND j.value != 'Unknown'
         GROUP BY j.value
         ORDER BY plays DESC
         LIMIT 20",
    )
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    let genres: Vec<String> = if genre_rows.is_empty() {
        // Fallback: most common genres in library (scoped)
        let lib_genres = sqlx::query(
            "SELECT j.value as genre_name, COUNT(*) as cnt
             FROM albums a, json_each(a.genre) j
             WHERE a.library_id IN (SELECT value FROM json_each(?))
               AND j.value != 'Unknown'
             GROUP BY j.value
             ORDER BY cnt DESC
             LIMIT 20",
        )
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;
        lib_genres.iter().map(|r| r.get("genre_name")).collect()
    } else {
        genre_rows.iter().map(|r| r.get("genre_name")).collect()
    };

    if genres.is_empty() {
        info!("genre mix: no genres found in play history or library for {user_id}, skipping");
        return Ok(());
    }

    let base_idx = seed_index(user_id, date_str, genres.len());
    let retries = MAX_SEED_RETRIES.min(genres.len());
    let mut best_result: Option<(String, Vec<MixTrack>)> = None;

    for attempt in 0..retries {
        let idx = (base_idx + attempt) % genres.len();
        let seed_genre = &genres[idx];

        // Expand seed genre to its family for a larger candidate pool
        let relatives = related_genres(seed_genre);
        let genre_list_json = if relatives.is_empty() {
            serde_json::to_string(&[seed_genre])?
        } else {
            serde_json::to_string(&relatives)?
        };

        // Get tracks from this genre family, diverse artists (scoped by library)
        let candidate_tracks = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE t.library_id IN (SELECT value FROM json_each(?))
               AND a.id IN (
                 SELECT DISTINCT a2.id FROM albums a2, json_each(a2.genre) jg
                 WHERE jg.value IN (SELECT value FROM json_each(?))
                 UNION
                 SELECT DISTINCT a3.id FROM albums a3, json_each(a3.style) js
                 WHERE js.value IN (SELECT value FROM json_each(?))
             )
             ORDER BY rating DESC
             LIMIT 200",
        )
        .bind(library_ids_json)
        .bind(&genre_list_json)
        .bind(&genre_list_json)
        .fetch_all(pool)
        .await?;

        let ctx = ScoringContext {
            pool,
            user_id,
            date_str,
            used_track_ids,
            compilation_penalty: 0.0,
            bliss_centroid: None,
            dclap_centroid: None,
            max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
            seed_artist_id: None,
        };
        let result = score_and_select(&candidate_tracks, &ctx).await?;

        if result.len() >= MIN_TRACKS_PER_MIX {
            best_result = Some((seed_genre.clone(), result));
            break;
        }

        match &best_result {
            Some((_, prev)) if prev.len() >= result.len() => {}
            _ => { best_result = Some((seed_genre.clone(), result)); }
        }
    }

    let Some((seed_genre, selected)) = best_result else {
        info!("genre mix: all seed retries produced no tracks for {user_id}, skipping");
        return Ok(());
    };

    if selected.len() < MIN_TRACKS_PER_MIX {
        info!("genre mix: best attempt only produced {} tracks for {user_id}, skipping", selected.len());
        return Ok(());
    }

    let title = format!("{} Mix", seed_genre);
    let description = format!("A mix of {} from your library", seed_genre);

    insert_mix(
        pool, user_id, date_str, "genre", &title, &description,
        &format!("genre:{seed_genre}"), &selected, library_id,
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
    used_track_ids: &mut Vec<String>,
    library_ids_json: &str,
    library_id: Option<&str>,
) -> Result<()> {
    // Tracks from well-rated albums that have never been played by this user (scoped by library)
    let candidate_tracks = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                t.bliss_features, t.dclap_embedding, t.mood,
                COALESCE(a.rating, 5.0) as rating,
                a.play_count, a.is_compilation, a.moods
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id NOT IN (
             SELECT track_id FROM play_history WHERE user_id = ?
         )
         AND a.rating IS NOT NULL
         AND a.rating >= 6.0
         AND t.library_id IN (SELECT value FROM json_each(?))
         ORDER BY a.rating DESC, RANDOM()
         LIMIT 200",
    )
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    let user_centroid = compute_user_bliss_centroid(pool, user_id).await?;
    let user_dclap_centroid = compute_user_dclap_centroid(pool, user_id).await?;

    if candidate_tracks.is_empty() {
        // Fallback: rarely played tracks (scoped by library)
        let fallback = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE a.play_count <= 1
               AND t.library_id IN (SELECT value FROM json_each(?))
             ORDER BY COALESCE(a.rating, 5.0) DESC, RANDOM()
             LIMIT 200",
        )
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;

        let ctx = ScoringContext {
            pool,
            user_id,
            date_str,
            used_track_ids,
            compilation_penalty: 0.0,
            bliss_centroid: user_centroid.as_deref(),
            dclap_centroid: user_dclap_centroid.as_deref(),
            max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
            seed_artist_id: None,
        };
        let selected = score_and_select(&fallback, &ctx).await?;
        if selected.is_empty() {
            info!("deep cuts: no candidates even from rarely-played fallback for {user_id}, skipping");
            return Ok(());
        }

        insert_mix(
            pool, user_id, date_str, "deep_cuts", "Deep Cuts",
            "Hidden gems from your library", "deep_cuts:", &selected, library_id,
        )
        .await?;

        used_track_ids.extend(selected.iter().map(|t| t.id.clone()));
        return Ok(());
    }

    let ctx = ScoringContext {
        pool,
        user_id,
        date_str,
        used_track_ids,
        compilation_penalty: 0.0,
        bliss_centroid: user_centroid.as_deref(),
        dclap_centroid: user_dclap_centroid.as_deref(),
        max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
        seed_artist_id: None,
    };
    let mut selected = score_and_select(&candidate_tracks, &ctx).await?;

    // If primary query yields too few tracks, merge with rarely-played fallback
    if selected.len() < MIN_TRACKS_PER_MIX {
        let fallback = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE a.play_count <= 1
               AND t.library_id IN (SELECT value FROM json_each(?))
             ORDER BY COALESCE(a.rating, 5.0) DESC, RANDOM()
             LIMIT 200",
        )
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;

        if !fallback.is_empty() {
            // Dedup: exclude tracks already selected
            let already: HashSet<String> = selected.iter().map(|t| t.id.clone()).collect();
            let mut extended_used: Vec<String> = used_track_ids.to_vec();
            extended_used.extend(already.iter().cloned());

            let ctx2 = ScoringContext {
                pool,
                user_id,
                date_str,
                used_track_ids: &extended_used,
                compilation_penalty: 0.0,
                bliss_centroid: user_centroid.as_deref(),
                dclap_centroid: user_dclap_centroid.as_deref(),
                max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
                seed_artist_id: None,
            };
            let mut extra = score_and_select(&fallback, &ctx2).await?;
            extra.truncate(MAX_TRACKS_PER_MIX.saturating_sub(selected.len()));
            selected.extend(extra);
        }
    }

    if selected.is_empty() {
        info!("deep cuts: scoring produced no tracks for {user_id}, skipping");
        return Ok(());
    }

    insert_mix(
        pool, user_id, date_str, "deep_cuts", "Deep Cuts",
        "Hidden gems from your library", "deep_cuts:", &selected, library_id,
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
    used_track_ids: &mut Vec<String>,
    library_ids_json: &str,
    library_id: Option<&str>,
) -> Result<()> {
    // Find most-played decades (scoped by library)
    let decade_rows = sqlx::query(
        "SELECT (a.year / 10 * 10) as decade, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE ph.user_id = ? AND ph.completed = 1 AND a.year IS NOT NULL
           AND t.library_id IN (SELECT value FROM json_each(?))
         GROUP BY decade
         ORDER BY plays DESC
         LIMIT 10",
    )
    .bind(user_id)
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    let decades: Vec<i32> = if decade_rows.is_empty() {
        // Fallback: most common decades in library (scoped)
        let lib_decades = sqlx::query(
            "SELECT (year / 10 * 10) as decade, COUNT(*) as cnt
             FROM albums
             WHERE year IS NOT NULL
               AND library_id IN (SELECT value FROM json_each(?))
             GROUP BY decade
             ORDER BY cnt DESC
             LIMIT 10",
        )
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;
        lib_decades.iter().map(|r| r.get("decade")).collect()
    } else {
        decade_rows.iter().map(|r| r.get("decade")).collect()
    };

    if decades.is_empty() {
        info!("decade mix: no decades found in play history or library for {user_id}, skipping");
        return Ok(());
    }

    let base_idx = seed_index(user_id, date_str, decades.len());
    let retries = MAX_SEED_RETRIES.min(decades.len());
    let mut best_result: Option<(i32, Vec<MixTrack>)> = None;

    for attempt in 0..retries {
        let idx = (base_idx + attempt) % decades.len();
        let seed_decade = decades[idx];
        let decade_end = seed_decade + 9;

        // Get tracks from this decade (scoped by library)
        let candidate_tracks = sqlx::query(
            "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                    t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                    t.key_analyzed, t.loudness_lufs,
                    t.bliss_features, t.dclap_embedding, t.mood,
                    COALESCE(a.rating, 5.0) as rating,
                    a.play_count, a.is_compilation, a.moods
             FROM tracks t
             JOIN albums a ON t.album_id = a.id
             JOIN artists ar ON a.artist_id = ar.id
             WHERE a.year >= ? AND a.year <= ?
               AND t.library_id IN (SELECT value FROM json_each(?))
             ORDER BY rating DESC
             LIMIT 200",
        )
        .bind(seed_decade)
        .bind(decade_end)
        .bind(library_ids_json)
        .fetch_all(pool)
        .await?;

        let ctx = ScoringContext {
            pool,
            user_id,
            date_str,
            used_track_ids,
            compilation_penalty: 0.0,
            bliss_centroid: None,
            dclap_centroid: None,
            max_tracks_per_artist: MAX_TRACKS_PER_ARTIST,
            seed_artist_id: None,
        };
        let result = score_and_select(&candidate_tracks, &ctx).await?;

        if result.len() >= MIN_TRACKS_PER_MIX {
            best_result = Some((seed_decade, result));
            break;
        }

        match &best_result {
            Some((_, prev)) if prev.len() >= result.len() => {}
            _ => { best_result = Some((seed_decade, result)); }
        }
    }

    let Some((seed_decade, selected)) = best_result else {
        info!("decade mix: all seed retries produced no tracks for {user_id}, skipping");
        return Ok(());
    };

    if selected.len() < MIN_TRACKS_PER_MIX {
        info!("decade mix: best attempt only produced {} tracks for {user_id}, skipping", selected.len());
        return Ok(());
    }

    let title = format!("{}s Mix", seed_decade);
    let description = format!("The best of the {}s from your library", seed_decade);

    insert_mix(
        pool, user_id, date_str, "decade", &title, &description,
        &format!("decade:{seed_decade}s"), &selected, library_id,
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
    bliss: Option<Vec<f64>>,
    dclap: Option<Vec<f32>>,
    duration_seconds: Option<i32>,
    mood: Option<String>,
    album_moods: Vec<String>,
    is_compilation: bool,
}

pub struct ScoringContext<'a> {
    pub pool: &'a SqlitePool,
    pub user_id: &'a str,
    pub date_str: &'a str,
    pub used_track_ids: &'a [String],
    pub compilation_penalty: f64,
    pub bliss_centroid: Option<&'a [f64]>,
    pub dclap_centroid: Option<&'a [f32]>,
    pub max_tracks_per_artist: usize,
    pub seed_artist_id: Option<&'a str>,
}

/// Per-track play statistics from play_history.
pub struct PlayStats {
    pub completed_plays: u32,
    pub total_plays: u32,
    pub last_played: Option<NaiveDateTime>,
}

pub async fn score_and_select(
    candidate_rows: &[sqlx::sqlite::SqliteRow],
    ctx: &ScoringContext<'_>,
) -> Result<Vec<MixTrack>> {
    if candidate_rows.is_empty() {
        return Ok(Vec::new());
    }

    // Pre-fetch user's favorited album, artist, and track IDs
    let fav_albums: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entity_id FROM favorites WHERE user_id = ? AND entity_type = 'album'",
    )
    .bind(ctx.user_id)
    .fetch_all(ctx.pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let fav_artists: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entity_id FROM favorites WHERE user_id = ? AND entity_type = 'artist'",
    )
    .bind(ctx.user_id)
    .fetch_all(ctx.pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let fav_tracks: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT entity_id FROM favorites WHERE user_id = ? AND entity_type = 'track'",
    )
    .bind(ctx.user_id)
    .fetch_all(ctx.pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    // Pre-fetch play stats per track for this user
    let play_stat_rows = sqlx::query(
        "SELECT track_id,
                SUM(completed) as completed_plays,
                COUNT(*) as total_plays,
                MAX(played_at) as last_played
         FROM play_history
         WHERE user_id = ?
         GROUP BY track_id",
    )
    .bind(ctx.user_id)
    .fetch_all(ctx.pool)
    .await?;

    let mut play_stats: HashMap<String, PlayStats> = HashMap::new();
    for row in &play_stat_rows {
        let track_id: String = row.get("track_id");
        let completed_plays: i64 = row.get("completed_plays");
        let total_plays: i64 = row.get("total_plays");
        let last_played_str: Option<String> = row.try_get("last_played").ok().flatten();
        let last_played = last_played_str.and_then(|s| {
            NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok()
        });
        play_stats.insert(track_id, PlayStats {
            completed_plays: completed_plays as u32,
            total_plays: total_plays as u32,
            last_played,
        });
    }

    // Pre-fetch tracks from recent mixes (cooldown)
    let cooldown_tracks: HashSet<String> = sqlx::query_as::<_, (String,)>(
        "SELECT dmt.track_id
         FROM daily_mix_tracks dmt
         JOIN daily_mixes dm ON dmt.mix_id = dm.id
         WHERE dm.user_id = ?
           AND dm.mix_date >= date(?, '-' || ? || ' days')
           AND dm.mix_date < ?",
    )
    .bind(ctx.user_id)
    .bind(ctx.date_str)
    .bind(SCORE_COOLDOWN_DAYS)
    .bind(ctx.date_str)
    .fetch_all(ctx.pool)
    .await?
    .into_iter()
    .map(|r| r.0)
    .collect();

    let fav_album_set: HashSet<&str> = fav_albums.iter().map(|s| s.as_str()).collect();
    let fav_artist_set: HashSet<&str> = fav_artists.iter().map(|s| s.as_str()).collect();
    let fav_track_set: HashSet<&str> = fav_tracks.iter().map(|s| s.as_str()).collect();
    let used_set: HashSet<&str> = ctx.used_track_ids.iter().map(|s| s.as_str()).collect();

    let now = ctx.date_str.parse::<NaiveDate>()
        .unwrap_or_else(|_| Utc::now().date_naive())
        .and_hms_opt(0, 0, 0)
        .unwrap_or_else(|| Utc::now().naive_utc());

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
            let bliss = parse_bliss(row);
            let dclap_emb = dclap::parse_dclap_embedding(row);
            let duration_seconds: Option<i32> = row.try_get("duration_seconds").ok().flatten();
            let mood: Option<String> = row.try_get("mood").ok().flatten();
            let album_moods_json: String = row.try_get("moods").unwrap_or_default();
            let album_moods: Vec<String> = crate::db::decode_json_array(&album_moods_json);
            let is_compilation: bool = row.try_get::<i32, _>("is_compilation")
                .ok()
                .map(|v| v != 0)
                .unwrap_or(false);

            // Base score from rating (0-10)
            let mut score = rating;

            // Favorites bonuses
            if fav_album_set.contains(album_id.as_str()) {
                score += 2.0;
            }
            if fav_artist_set.contains(artist_id.as_str()) {
                score += 2.0;
            }
            if fav_track_set.contains(id.as_str()) {
                score += SCORE_FAV_TRACK;
            }

            // Play frequency & skip penalty
            if let Some(stats) = play_stats.get(&id) {
                // Play frequency bonus: ln(1 + completed_plays), capped
                let freq_bonus = (1.0 + stats.completed_plays as f64).ln().min(SCORE_PLAY_FREQ_CAP);
                score += freq_bonus;

                // Skip penalty: only if enough plays for meaningful signal
                if stats.total_plays >= 3 {
                    let skip_rate = 1.0 - (stats.completed_plays as f64 / stats.total_plays as f64);
                    score -= skip_rate * SCORE_SKIP_MAX;
                }

                // Recency decay: linear penalty over SCORE_RECENCY_DAYS
                if let Some(last) = stats.last_played {
                    let days_ago = (now - last).num_days() as f64;
                    if days_ago < SCORE_RECENCY_DAYS {
                        let decay = (1.0 - days_ago / SCORE_RECENCY_DAYS) * SCORE_RECENCY_MAX;
                        score -= decay;
                    }
                }
            } else {
                // Unplayed track bonus (discovery)
                score += 1.0;
            }

            // Cooldown: was in a mix within the last N days
            if cooldown_tracks.contains(&id) {
                score -= SCORE_COOLDOWN_PENALTY;
            }

            // Already used in another mix today
            if used_set.contains(id.as_str()) {
                score -= 5.0;
            }

            // Compilation penalty (artist mix only)
            if is_compilation && ctx.compilation_penalty > 0.0 {
                score -= ctx.compilation_penalty;
            }

            // Similarity bonus: prefer DCLAP cosine, fall back to bliss euclidean
            if let (Some(centroid), Some(ref track_dclap)) = (ctx.dclap_centroid, &dclap_emb) {
                let sim = dclap::cosine_similarity(centroid, track_dclap);
                // sim is -1..1 (cosine), typically 0.3..0.95 for same-genre tracks
                score += (sim as f64 * SCORE_BLISS_MAX).clamp(0.0, SCORE_BLISS_MAX);
            } else if let (Some(centroid), Some(ref track_bliss)) = (ctx.bliss_centroid, &bliss) {
                let dist = bliss_euclidean_distance(centroid, track_bliss);
                let similarity = (SCORE_BLISS_MAX - dist / SCORE_BLISS_SCALE).clamp(0.0, SCORE_BLISS_MAX);
                score += similarity;
            }

            ScoredTrack {
                id, album_id, artist_id, score, bpm, key, loudness,
                bliss, dclap: dclap_emb, duration_seconds, mood, album_moods, is_compilation,
            }
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Log top few scores for debugging
    for track in scored.iter().take(3) {
        debug!(
            "scored track {} = {:.2} (compilation={}, bliss={})",
            track.id, track.score, track.is_compilation, track.bliss.is_some()
        );
    }

    // Apply diversity constraints
    let mut selected: Vec<MixTrack> = Vec::new();
    let mut album_count: HashMap<String, usize> = HashMap::new();
    let mut artist_count: HashMap<String, usize> = HashMap::new();
    let mut artist_set: HashSet<String> = HashSet::new();

    for track in &scored {
        if selected.len() >= MAX_TRACKS_PER_MIX {
            break;
        }

        let ac = album_count.get(&track.album_id).copied().unwrap_or(0);
        if ac >= MAX_TRACKS_PER_ALBUM {
            continue;
        }

        let arc = artist_count.get(&track.artist_id).copied().unwrap_or(0);
        let artist_limit = if ctx.seed_artist_id == Some(track.artist_id.as_str()) {
            SEED_ARTIST_TRACK_CAP
        } else {
            ctx.max_tracks_per_artist
        };
        if arc >= artist_limit {
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
            bliss: track.bliss.clone(),
            dclap: track.dclap.clone(),
            duration_seconds: track.duration_seconds,
            mood: track.mood.clone(),
            album_moods: track.album_moods.clone(),
        });
    }

    // Backfill pass: if we didn't reach MAX_TRACKS_PER_MIX, retry with doubled artist limits
    if selected.len() < MAX_TRACKS_PER_MIX {
        let selected_ids: HashSet<String> = selected.iter().map(|t| t.id.clone()).collect();
        for track in &scored {
            if selected.len() >= MAX_TRACKS_PER_MIX {
                break;
            }
            if selected_ids.contains(&track.id) {
                continue;
            }

            let ac = album_count.get(&track.album_id).copied().unwrap_or(0);
            if ac >= MAX_TRACKS_PER_ALBUM {
                continue;
            }

            let arc = artist_count.get(&track.artist_id).copied().unwrap_or(0);
            let base_limit = if ctx.seed_artist_id == Some(track.artist_id.as_str()) {
                SEED_ARTIST_TRACK_CAP
            } else {
                ctx.max_tracks_per_artist
            };
            if arc >= base_limit * 2 {
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
                bliss: track.bliss.clone(),
                dclap: track.dclap.clone(),
                duration_seconds: track.duration_seconds,
                mood: track.mood.clone(),
                album_moods: track.album_moods.clone(),
            });
        }
    }

    // Enforce minimum distinct artists
    if artist_set.len() < MIN_DISTINCT_ARTISTS && selected.len() < MIN_DISTINCT_ARTISTS {
        return Ok(selected);
    }

    order_for_flow(&mut selected);

    Ok(selected)
}

// ─── Bliss Centroid Functions ─────────────────────────────────────────────────

/// Mean bliss vector for all analyzed tracks by a given artist.
pub async fn compute_artist_bliss_centroid(
    pool: &SqlitePool,
    artist_id: &str,
) -> Result<Option<Vec<f64>>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT t.bliss_features
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         WHERE a.artist_id = ? AND t.bliss_features IS NOT NULL
         LIMIT 200",
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await?;

    let vectors: Vec<Vec<f64>> = rows
        .iter()
        .filter_map(|(json,)| serde_json::from_str(json).ok())
        .collect();

    let refs: Vec<&[f64]> = vectors.iter().map(|v| v.as_slice()).collect();
    Ok(compute_centroid(&refs))
}

/// Mean bliss vector of a user's top 50 most-completed tracks.
pub async fn compute_user_bliss_centroid(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Option<Vec<f64>>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT t.bliss_features
         FROM tracks t
         JOIN (
             SELECT track_id, SUM(completed) as plays
             FROM play_history
             WHERE user_id = ?
             GROUP BY track_id
             ORDER BY plays DESC
             LIMIT 50
         ) ph ON t.id = ph.track_id
         WHERE t.bliss_features IS NOT NULL",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let vectors: Vec<Vec<f64>> = rows
        .iter()
        .filter_map(|(json,)| serde_json::from_str(json).ok())
        .collect();

    let refs: Vec<&[f64]> = vectors.iter().map(|v| v.as_slice()).collect();
    Ok(compute_centroid(&refs))
}

// ─── DCLAP Centroid Functions ─────────────────────────────────────────────────

/// Mean DCLAP embedding for all analyzed tracks by a given artist.
pub async fn compute_artist_dclap_centroid(
    pool: &SqlitePool,
    artist_id: &str,
) -> Result<Option<Vec<f32>>> {
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT t.dclap_embedding
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         WHERE a.artist_id = ? AND t.dclap_embedding IS NOT NULL
         LIMIT 200",
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await?;

    let vectors: Vec<Vec<f32>> = rows
        .iter()
        .filter_map(|(blob,)| {
            if blob.len() != 512 * 4 {
                return None;
            }
            Some(
                blob.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        })
        .collect();

    let refs: Vec<&[f32]> = vectors.iter().map(|v| v.as_slice()).collect();
    Ok(dclap::compute_dclap_centroid(&refs))
}

/// Mean DCLAP embedding of a user's top 50 most-completed tracks.
pub async fn compute_user_dclap_centroid(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Option<Vec<f32>>> {
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT t.dclap_embedding
         FROM tracks t
         JOIN (
             SELECT track_id, SUM(completed) as plays
             FROM play_history
             WHERE user_id = ?
             GROUP BY track_id
             ORDER BY plays DESC
             LIMIT 50
         ) ph ON t.id = ph.track_id
         WHERE t.dclap_embedding IS NOT NULL",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let vectors: Vec<Vec<f32>> = rows
        .iter()
        .filter_map(|(blob,)| {
            if blob.len() != 512 * 4 {
                return None;
            }
            Some(
                blob.chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
            )
        })
        .collect();

    let refs: Vec<&[f32]> = vectors.iter().map(|v| v.as_slice()).collect();
    Ok(dclap::compute_dclap_centroid(&refs))
}

// ─── Flow Ordering (greedy nearest-neighbor) ─────────────────────────────────

pub fn order_for_flow(tracks: &mut Vec<MixTrack>) {
    if tracks.len() < 2 {
        return;
    }

    // Find median BPM for the starting track
    let mut bpms: Vec<f64> = tracks.iter().filter_map(|t| t.bpm).collect();
    let median_bpm = if bpms.is_empty() {
        120.0
    } else {
        bpms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        bpms[bpms.len() / 2]
    };

    // Compute min/max loudness for loudness arc
    let loudness_vals: Vec<f64> = tracks.iter().filter_map(|t| t.loudness).collect();
    let (min_lufs, max_lufs) = if loudness_vals.is_empty() {
        (-20.0, -10.0)
    } else {
        let min = loudness_vals.iter().copied().fold(f64::MAX, f64::min);
        let max = loudness_vals.iter().copied().fold(f64::MIN, f64::max);
        (min, max)
    };
    let median_lufs = if loudness_vals.is_empty() {
        -14.0
    } else {
        let mut sorted = loudness_vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        sorted[sorted.len() / 2]
    };

    // Compute max bliss distance for normalization
    let bliss_vecs: Vec<&Vec<f64>> = tracks.iter().filter_map(|t| t.bliss.as_ref()).collect();
    let max_bliss_dist = if bliss_vecs.len() >= 2 {
        let mut max_d = 0.0f64;
        for i in 0..bliss_vecs.len().min(20) {
            for j in (i + 1)..bliss_vecs.len().min(20) {
                let d = bliss_euclidean_distance(bliss_vecs[i], bliss_vecs[j]);
                if d > max_d && d < f64::MAX {
                    max_d = d;
                }
            }
        }
        if max_d < f64::EPSILON { 1.0 } else { max_d }
    } else {
        1.0
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

    ordered.push(remaining.swap_remove(start_idx));

    let min_bpm = bpms.first().copied().unwrap_or(100.0);
    let max_bpm = bpms.last().copied().unwrap_or(140.0);

    while !remaining.is_empty() {
        let pos_frac = ordered.len() as f64 / total as f64;
        let prev = ordered.last().unwrap();
        let second_prev = if ordered.len() >= 2 { Some(&ordered[ordered.len() - 2]) } else { None };

        // Target BPM from energy arc curve
        let target_bpm = if pos_frac < 0.4 {
            let t = pos_frac / 0.4;
            median_bpm + t * (max_bpm - median_bpm)
        } else if pos_frac < 0.7 {
            max_bpm
        } else {
            let t = (pos_frac - 0.7) / 0.3;
            max_bpm - t * (max_bpm - min_bpm)
        };

        // Target LUFS from loudness arc (same shape as BPM arc)
        let target_lufs = if pos_frac < 0.4 {
            let t = pos_frac / 0.4;
            median_lufs + t * (max_lufs - median_lufs)
        } else if pos_frac < 0.7 {
            max_lufs
        } else {
            let t = (pos_frac - 0.7) / 0.3;
            max_lufs - t * (max_lufs - min_lufs)
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

            // Timbral distance: prefer DCLAP cosine, fall back to bliss euclidean
            if let (Some(ref prev_dclap), Some(ref cand_dclap)) = (&prev.dclap, &candidate.dclap) {
                // Convert cosine similarity to distance: 0 = identical, 2 = opposite
                let dist = 1.0 - dclap::cosine_similarity(prev_dclap, cand_dclap) as f64;
                cost += FLOW_BLISS_WEIGHT * dist;
            } else if let (Some(ref prev_bliss), Some(ref cand_bliss)) = (&prev.bliss, &candidate.bliss) {
                let dist = bliss_euclidean_distance(prev_bliss, cand_bliss);
                if dist < f64::MAX {
                    cost += FLOW_BLISS_WEIGHT * (dist / max_bliss_dist);
                }
            }

            // Mood adjacency penalty (FLOW_MOOD_PENALTY)
            // Fall back to album-level AI moods when track mood is NULL
            let prev_effective_mood = prev.mood.as_deref()
                .or_else(|| prev.album_moods.first().map(|s| s.as_str()));
            let cand_effective_mood = candidate.mood.as_deref()
                .or_else(|| candidate.album_moods.first().map(|s| s.as_str()));
            if let (Some(prev_mood), Some(cand_mood)) = (prev_effective_mood, cand_effective_mood) {
                if prev_mood != cand_mood {
                    cost += FLOW_MOOD_PENALTY;
                }
            }

            // Duration adjacency: penalize two adjacent long or two adjacent short
            if let (Some(prev_dur), Some(cand_dur)) = (prev.duration_seconds, candidate.duration_seconds) {
                if prev_dur > 420 && cand_dur > 420 {
                    cost += 1.5; // two long tracks (>7min)
                } else if prev_dur < 120 && cand_dur < 120 {
                    cost += 1.0; // two short tracks (<2min)
                }
            }

            // Artist adjacency — HARD constraint: never consecutive
            if candidate.artist_id == prev.artist_id {
                cost = f64::MAX;
            } else if let Some(sp) = second_prev {
                if candidate.artist_id == sp.artist_id {
                    cost += 3.0;
                }
            }

            // Album adjacency — HARD constraint: never consecutive
            if cost < f64::MAX && candidate.album_id == prev.album_id {
                cost = f64::MAX;
            } else if cost < f64::MAX {
                if let Some(sp) = second_prev {
                    if candidate.album_id == sp.album_id {
                        cost += 2.0;
                    }
                }
            }

            if cost == f64::MAX {
                continue;
            }

            // Energy arc bias (weight 0.2) — BPM target curve
            if let Some(cand_bpm) = candidate.bpm {
                cost += 0.2 * (cand_bpm - target_bpm).abs() / 20.0;
            }

            // Loudness arc bias (weight FLOW_LOUDNESS_ARC_WEIGHT) — LUFS target curve
            if let Some(cand_lufs) = candidate.loudness {
                let lufs_range = (max_lufs - min_lufs).max(1.0);
                cost += FLOW_LOUDNESS_ARC_WEIGHT * (cand_lufs - target_lufs).abs() / lufs_range;
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

pub async fn generate_mix_cover(pool: &SqlitePool, mix_id: &str) -> Result<Option<PathBuf>> {
    // Query mix metadata
    let mix_row = sqlx::query_as::<_, (String, String)>(
        "SELECT mix_type, seed_value FROM daily_mixes WHERE id = ?",
    )
    .bind(mix_id)
    .fetch_optional(pool)
    .await?;

    let Some((mix_type, seed_value)) = mix_row else {
        return Ok(None);
    };

    let result = match mix_type.as_str() {
        "artist" => generate_artist_cover(pool, mix_id, &seed_value).await?,
        "genre" => generate_genre_cover(pool, mix_id, &seed_value).await?,
        "deep_cuts" => generate_deep_cuts_cover_dispatch(pool, mix_id).await?,
        "decade" => generate_decade_cover(pool, mix_id, &seed_value).await?,
        _ => generate_legacy_collage_cover(pool, mix_id).await?,
    };

    let Some(cover_image) = result else {
        return Ok(None);
    };

    // Save as JPEG to cache dir
    let mix_id_owned = mix_id.to_string();
    let cache_dir = crate::mix_collage::get_cache_dir()?;
    let filename = format!("mix_{}.jpg", mix_id_owned);
    let file_path = cache_dir.join(&filename);

    let file = std::fs::File::create(&file_path)?;
    let mut buf = std::io::BufWriter::new(file);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
    cover_image.write_with_encoder(encoder)?;

    sqlx::query("UPDATE daily_mixes SET cover_path = ? WHERE id = ?")
        .bind(file_path.to_str().unwrap())
        .bind(&mix_id_owned)
        .execute(pool)
        .await?;

    info!("generated {mix_type} cover for mix {mix_id_owned}");
    Ok(Some(file_path))
}

/// Fetch distinct album cover paths for a mix (up to `limit`).
async fn fetch_mix_cover_paths(pool: &SqlitePool, mix_id: &str, limit: u32) -> Result<Vec<PathBuf>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT DISTINCT a.cover_art_path
         FROM daily_mix_tracks dmt
         JOIN tracks t ON dmt.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE dmt.mix_id = ? AND a.cover_art_path IS NOT NULL
         ORDER BY dmt.sort_order
         LIMIT ?",
    )
    .bind(mix_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(p,)| PathBuf::from(p)).collect())
}

/// Load cover images from paths (skips files that fail to open).
fn load_cover_images(paths: &[PathBuf]) -> Vec<RgbaImage> {
    paths
        .iter()
        .filter_map(|p| image::open(p).ok().map(|img| img.to_rgba8()))
        .collect()
}

async fn generate_artist_cover(
    pool: &SqlitePool,
    mix_id: &str,
    seed_value: &str,
) -> Result<Option<image::DynamicImage>> {
    // Parse "artist:<uuid>"
    let artist_id = seed_value.strip_prefix("artist:").unwrap_or(seed_value);

    // Try to download the artist image
    let artist_image = fetch_artist_image(pool, artist_id).await;

    let cover_paths = fetch_mix_cover_paths(pool, mix_id, 4).await?;
    if cover_paths.is_empty() && artist_image.is_none() {
        return Ok(None);
    }

    let artist_rgba = artist_image.map(|img| img.to_rgba8());

    let result = tokio::task::spawn_blocking(move || {
        let path_refs: Vec<&Path> = cover_paths.iter().map(|p| p.as_path()).collect();
        crate::mix_collage::generate_artist_mix_cover(artist_rgba.as_ref(), &path_refs)
    })
    .await??;

    Ok(Some(result))
}

async fn generate_genre_cover(
    pool: &SqlitePool,
    mix_id: &str,
    seed_value: &str,
) -> Result<Option<image::DynamicImage>> {
    let genre = seed_value.strip_prefix("genre:").unwrap_or(seed_value);
    let cover_paths = fetch_mix_cover_paths(pool, mix_id, 5).await?;

    if cover_paths.is_empty() {
        return Ok(None);
    }

    let genre_owned = genre.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let images = load_cover_images(&cover_paths);
        crate::mix_collage::generate_genre_mix_cover(&genre_owned, &images)
    })
    .await??;

    Ok(Some(result))
}

async fn generate_deep_cuts_cover_dispatch(
    pool: &SqlitePool,
    mix_id: &str,
) -> Result<Option<image::DynamicImage>> {
    // Find the most-represented artist in this mix
    let top_artist = sqlx::query_as::<_, (String,)>(
        "SELECT a.artist_id
         FROM daily_mix_tracks dmt
         JOIN tracks t ON dmt.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE dmt.mix_id = ?
         GROUP BY a.artist_id
         ORDER BY COUNT(*) DESC
         LIMIT 1",
    )
    .bind(mix_id)
    .fetch_optional(pool)
    .await?;

    let artist_image = if let Some((artist_id,)) = &top_artist {
        fetch_artist_image(pool, artist_id).await
    } else {
        None
    };

    let cover_paths = fetch_mix_cover_paths(pool, mix_id, 4).await?;
    if cover_paths.is_empty() && artist_image.is_none() {
        return Ok(None);
    }

    let artist_rgba = artist_image.map(|img| img.to_rgba8());

    let result = tokio::task::spawn_blocking(move || {
        let path_refs: Vec<&Path> = cover_paths.iter().map(|p| p.as_path()).collect();
        crate::mix_collage::generate_deep_cuts_cover(artist_rgba.as_ref(), &path_refs)
    })
    .await??;

    Ok(Some(result))
}

async fn generate_decade_cover(
    pool: &SqlitePool,
    mix_id: &str,
    seed_value: &str,
) -> Result<Option<image::DynamicImage>> {
    // Parse "decade:1980s" → 1980
    let decade_str = seed_value.strip_prefix("decade:").unwrap_or(seed_value);
    let decade: i32 = decade_str
        .trim_end_matches('s')
        .parse()
        .unwrap_or(2000);

    let cover_paths = fetch_mix_cover_paths(pool, mix_id, 3).await?;

    if cover_paths.is_empty() {
        return Ok(None);
    }

    let result = tokio::task::spawn_blocking(move || {
        let images = load_cover_images(&cover_paths);
        crate::mix_collage::generate_decade_mix_cover(decade, &images)
    })
    .await??;

    Ok(Some(result))
}

/// Fetch artist image_url from DB and download it.
async fn fetch_artist_image(pool: &SqlitePool, artist_id: &str) -> Option<image::DynamicImage> {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT image_url FROM artists WHERE id = ?",
    )
    .bind(artist_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let image_url = row.and_then(|(url,)| url)?;
    if image_url.is_empty() {
        return None;
    }

    match crate::mix_collage::download_artist_image(artist_id, &image_url).await {
        Ok(img) => img,
        Err(e) => {
            warn!("failed to download artist image for {artist_id}: {e}");
            None
        }
    }
}

/// Legacy fallback: 2×2 collage (for unknown mix types).
async fn generate_legacy_collage_cover(
    pool: &SqlitePool,
    mix_id: &str,
) -> Result<Option<image::DynamicImage>> {
    let cover_paths = fetch_mix_cover_paths(pool, mix_id, 4).await?;
    if cover_paths.is_empty() {
        return Ok(None);
    }

    let result = tokio::task::spawn_blocking(move || {
        let path_refs: Vec<&Path> = cover_paths.iter().map(|p| p.as_path()).collect();
        crate::mix_collage::generate_mix_collage(&path_refs)
    })
    .await??;

    Ok(Some(result))
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
    library_id: Option<&str>,
) -> Result<()> {
    let mix_id = Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO daily_mixes (id, user_id, mix_date, mix_type, title, description, seed_value, library_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&mix_id)
    .bind(user_id)
    .bind(date_str)
    .bind(mix_type)
    .bind(title)
    .bind(description)
    .bind(seed_value)
    .bind(library_id)
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

    // Generate cover art in the background — don't block the response
    let pool = pool.clone();
    let mix_id_bg = mix_id.clone();
    tokio::spawn(async move {
        if let Err(e) = generate_mix_cover(&pool, &mix_id_bg).await {
            warn!("failed to generate cover for mix {mix_id_bg}: {e}");
        }
    });

    Ok(())
}
