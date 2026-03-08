use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::hash::{Hash, Hasher};
use tracing::info;

use crate::analysis::dclap::{self, DclapModel};
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

// ─── DCLAP-powered Playlist Generation ──────────────────────────────────────

pub struct GeneratedPlaylist {
    pub name: String,
    pub description: String,
    pub track_ids: Vec<String>,
    pub scores: Vec<f32>,
}

/// Generate a playlist from a natural language prompt using DCLAP text-to-audio similarity.
pub async fn generate_playlist_from_prompt(
    pool: &SqlitePool,
    prompt: &str,
    track_count: usize,
    library_ids_json: &str,
) -> Result<GeneratedPlaylist> {
    let track_count = track_count.clamp(5, 50);

    // Load DCLAP model and compute text embedding
    let model = DclapModel::load()?;
    let text_embedding = tokio::task::spawn_blocking({
        let prompt = prompt.to_string();
        move || model.embed_text(&prompt)
    })
    .await??;

    // Fetch all tracks with DCLAP embeddings
    let rows = sqlx::query(
        "SELECT t.id, t.title, t.album_id, a.artist_id, ar.name as artist_name,
                t.duration_seconds, t.bpm_analyzed, t.bpm_tag,
                t.key_analyzed, t.loudness_lufs,
                t.bliss_features, t.dclap_embedding, t.mood,
                a.moods
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.dclap_embedding IS NOT NULL
           AND t.library_id IN (SELECT value FROM json_each(?))",
    )
    .bind(library_ids_json)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        anyhow::bail!("no tracks with DCLAP embeddings available");
    }

    // Score each track by cosine similarity to the text embedding
    let mut scored: Vec<(f32, &sqlx::sqlite::SqliteRow)> = rows
        .iter()
        .filter_map(|row| {
            let emb = dclap::parse_dclap_embedding(row)?;
            let sim = dclap::cosine_similarity(&text_embedding, &emb);
            Some((sim, row))
        })
        .collect();

    // Sort by similarity descending
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Take top candidates (2x requested count for diversity filtering)
    let candidate_count = (track_count * 2).min(scored.len());
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
    let name = auto_name_from_prompt(prompt);

    let (min_score, max_score) = scores.iter().copied().fold((f32::MAX, f32::MIN), |(mn, mx), s| (mn.min(s), mx.max(s)));
    info!(
        prompt = prompt,
        tracks = track_ids.len(),
        candidates = scored.len(),
        min_score = format!("{:.4}", min_score),
        max_score = format!("{:.4}", max_score),
        "generated playlist from prompt"
    );

    Ok(GeneratedPlaylist {
        name,
        description: prompt.to_string(),
        track_ids,
        scores,
    })
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
