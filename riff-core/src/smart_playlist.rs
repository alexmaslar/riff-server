use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::hash::{Hash, Hasher};

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
