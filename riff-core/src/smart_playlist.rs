use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::hash::{Hash, Hasher};
use tracing::info;

use crate::analysis::dclap;
use crate::daily_mixes::order_for_flow;

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
/// Scores tracks by genre match, energy/loudness, and decade.
/// Falls back to prompt keyword matching when no structured genres are provided.
pub async fn generate_playlist_from_prompt(
    pool: &SqlitePool,
    criteria: &PlaylistCriteria,
    track_count: usize,
    library_ids_json: &str,
) -> Result<GeneratedPlaylist> {
    let track_count = track_count.clamp(5, 50);

    // Fetch all tracks with album metadata
    let rows = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                t.bliss_features, t.dclap_embedding, t.mood,
                a.genre, a.style, a.year, a.moods
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.library_id IN (SELECT value FROM json_each(?))",
    )
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        anyhow::bail!("no tracks available in library");
    }

    // Lowercase genres for matching
    let query_genres: Vec<String> = criteria.genres.iter().map(|g| g.to_lowercase()).collect();
    let query_moods: Vec<String> = criteria.moods.iter().map(|m| m.to_lowercase()).collect();
    let query_decade = criteria.era.as_ref().and_then(|e| parse_decade(e));

    // Compute target loudness range from energy level
    // Energy 1-5 maps to loudness ranges (LUFS): quieter is more negative
    let target_loudness: f64 = match criteria.energy {
        1 => -25.0, // very calm
        2 => -20.0, // relaxed
        3 => -16.0, // moderate
        4 => -12.0, // energetic
        _ => -9.0,  // intense
    };

    // Score each track
    let mut scored: Vec<(f32, &sqlx::sqlite::SqliteRow)> = rows
        .iter()
        .map(|row| {
            let mut score: f32 = 0.0;

            // Genre matching (0-60 points) — heaviest weight
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

            if !query_genres.is_empty() {
                let mut genre_score: f32 = 0.0;
                for qg in &query_genres {
                    // Exact match
                    if track_genres.iter().any(|tg| tg == qg) {
                        genre_score += 60.0;
                    }
                    // Partial/substring match (e.g. "hip hop" matches "west coast hip hop")
                    else if track_genres.iter().any(|tg| tg.contains(qg.as_str()) || qg.contains(tg.as_str())) {
                        genre_score += 40.0;
                    }
                    // Style match
                    else if track_styles.iter().any(|ts| ts.contains(qg.as_str()) || qg.contains(ts.as_str())) {
                        genre_score += 25.0;
                    }
                    // Parent genre match (e.g. "rock" matches "indie rock")
                    else {
                        let qg_words: Vec<&str> = qg.split_whitespace().collect();
                        let has_word_match = qg_words.iter().any(|w|
                            track_genres.iter().any(|tg| tg.split_whitespace().any(|tw| tw == *w))
                        );
                        if has_word_match {
                            genre_score += 15.0;
                        }
                    }
                }
                score += genre_score / query_genres.len() as f32;
            }

            // Energy/loudness matching (0-20 points)
            if let Ok(Some(loudness)) = row.try_get::<Option<f64>, _>("loudness_lufs") {
                let diff = (loudness - target_loudness).abs();
                // 0 diff = 20 pts, 10+ diff = 0 pts
                score += (20.0 - (diff as f32 * 2.0)).max(0.0);
            }

            // Decade matching (0-15 points)
            if let Some(target_decade) = query_decade {
                if let Ok(Some(year)) = row.try_get::<Option<i32>, _>("year") {
                    let track_decade = (year / 10) * 10;
                    if track_decade == target_decade {
                        score += 15.0;
                    } else if (track_decade - target_decade).abs() <= 10 {
                        score += 7.0; // adjacent decade
                    }
                }
            }

            // Mood matching (0-5 points bonus)
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

            (score, row)
        })
        .collect();

    // Sort by score descending
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Take top candidates (3x requested count for diversity filtering)
    let candidate_count = (track_count * 3).min(scored.len());
    let candidates = &scored[..candidate_count];

    // Apply diversity: max 2 tracks per album, max 3 per artist
    let mut selected = Vec::new();
    let mut album_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut artist_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut score_map: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

    for (score, row) in candidates {
        if selected.len() >= track_count {
            break;
        }

        let album_id: String = row.get("album_id");
        let artist_id: String = row.get("artist_id");

        let ac = album_count.get(&album_id).copied().unwrap_or(0);
        if ac >= 2 {
            continue;
        }
        let arc = artist_count.get(&artist_id).copied().unwrap_or(0);
        if arc >= 3 {
            continue;
        }

        *album_count.entry(album_id.clone()).or_insert(0) += 1;
        *artist_count.entry(artist_id.clone()).or_insert(0) += 1;

        let track_id: String = row.get("id");
        score_map.insert(track_id.clone(), *score);

        let bpm_analyzed: Option<f64> = row.try_get("bpm_analyzed").ok().flatten();
        let bpm_tag: Option<f64> = row.try_get("bpm_tag").ok().flatten();

        selected.push(crate::daily_mixes::MixTrack {
            id: track_id,
            artist_id,
            album_id,
            bpm: bpm_analyzed.or(bpm_tag),
            key: row.try_get("key_analyzed").ok().flatten(),
            loudness: row.try_get("loudness_lufs").ok().flatten(),
            bliss: crate::daily_mixes::parse_bliss(row),
            dclap: dclap::parse_dclap_embedding(row),
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

    // Apply flow ordering for smooth BPM/key/loudness sequencing
    order_for_flow(&mut selected);

    let track_ids: Vec<String> = selected.iter().map(|t| t.id.clone()).collect();
    let scores: Vec<f32> = track_ids
        .iter()
        .map(|id| score_map.get(id).copied().unwrap_or(0.0))
        .collect();

    // Auto-generate a name from the prompt
    let name = auto_name_from_prompt(&criteria.prompt);

    let (min_score, max_score) = scores.iter().copied().fold((f32::MAX, f32::MIN), |(mn, mx), s| (mn.min(s), mx.max(s)));
    info!(
        prompt = criteria.prompt,
        genres = ?criteria.genres,
        energy = criteria.energy,
        tracks = track_ids.len(),
        candidates = scored.len(),
        min_score = format!("{:.4}", min_score),
        max_score = format!("{:.4}", max_score),
        "generated playlist from metadata"
    );

    Ok(GeneratedPlaylist {
        name,
        description: criteria.prompt.clone(),
        track_ids,
        scores,
    })
}

/// Parse a decade from era strings like "1990s", "90s", "2010s"
fn parse_decade(era: &str) -> Option<i32> {
    let digits: String = era.chars().filter(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        4 => digits.parse::<i32>().ok().map(|y| (y / 10) * 10),
        2 => {
            let short: i32 = digits.parse().ok()?;
            if short >= 50 { Some(1900 + short) } else { Some(2000 + short) }
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
