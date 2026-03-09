use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::hash::{Hash, Hasher};
use tracing::info;

use crate::daily_mixes::{order_for_flow, related_genres};

// ─── Suggestion Generation (metadata-based, no AI) ──────────────────────────

pub struct Suggestion {
    pub label: String,
    pub prompt: String,
}

pub async fn generate_suggestions(pool: &SqlitePool, user_id: &str) -> Result<Vec<Suggestion>> {
    let date_str = chrono::Utc::now().format("%Y-%m-%d").to_string();

    // Top genres by play count
    let genre_rows = sqlx::query(
        "SELECT j.value as genre_name, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id,
         json_each(CASE WHEN a.genre = '[]' OR a.genre IS NULL THEN '[\"Unknown\"]' ELSE a.genre END) j
         WHERE ph.user_id = ? AND ph.completed = 1
         GROUP BY j.value
         HAVING j.value != 'Unknown'
         ORDER BY plays DESC
         LIMIT 5",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let genres: Vec<String> = genre_rows.iter().map(|r| r.get("genre_name")).collect();

    // Top moods
    let mood_rows = sqlx::query(
        "SELECT t.mood, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         WHERE ph.user_id = ? AND ph.completed = 1 AND t.mood IS NOT NULL
         GROUP BY t.mood
         ORDER BY plays DESC
         LIMIT 3",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let moods: Vec<String> = mood_rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("mood"))
        .collect();

    // Top decades
    let decade_rows = sqlx::query(
        "SELECT (a.year / 10 * 10) as decade, COUNT(*) as plays
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         WHERE ph.user_id = ? AND ph.completed = 1 AND a.year IS NOT NULL
         GROUP BY decade
         ORDER BY plays DESC
         LIMIT 3",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let decades: Vec<i32> = decade_rows.iter().map(|r| r.get("decade")).collect();

    // Build suggestion templates
    let mut templates: Vec<(String, String)> = Vec::new();

    for g in &genres {
        templates.push((
            format!("Late night {}", g.to_lowercase()),
            format!("Mellow, relaxing {} for winding down", g.to_lowercase()),
        ));
        templates.push((
            format!("Best of your {} collection", g),
            format!("Top-rated {} tracks from your library", g),
        ));
    }

    for m in &moods {
        if let Some(g) = genres.first() {
            templates.push((
                format!("{} {} session", m, g.to_lowercase()),
                format!("A {} {} listening session", m.to_lowercase(), g.to_lowercase()),
            ));
        }
    }

    for d in &decades {
        templates.push((
            format!("Deep cuts from the {}s", d),
            format!("Unplayed tracks from my {}s collection", d),
        ));
        if let Some(g) = genres.first() {
            templates.push((
                format!("{}s {} classics", d, g.to_lowercase()),
                format!("The best {} from the {}s in your library", g.to_lowercase(), d),
            ));
        }
    }

    // Activity-based (always available)
    templates.push(("Workout mix".to_string(), "High-energy tracks to keep you moving".to_string()));
    templates.push(("Sunday morning".to_string(), "Laid-back, warm tracks for a relaxed morning".to_string()));
    templates.push(("Focus mode".to_string(), "Instrumental and ambient tracks for concentration".to_string()));
    templates.push(("Dinner party".to_string(), "Sophisticated, upbeat tracks for entertaining".to_string()));

    if templates.is_empty() {
        return Ok(Vec::new());
    }

    // Use daily seed to pick 3-4 suggestions
    let seed = seed_index(user_id, &date_str, templates.len());
    let count = 4.min(templates.len());
    let mut suggestions = Vec::new();

    for i in 0..count {
        let idx = (seed + i) % templates.len();
        let (label, prompt) = &templates[idx];
        suggestions.push(Suggestion {
            label: label.clone(),
            prompt: prompt.clone(),
        });
    }

    Ok(suggestions)
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

// ─── Metadata-scored Playlist Generation ─────────────────────────────────────

pub struct PlaylistCriteria {
    pub prompt: String,
    pub genres: Vec<String>,
    pub energy: i32,
    pub era: Option<String>,
    pub moods: Vec<String>,
}

pub struct GeneratedPlaylist {
    pub name: String,
    pub description: String,
    pub track_ids: Vec<String>,
    pub scores: Vec<f32>,
}

/// Generate a playlist from structured metadata criteria.
///
/// Uses a Spotify-inspired two-phase approach:
/// Phase 1 (hard filter): Restrict candidate pool to tracks in the same genre family
/// Phase 2 (soft scoring): Rank filtered candidates by genre precision, energy, decade, mood
///
/// Falls back to all-library scoring (with minimum genre threshold) when the genre
/// family filter yields too few candidates.
pub async fn generate_playlist_from_prompt(
    pool: &SqlitePool,
    criteria: &PlaylistCriteria,
    track_count: usize,
    library_ids_json: &str,
    user_id: &str,
) -> Result<GeneratedPlaylist> {
    let track_count = track_count.clamp(5, 50);
    let min_candidates = 5;

    let query_genres: Vec<String> = criteria.genres.iter().map(|g| g.to_lowercase()).collect();
    let query_moods: Vec<String> = criteria.moods.iter().map(|m| m.to_lowercase()).collect();
    let era_filter = criteria.era.as_ref().and_then(|e| parse_era(e));

    let target_loudness: f64 = match criteria.energy {
        1 => -25.0,
        2 => -20.0,
        3 => -16.0,
        4 => -12.0,
        _ => -9.0,
    };

    // Phase 1: Build genre family filter for SQL-level candidate restriction
    let family_genres = expand_to_family(&query_genres);

    let mut in_fallback_mode = false;

    let rows = if !family_genres.is_empty() {
        // Try genre-family-filtered query first
        let filtered = fetch_tracks_in_genres(pool, library_ids_json, &family_genres, era_filter).await?;
        if filtered.len() >= min_candidates {
            info!(
                family_size = family_genres.len(),
                candidates = filtered.len(),
                "genre family filter applied"
            );
            filtered
        } else {
            // Fallback: fetch all tracks (will apply minimum genre threshold in scoring)
            info!(
                family_candidates = filtered.len(),
                "genre family too small, falling back to full library with genre threshold"
            );
            in_fallback_mode = true;
            fetch_all_tracks(pool, library_ids_json, era_filter).await?
        }
    } else {
        // No genres specified — fetch everything
        fetch_all_tracks(pool, library_ids_json, era_filter).await?
    };

    if rows.is_empty() {
        anyhow::bail!("no tracks available in library");
    }

    // Load LB similar artist scores for the user's top artists
    let lb_scores = load_lb_scores_for_user(pool, user_id).await.unwrap_or_default();

    // Phase 2: Score each track (simplified — genre + LB + energy + decade + mood)
    let family_lower: Vec<String> = family_genres.iter().map(|g| g.to_lowercase()).collect();

    let mut scored: Vec<(f32, f32, &sqlx::sqlite::SqliteRow)> = rows
        .iter()
        .map(|row| {
            let mut score: f32 = 0.0;
            let mut genre_score_final: f32 = 0.0;

            let genre_json: String = row.try_get("genre").unwrap_or_default();
            let style_json: String = row.try_get("style").unwrap_or_default();
            let track_genres: Vec<String> = crate::db::decode_json_array(&genre_json)
                .into_iter()
                .map(|g| g.to_lowercase())
                .collect();
            let track_styles: Vec<String> = crate::db::decode_json_array(&style_json)
                .into_iter()
                .map(|s| s.to_lowercase())
                .collect();

            // Genre precision scoring (0-30 points, reduced from 60)
            if !query_genres.is_empty() {
                let mut genre_score: f32 = 0.0;
                for qg in &query_genres {
                    if track_genres.iter().any(|tg| tg == qg) {
                        genre_score += 30.0;
                    } else if track_genres.iter().any(|tg| tg.contains(qg.as_str())) {
                        genre_score += 20.0;
                    } else if track_genres.iter().any(|tg| tg.contains(' ') && qg.contains(tg.as_str())) {
                        genre_score += 17.0;
                    } else if track_styles.iter().any(|ts| ts.contains(qg.as_str()) || (ts.contains(' ') && qg.contains(ts.as_str()))) {
                        genre_score += 12.0;
                    } else if !family_lower.is_empty() {
                        let in_family = track_genres.iter().any(|tg| family_lower.contains(tg))
                            || track_styles.iter().any(|ts| family_lower.contains(ts));
                        if in_family {
                            genre_score += 10.0;
                        }
                    }
                }
                genre_score_final = genre_score / query_genres.len() as f32;
                score += genre_score_final;
            }

            // LB artist similarity (0-25 points) — primary collaborative signal
            let artist_ext_id: Option<String> = row.try_get("artist_external_id").ok().flatten();
            if let Some(ref ext_id) = artist_ext_id {
                if let Some(&lb_score) = lb_scores.get(ext_id) {
                    score += (lb_score * 25.0) as f32;
                }
            }

            // Energy/loudness matching (0-10 points)
            if let Ok(Some(loudness)) = row.try_get::<Option<f64>, _>("loudness_lufs") {
                let diff = (loudness - target_loudness).abs();
                score += (10.0 - (diff as f32 * 1.0)).max(0.0);
            }

            // Mood matching (0-5 points)
            if !query_moods.is_empty() {
                let moods_json: String = row.try_get("moods").unwrap_or_default();
                let track_moods: Vec<String> = crate::db::decode_json_array(&moods_json)
                    .into_iter()
                    .map(|m| m.to_lowercase())
                    .collect();
                for qm in &query_moods {
                    if track_moods.iter().any(|tm| tm.contains(qm.as_str())) {
                        score += 5.0;
                    }
                }
            }

            (score, genre_score_final, row)
        })
        .collect();

    // Filter: in fallback mode require genre_score >= 8; otherwise filter out 0s
    if !query_genres.is_empty() {
        let min_genre_score = if in_fallback_mode { 8.0 } else { 0.0 };
        scored.retain(|&(_, gs, _)| gs > min_genre_score);
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Adaptive diversity caps based on candidate pool diversity
    let candidate_count = (track_count * 3).min(scored.len());
    let candidates = &scored[..candidate_count];

    let distinct_albums: usize = candidates
        .iter()
        .map(|(_, _, row)| row.get::<String, _>("album_id"))
        .collect::<std::collections::HashSet<_>>()
        .len();
    let distinct_artists: usize = candidates
        .iter()
        .map(|(_, _, row)| row.get::<String, _>("artist_id"))
        .collect::<std::collections::HashSet<_>>()
        .len();

    let max_per_album = if distinct_albums <= 3 {
        track_count // no cap
    } else if distinct_albums <= 8 {
        4
    } else {
        2
    };
    let max_per_artist = if distinct_artists <= 3 {
        track_count // no cap
    } else if distinct_artists <= 6 {
        5
    } else {
        3
    };

    info!(
        distinct_albums,
        distinct_artists,
        max_per_album,
        max_per_artist,
        "adaptive diversity caps"
    );

    let mut selected = Vec::new();
    let mut album_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut artist_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut score_map: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    for (total_score, _, row) in candidates {
        if selected.len() >= track_count {
            break;
        }

        let album_id: String = row.get("album_id");
        let artist_id: String = row.get("artist_id");

        if album_count.get(&album_id).copied().unwrap_or(0) >= max_per_album {
            continue;
        }
        if artist_count.get(&artist_id).copied().unwrap_or(0) >= max_per_artist {
            continue;
        }

        *album_count.entry(album_id.clone()).or_insert(0) += 1;
        *artist_count.entry(artist_id.clone()).or_insert(0) += 1;

        let track_id: String = row.get("id");
        score_map.insert(track_id.clone(), *total_score);

        let bpm_analyzed: Option<f64> = row.try_get("bpm_analyzed").ok().flatten();
        let bpm_tag: Option<f64> = row.try_get("bpm_tag").ok().flatten();

        selected.push(crate::daily_mixes::MixTrack {
            id: track_id,
            artist_id,
            album_id,
            bpm: bpm_analyzed.or(bpm_tag),
            key: row.try_get("key_analyzed").ok().flatten(),
            loudness: row.try_get("loudness_lufs").ok().flatten(),
            duration_seconds: row.try_get("duration_seconds").ok().flatten(),
            mood: row.try_get("mood").ok().flatten(),
            album_moods: {
                let moods_json: String = row.try_get("moods").unwrap_or_default();
                crate::db::decode_json_array(&moods_json)
            },
        });
    }

    if selected.is_empty() {
        anyhow::bail!("no matching tracks found for prompt");
    }

    order_for_flow(&mut selected);

    let track_ids: Vec<String> = selected.iter().map(|t| t.id.clone()).collect();
    let scores: Vec<f32> = track_ids
        .iter()
        .map(|id| score_map.get(id).copied().unwrap_or(0.0))
        .collect();

    let name = auto_name_from_prompt(&criteria.prompt);

    let (min_score, max_score) = scores.iter().copied().fold((f32::MAX, f32::MIN), |(mn, mx), s| (mn.min(s), mx.max(s)));
    info!(
        prompt = criteria.prompt,
        genres = ?criteria.genres,
        energy = criteria.energy,
        era = ?era_filter,
        tracks = track_ids.len(),
        total_candidates = rows.len(),
        scored_candidates = scored.len(),
        fallback = in_fallback_mode,
        min_score = format!("{:.1}", min_score),
        max_score = format!("{:.1}", max_score),
        "generated playlist from metadata"
    );

    Ok(GeneratedPlaylist {
        name,
        description: criteria.prompt.clone(),
        track_ids,
        scores,
    })
}

/// Expand query genres to their full genre families using GENRE_FAMILIES.
/// Returns all genres in the matching families (deduplicated).
fn expand_to_family(query_genres: &[String]) -> Vec<String> {
    let mut family_set = std::collections::HashSet::new();
    for qg in query_genres {
        let related = related_genres(qg);
        if related.is_empty() {
            // Genre not in any family — include it as-is so it can still match
            family_set.insert(qg.clone());
        } else {
            for g in related {
                family_set.insert(g.to_string());
            }
        }
    }
    family_set.into_iter().collect()
}

/// Build a SQL year/era clause and its bind value from an EraFilter.
fn era_clause(era: Option<EraFilter>, param_idx: i32) -> (String, Option<i32>, Option<i32>) {
    match era {
        Some(EraFilter::Year(y)) => (
            format!(" AND a.year = ?{}", param_idx),
            Some(y),
            None,
        ),
        Some(EraFilter::Decade(d)) => (
            format!(" AND a.year >= ?{} AND a.year < ?{}", param_idx, param_idx + 1),
            Some(d),
            Some(d + 10),
        ),
        None => (String::new(), None, None),
    }
}

/// Fetch only tracks whose album genre or style overlaps with the given genre set.
async fn fetch_tracks_in_genres(
    pool: &SqlitePool,
    library_ids_json: &str,
    genres: &[String],
    era: Option<EraFilter>,
) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
    if genres.is_empty() {
        return Ok(vec![]);
    }

    let genres_json = serde_json::to_string(genres)?;
    let (era_sql, era_bind1, era_bind2) = era_clause(era, 3);

    let sql = format!(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                t.mood,
                a.genre, a.style, a.year, a.moods,
                ar.external_id as artist_external_id
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.library_id IN (SELECT value FROM json_each(?1))
           AND (
               EXISTS (
                   SELECT 1 FROM json_each(a.genre) ag, json_each(?2) fg
                   WHERE LOWER(ag.value) = LOWER(fg.value)
               )
               OR EXISTS (
                   SELECT 1 FROM json_each(a.style) as2, json_each(?2) fg2
                   WHERE LOWER(as2.value) = LOWER(fg2.value)
               )
           ){}",
        era_sql
    );

    let mut query = sqlx::query(&sql)
        .bind(library_ids_json)
        .bind(&genres_json);
    if let Some(v) = era_bind1 { query = query.bind(v); }
    if let Some(v) = era_bind2 { query = query.bind(v); }

    Ok(query.fetch_all(pool).await?)
}

/// Fetch all tracks in the library (fallback path).
async fn fetch_all_tracks(
    pool: &SqlitePool,
    library_ids_json: &str,
    era: Option<EraFilter>,
) -> Result<Vec<sqlx::sqlite::SqliteRow>> {
    let (era_sql, era_bind1, era_bind2) = era_clause(era, 2);

    let sql = format!(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                t.mood,
                a.genre, a.style, a.year, a.moods,
                ar.external_id as artist_external_id
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.library_id IN (SELECT value FROM json_each(?1)){}",
        era_sql
    );

    let mut query = sqlx::query(&sql)
        .bind(library_ids_json);
    if let Some(v) = era_bind1 { query = query.bind(v); }
    if let Some(v) = era_bind2 { query = query.bind(v); }

    Ok(query.fetch_all(pool).await?)
}

/// Parsed era: either a specific year or a decade range.
#[derive(Debug, Clone, Copy)]
enum EraFilter {
    /// Exact year (e.g., "2010" → only albums from 2010)
    Year(i32),
    /// Decade (e.g., "2010s" → albums from 2010-2019)
    Decade(i32),
}

/// Parse an era string into either a specific year or a decade.
/// "2010" → Year(2010), "2010s" → Decade(2010), "90s" → Decade(1990)
fn parse_era(era: &str) -> Option<EraFilter> {
    let trimmed = era.trim();
    let has_s_suffix = trimmed.ends_with('s') || trimmed.ends_with('S');
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        4 => {
            let year: i32 = digits.parse().ok()?;
            if has_s_suffix {
                Some(EraFilter::Decade((year / 10) * 10))
            } else {
                Some(EraFilter::Year(year))
            }
        }
        2 => {
            let short: i32 = digits.parse().ok()?;
            let full = if short >= 50 { 1900 + short } else { 2000 + short };
            if has_s_suffix {
                Some(EraFilter::Decade(full))
            } else {
                Some(EraFilter::Year(full))
            }
        }
        _ => None,
    }
}

/// Generate a concise playlist name from a prompt.
fn auto_name_from_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.len() <= 40 {
        // Capitalize first letter
        let mut chars = trimmed.chars();
        match chars.next() {
            None => "Generated Playlist".to_string(),
            Some(c) => c.to_uppercase().to_string() + chars.as_str(),
        }
    } else {
        // Truncate and add ellipsis
        let truncated = &trimmed[..trimmed.char_indices().take(37).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(37)];
        format!("{}...", truncated)
    }
}

/// Load LB similar artist scores for a user's top-played artists.
/// Returns a map of artist MBID → normalized score (0.0-1.0).
async fn load_lb_scores_for_user(
    pool: &SqlitePool,
    user_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, f64>> {
    // Get the user's top 10 most-played artists' MBIDs
    let top_artist_mbids: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT ar.external_id
         FROM play_history ph
         JOIN tracks t ON ph.track_id = t.id
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE ph.user_id = ? AND ph.completed = 1
           AND ar.external_id IS NOT NULL AND ar.external_id != ''
         GROUP BY ar.id
         ORDER BY COUNT(*) DESC
         LIMIT 10",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    if top_artist_mbids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    for (mbid,) in &top_artist_mbids {
        let rows: Vec<(String, i32)> = sqlx::query_as(
            "SELECT similar_artist_mbid, score FROM lb_similar_artists
             WHERE artist_mbid = ?
             ORDER BY score DESC",
        )
        .bind(mbid)
        .fetch_all(pool)
        .await?;

        if rows.is_empty() {
            continue;
        }

        let max_score = rows.iter().map(|(_, s)| *s).max().unwrap_or(1) as f64;
        for (similar_mbid, score) in rows {
            let normalized = score as f64 / max_score.max(1.0);
            let entry = scores.entry(similar_mbid).or_insert(0.0);
            if normalized > *entry {
                *entry = normalized;
            }
        }
    }

    Ok(scores)
}
