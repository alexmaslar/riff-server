use anyhow::Result;
use serde::Deserialize;
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::ai::prompt::{
    build_smart_playlist_prompt, build_smart_playlist_refine_prompt, TrackCandidate,
    INTENT_EXTRACTION_SYSTEM_PROMPT, SMART_PLAYLIST_REFINE_SYSTEM_PROMPT,
    SMART_PLAYLIST_SYSTEM_PROMPT,
};
use crate::ai::provider::{create_provider, GenerateOptions};
use crate::config::AiConfig;

const MAX_CANDIDATES: usize = 500;
const MIN_KEYWORD_CANDIDATES: usize = 20;
const DEFAULT_TRACK_COUNT: u32 = 20;
const MAX_TRACKS_PER_ALBUM: usize = 3;
const MAX_TRACKS_PER_ARTIST: usize = 5;
const RATE_LIMIT_SECONDS: u64 = 5;

// Per-user rate limiting
static RATE_LIMITS: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn check_rate_limit(user_id: &str) -> Result<(), String> {
    let mut limits = RATE_LIMITS.lock().await;
    if let Some(last) = limits.get(user_id) {
        if last.elapsed().as_secs() < RATE_LIMIT_SECONDS {
            return Err("Please wait before generating again".to_string());
        }
    }
    limits.insert(user_id.to_string(), Instant::now());
    Ok(())
}

// ─── Suggestion Generation (no AI) ──────────────────────────────────────────

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

// ─── Playlist Generation ────────────────────────────────────────────────────

pub struct GeneratedPlaylist {
    pub title: String,
    pub description: String,
    pub tracks: Vec<SelectedTrack>,
    pub candidate_count: usize,
}

pub struct SelectedTrack {
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
}

pub async fn generate_smart_playlist(
    pool: &SqlitePool,
    config: &AiConfig,
    user_prompt: &str,
    track_count: Option<u32>,
) -> Result<GeneratedPlaylist> {
    let track_count = track_count.unwrap_or(DEFAULT_TRACK_COUNT).min(50);

    // Step 1: Keyword-based candidate filtering
    let filters = extract_keyword_filters(pool, user_prompt).await;
    let mut candidates = query_candidates(pool, &filters).await?;
    let original_prompt = user_prompt;

    // If keyword matching yields < MIN_KEYWORD_CANDIDATES, try AI intent extraction
    if candidates.len() < MIN_KEYWORD_CANDIDATES {
        debug!(
            "keyword matching yielded {} candidates (< {}), trying AI intent extraction",
            candidates.len(),
            MIN_KEYWORD_CANDIDATES
        );
        match extract_intent_with_ai(config, user_prompt).await {
            Ok(ai_filters) => {
                let ai_candidates = query_candidates(pool, &ai_filters).await?;
                if ai_candidates.len() > candidates.len() {
                    candidates = ai_candidates;
                }
            }
            Err(e) => {
                warn!("AI intent extraction failed, using keyword candidates: {e}");
            }
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("No tracks match your description. Try broadening your request.");
    }

    let candidate_count = candidates.len();

    // Step 2: AI track selection
    let provider = create_provider(config)?;
    let track_candidates: Vec<TrackCandidate> = candidates
        .iter()
        .map(|c| TrackCandidate {
            id: c.id.clone(),
            title: c.title.clone(),
            artist: c.artist_name.clone(),
            album: c.album_title.clone(),
            genre: c.genre.clone(),
            year: c.year,
            bpm: c.bpm,
            key: c.musical_key.clone(),
            mood: c.mood.clone(),
            duration_seconds: c.duration_seconds,
        })
        .collect();

    let prompt_text = build_smart_playlist_prompt(original_prompt, &track_candidates, track_count);

    let opts = GenerateOptions {
        max_tokens: 4096,
        temperature: Some(0.3),
        web_search: false,
        json_output: true,
    };

    let response = provider
        .generate(SMART_PLAYLIST_SYSTEM_PROMPT, &prompt_text, &opts)
        .await?;
    let json_str = strip_code_fences(&response);

    let parsed: AiPlaylistResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(first_err) => {
            warn!("failed to parse AI playlist response (retrying): {first_err}");
            let retry_prompt = "Your previous response was not valid JSON. Respond with ONLY the JSON object: {\"title\": \"...\", \"description\": \"...\", \"trackIds\": [...]}";
            let retry_response = provider
                .generate(SMART_PLAYLIST_SYSTEM_PROMPT, retry_prompt, &opts)
                .await?;
            let retry_json = strip_code_fences(&retry_response);
            serde_json::from_str(retry_json).map_err(|e| {
                anyhow::anyhow!("failed to parse AI playlist response after retry: {e}")
            })?
        }
    };

    if parsed.track_ids.is_empty() {
        anyhow::bail!("AI selected no tracks. Try a different description.");
    }

    // Post-process: validate IDs, apply diversity constraints
    let candidate_map: HashMap<&str, &CandidateRow> =
        candidates.iter().map(|c| (c.id.as_str(), c)).collect();

    let valid_ids: Vec<&str> = parsed
        .track_ids
        .iter()
        .filter(|id| candidate_map.contains_key(id.as_str()))
        .map(|s| s.as_str())
        .collect();

    let selected_ids = apply_diversity_constraints(&valid_ids, &candidate_map);

    // Fetch full track details for selected IDs in order
    let tracks = fetch_track_details(pool, &selected_ids).await?;

    Ok(GeneratedPlaylist {
        title: parsed.title,
        description: parsed.description,
        tracks,
        candidate_count,
    })
}

pub async fn refine_smart_playlist(
    pool: &SqlitePool,
    config: &AiConfig,
    refinement_prompt: &str,
    current_track_ids: &[String],
    track_count: Option<u32>,
) -> Result<GeneratedPlaylist> {
    let track_count = track_count.unwrap_or(DEFAULT_TRACK_COUNT).min(50);

    // Get current track details for context
    let current_tracks = fetch_candidates_by_ids(pool, current_track_ids).await?;
    let current_candidates: Vec<TrackCandidate> = current_tracks
        .iter()
        .map(|c| TrackCandidate {
            id: c.id.clone(),
            title: c.title.clone(),
            artist: c.artist_name.clone(),
            album: c.album_title.clone(),
            genre: c.genre.clone(),
            year: c.year,
            bpm: c.bpm,
            key: c.musical_key.clone(),
            mood: c.mood.clone(),
            duration_seconds: c.duration_seconds,
        })
        .collect();

    // Get new candidates from a broader search
    let filters = extract_keyword_filters(pool, refinement_prompt).await;
    let mut candidates = query_candidates(pool, &filters).await?;

    // If keyword matching is sparse, also do a broader query
    if candidates.len() < MIN_KEYWORD_CANDIDATES {
        let broad_filters = CandidateFilters::default();
        candidates = query_candidates(pool, &broad_filters).await?;
    }

    // Remove tracks already in current playlist from candidates
    let current_set: HashSet<&str> = current_track_ids.iter().map(|s| s.as_str()).collect();
    candidates.retain(|c| !current_set.contains(c.id.as_str()));

    let candidate_count = candidates.len() + current_tracks.len();

    let new_candidates: Vec<TrackCandidate> = candidates
        .iter()
        .map(|c| TrackCandidate {
            id: c.id.clone(),
            title: c.title.clone(),
            artist: c.artist_name.clone(),
            album: c.album_title.clone(),
            genre: c.genre.clone(),
            year: c.year,
            bpm: c.bpm,
            key: c.musical_key.clone(),
            mood: c.mood.clone(),
            duration_seconds: c.duration_seconds,
        })
        .collect();

    let prompt_text = build_smart_playlist_refine_prompt(
        refinement_prompt,
        &current_candidates,
        &new_candidates,
        track_count,
    );

    let provider = create_provider(config)?;
    let opts = GenerateOptions {
        max_tokens: 4096,
        temperature: Some(0.3),
        web_search: false,
        json_output: true,
    };

    let response = provider
        .generate(SMART_PLAYLIST_REFINE_SYSTEM_PROMPT, &prompt_text, &opts)
        .await?;
    let json_str = strip_code_fences(&response);

    let parsed: AiPlaylistResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(first_err) => {
            warn!("failed to parse AI refine response (retrying): {first_err}");
            let retry_prompt = "Your previous response was not valid JSON. Respond with ONLY the JSON object: {\"title\": \"...\", \"description\": \"...\", \"trackIds\": [...]}";
            let retry_response = provider
                .generate(SMART_PLAYLIST_REFINE_SYSTEM_PROMPT, retry_prompt, &opts)
                .await?;
            let retry_json = strip_code_fences(&retry_response);
            serde_json::from_str(retry_json).map_err(|e| {
                anyhow::anyhow!("failed to parse AI refine response after retry: {e}")
            })?
        }
    };

    if parsed.track_ids.is_empty() {
        anyhow::bail!("AI selected no tracks. Try a different refinement.");
    }

    // Build combined candidate map (current + new)
    let all_candidates: Vec<CandidateRow> = current_tracks
        .into_iter()
        .chain(candidates)
        .collect();
    let candidate_map: HashMap<&str, &CandidateRow> =
        all_candidates.iter().map(|c| (c.id.as_str(), c)).collect();

    let valid_ids: Vec<&str> = parsed
        .track_ids
        .iter()
        .filter(|id| candidate_map.contains_key(id.as_str()))
        .map(|s| s.as_str())
        .collect();

    let selected_ids = apply_diversity_constraints(&valid_ids, &candidate_map);
    let tracks = fetch_track_details(pool, &selected_ids).await?;

    Ok(GeneratedPlaylist {
        title: parsed.title,
        description: parsed.description,
        tracks,
        candidate_count,
    })
}

// ─── Keyword Extraction ─────────────────────────────────────────────────────

#[derive(Default)]
struct CandidateFilters {
    genres: Vec<String>,
    moods: Vec<String>,
    decade_start: Option<i32>,
    decade_end: Option<i32>,
    bpm_min: Option<f64>,
    bpm_max: Option<f64>,
    artists: Vec<String>,
    exclude_genres: Vec<String>,
    exclude_artists: Vec<String>,
}

async fn extract_keyword_filters(pool: &SqlitePool, prompt: &str) -> CandidateFilters {
    let prompt_lower = prompt.to_lowercase();
    let mut filters = CandidateFilters::default();

    // Genre matching: check against known genres in the DB
    if let Ok(known_genres) = get_known_genres(pool).await {
        for genre in &known_genres {
            let genre_lower = genre.to_lowercase();
            if prompt_lower.contains(&genre_lower) {
                // Check for negation
                let negated = is_negated(&prompt_lower, &genre_lower);
                if negated {
                    filters.exclude_genres.push(genre.clone());
                } else {
                    filters.genres.push(genre.clone());
                }
            }
        }
    }

    // Artist matching: check against known artists
    if let Ok(known_artists) = get_known_artists(pool).await {
        for (name, _id) in &known_artists {
            let name_lower = name.to_lowercase();
            if name_lower.len() >= 3 && prompt_lower.contains(&name_lower) {
                let negated = is_negated(&prompt_lower, &name_lower);
                if negated {
                    filters.exclude_artists.push(name.clone());
                } else {
                    filters.artists.push(name.clone());
                }
            }
        }
    }

    // Decade detection: look for patterns like "1960s", "60s", "90s" in words
    for word in prompt_lower.split_whitespace() {
        let word = word.trim_matches(|c: char| !c.is_alphanumeric());
        if let Some(digits) = word.strip_suffix('s') {
            if let Ok(year) = digits.parse::<i32>() {
                if (1920..=2020).contains(&year) && year % 10 == 0 {
                    // "1960s" → 1960-1969
                    filters.decade_start = Some(year);
                    filters.decade_end = Some(year + 9);
                } else if (20..=99).contains(&year) {
                    // "60s" → 1960s
                    let full_year = 1900 + year;
                    filters.decade_start = Some(full_year);
                    filters.decade_end = Some(full_year + 9);
                } else if (0..20).contains(&year) {
                    // "00s", "10s" → 2000s, 2010s
                    let full_year = 2000 + year;
                    filters.decade_start = Some(full_year);
                    filters.decade_end = Some(full_year + 9);
                }
            }
        }
    }

    // Written decade words
    let decade_words = [
        ("twenties", 1920), ("thirties", 1930), ("forties", 1940),
        ("fifties", 1950), ("sixties", 1960), ("seventies", 1970),
        ("eighties", 1980), ("nineties", 1990),
    ];
    for (word, year) in &decade_words {
        if prompt_lower.contains(word) {
            filters.decade_start = Some(*year);
            filters.decade_end = Some(*year + 9);
        }
    }

    // BPM hints
    let bpm_keywords = [
        (&["upbeat", "fast", "energetic", "high-energy", "pump"] as &[&str], 120.0, 200.0),
        (&["chill", "slow", "relaxed", "mellow", "calm", "ambient"], 40.0, 100.0),
        (&["medium tempo", "mid-tempo", "moderate"], 90.0, 120.0),
    ];
    for (keywords, min, max) in &bpm_keywords {
        for kw in *keywords {
            if prompt_lower.contains(kw) {
                filters.bpm_min = Some(*min);
                filters.bpm_max = Some(*max);
                break;
            }
        }
    }

    // Mood keywords
    let mood_keywords = [
        "energetic", "melancholic", "aggressive", "calm", "happy",
        "sad", "dark", "bright", "dreamy", "intense",
    ];
    for mood in &mood_keywords {
        if prompt_lower.contains(mood) {
            filters.moods.push(mood.to_string());
        }
    }

    filters
}

fn is_negated(prompt: &str, term: &str) -> bool {
    if let Some(pos) = prompt.find(term) {
        let before = &prompt[..pos];
        let trimmed = before.trim_end();
        trimmed.ends_with("no") || trimmed.ends_with("without") || trimmed.ends_with("except")
    } else {
        false
    }
}

async fn get_known_genres(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT j.value
         FROM albums a, json_each(a.genre) j
         WHERE j.value IS NOT NULL AND j.value != ''
         UNION
         SELECT DISTINCT j.value
         FROM albums a, json_each(a.style) j
         WHERE j.value IS NOT NULL AND j.value != ''",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(g,)| g).collect())
}

async fn get_known_artists(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT name, id FROM artists").fetch_all(pool).await?;
    Ok(rows)
}

// ─── SQL Candidate Query ────────────────────────────────────────────────────

#[allow(dead_code)]
struct CandidateRow {
    id: String,
    title: String,
    album_title: String,
    album_id: String,
    artist_id: String,
    artist_name: String,
    genre: String,
    year: Option<i32>,
    duration_seconds: i32,
    bpm: Option<f64>,
    musical_key: Option<String>,
    mood: Option<String>,
    format: String,
    track_number: i32,
    disc_number: i32,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    composer: Option<String>,
    loudness_lufs: Option<f64>,
}

async fn query_candidates(pool: &SqlitePool, filters: &CandidateFilters) -> Result<Vec<CandidateRow>> {
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT t.id, t.title, a.title as album_title, a.id as album_id,
                ar.id as artist_id, ar.name as artist_name,
                a.genre, a.year, t.duration_seconds,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.mood, t.format, t.track_number, t.disc_number,
                t.sample_rate, t.bit_depth, t.composer, t.loudness_lufs,
                COALESCE(a.ai_rating, 5.0) as rating
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE 1=1",
    );

    let has_genre_filter = !filters.genres.is_empty();
    let has_artist_filter = !filters.artists.is_empty();

    // Genre/style filter
    if has_genre_filter {
        builder.push(" AND (a.id IN (SELECT a2.id FROM albums a2, json_each(a2.genre) jg WHERE jg.value IN (");
        let mut sep = builder.separated(", ");
        for g in &filters.genres {
            sep.push_bind(g.clone());
        }
        sep.push_unseparated("))");
        builder.push(" OR a.id IN (SELECT a3.id FROM albums a3, json_each(a3.style) js WHERE js.value IN (");
        let mut sep = builder.separated(", ");
        for g in &filters.genres {
            sep.push_bind(g.clone());
        }
        sep.push_unseparated(")))");
    }

    // Exclude genres
    if !filters.exclude_genres.is_empty() {
        builder.push(" AND a.id NOT IN (SELECT a4.id FROM albums a4, json_each(a4.genre) jg4 WHERE jg4.value IN (");
        let mut sep = builder.separated(", ");
        for g in &filters.exclude_genres {
            sep.push_bind(g.clone());
        }
        sep.push_unseparated("))");
    }

    // Artist filter
    if has_artist_filter {
        builder.push(" AND ar.name IN (");
        let mut sep = builder.separated(", ");
        for a in &filters.artists {
            sep.push_bind(a.clone());
        }
        sep.push_unseparated(")");
    }

    // Exclude artists
    if !filters.exclude_artists.is_empty() {
        builder.push(" AND ar.name NOT IN (");
        let mut sep = builder.separated(", ");
        for a in &filters.exclude_artists {
            sep.push_bind(a.clone());
        }
        sep.push_unseparated(")");
    }

    // Decade filter
    if let (Some(start), Some(end)) = (filters.decade_start, filters.decade_end) {
        builder.push(" AND a.year >= ");
        builder.push_bind(start);
        builder.push(" AND a.year <= ");
        builder.push_bind(end);
    }

    // BPM filter
    if let Some(min) = filters.bpm_min {
        builder.push(" AND COALESCE(t.bpm_analyzed, t.bpm_tag) >= ");
        builder.push_bind(min);
    }
    if let Some(max) = filters.bpm_max {
        builder.push(" AND COALESCE(t.bpm_analyzed, t.bpm_tag) <= ");
        builder.push_bind(max);
    }

    // Mood filter
    if !filters.moods.is_empty() {
        builder.push(" AND t.mood IN (");
        let mut sep = builder.separated(", ");
        for m in &filters.moods {
            sep.push_bind(m.clone());
        }
        sep.push_unseparated(")");
    }

    builder.push(" ORDER BY rating DESC, RANDOM()");

    builder.push(" LIMIT ");
    builder.push_bind(MAX_CANDIDATES as i64);

    let rows = builder.build().fetch_all(pool).await?;

    let candidates = rows
        .iter()
        .map(|row| {
            let genre_json: String = row.get("genre");
            let genres: Vec<String> = serde_json::from_str(&genre_json).unwrap_or_default();

            CandidateRow {
                id: row.get("id"),
                title: row.get("title"),
                album_title: row.get("album_title"),
                album_id: row.get("album_id"),
                artist_id: row.get("artist_id"),
                artist_name: row.get("artist_name"),
                genre: genres.join(", "),
                year: row.try_get("year").ok().flatten(),
                duration_seconds: row.get("duration_seconds"),
                bpm: row.try_get("bpm").ok().flatten(),
                musical_key: row.try_get("musical_key").ok().flatten(),
                mood: row.try_get("mood").ok().flatten(),
                format: row.get("format"),
                track_number: row.get("track_number"),
                disc_number: row.get("disc_number"),
                sample_rate: row.try_get("sample_rate").ok().flatten(),
                bit_depth: row.try_get("bit_depth").ok().flatten(),
                composer: row.try_get("composer").ok().flatten(),
                loudness_lufs: row.try_get("loudness_lufs").ok().flatten(),
            }
        })
        .collect();

    Ok(candidates)
}

async fn fetch_candidates_by_ids(pool: &SqlitePool, ids: &[String]) -> Result<Vec<CandidateRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT t.id, t.title, a.title as album_title, a.id as album_id,
                ar.id as artist_id, ar.name as artist_name,
                a.genre, a.year, t.duration_seconds,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.mood, t.format, t.track_number, t.disc_number,
                t.sample_rate, t.bit_depth, t.composer, t.loudness_lufs
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id IN (",
    );

    let mut sep = builder.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    sep.push_unseparated(")");

    let rows = builder.build().fetch_all(pool).await?;

    let candidates = rows
        .iter()
        .map(|row| {
            let genre_json: String = row.get("genre");
            let genres: Vec<String> = serde_json::from_str(&genre_json).unwrap_or_default();

            CandidateRow {
                id: row.get("id"),
                title: row.get("title"),
                album_title: row.get("album_title"),
                album_id: row.get("album_id"),
                artist_id: row.get("artist_id"),
                artist_name: row.get("artist_name"),
                genre: genres.join(", "),
                year: row.try_get("year").ok().flatten(),
                duration_seconds: row.get("duration_seconds"),
                bpm: row.try_get("bpm").ok().flatten(),
                musical_key: row.try_get("musical_key").ok().flatten(),
                mood: row.try_get("mood").ok().flatten(),
                format: row.get("format"),
                track_number: row.get("track_number"),
                disc_number: row.get("disc_number"),
                sample_rate: row.try_get("sample_rate").ok().flatten(),
                bit_depth: row.try_get("bit_depth").ok().flatten(),
                composer: row.try_get("composer").ok().flatten(),
                loudness_lufs: row.try_get("loudness_lufs").ok().flatten(),
            }
        })
        .collect();

    Ok(candidates)
}

// ─── AI Intent Extraction (fallback) ────────────────────────────────────────

#[derive(Deserialize)]
struct AiIntentResponse {
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    moods: Vec<String>,
    decade_start: Option<i32>,
    decade_end: Option<i32>,
    bpm_min: Option<i32>,
    bpm_max: Option<i32>,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    exclude_genres: Vec<String>,
    #[serde(default)]
    exclude_artists: Vec<String>,
}

async fn extract_intent_with_ai(config: &AiConfig, prompt: &str) -> Result<CandidateFilters> {
    let provider = create_provider(config)?;
    let opts = GenerateOptions {
        max_tokens: 512,
        temperature: Some(0.0),
        web_search: false,
        json_output: true,
    };

    let user_prompt = format!(
        "Parse this playlist description into search criteria:\n\"{}\"",
        prompt
    );

    let response = provider
        .generate(INTENT_EXTRACTION_SYSTEM_PROMPT, &user_prompt, &opts)
        .await?;
    let json_str = strip_code_fences(&response);
    let intent: AiIntentResponse = serde_json::from_str(json_str)?;

    Ok(CandidateFilters {
        genres: intent.genres,
        moods: intent.moods,
        decade_start: intent.decade_start,
        decade_end: intent.decade_end,
        bpm_min: intent.bpm_min.map(|v| v as f64),
        bpm_max: intent.bpm_max.map(|v| v as f64),
        artists: intent.artists,
        exclude_genres: intent.exclude_genres,
        exclude_artists: intent.exclude_artists,
    })
}

// ─── AI Response Parsing ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AiPlaylistResponse {
    title: String,
    description: String,
    #[serde(rename = "trackIds")]
    track_ids: Vec<String>,
}

fn strip_code_fences(s: &str) -> &str {
    let trimmed = s.trim();
    let trimmed = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed);
    trimmed.trim()
}

// ─── Post-Processing ────────────────────────────────────────────────────────

fn apply_diversity_constraints(
    track_ids: &[&str],
    candidates: &HashMap<&str, &CandidateRow>,
) -> Vec<String> {
    let mut selected: Vec<String> = Vec::new();
    let mut album_count: HashMap<&str, usize> = HashMap::new();
    let mut artist_count: HashMap<&str, usize> = HashMap::new();

    for &id in track_ids {
        if let Some(track) = candidates.get(id) {
            let ac = album_count.get(track.album_id.as_str()).copied().unwrap_or(0);
            if ac >= MAX_TRACKS_PER_ALBUM {
                continue;
            }
            let arc = artist_count.get(track.artist_id.as_str()).copied().unwrap_or(0);
            if arc >= MAX_TRACKS_PER_ARTIST {
                continue;
            }

            *album_count.entry(track.album_id.as_str()).or_insert(0) += 1;
            *artist_count.entry(track.artist_id.as_str()).or_insert(0) += 1;
            selected.push(id.to_string());
        }
    }

    selected
}

// ─── Track Detail Fetching ──────────────────────────────────────────────────

async fn fetch_track_details(pool: &SqlitePool, ids: &[String]) -> Result<Vec<SelectedTrack>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT t.id, t.title, t.track_number, t.disc_number, t.duration_seconds,
                t.format, t.album_id, ar.name as artist_name,
                t.sample_rate, t.bit_depth, t.composer,
                COALESCE(t.bpm_analyzed, t.bpm_tag) as bpm,
                COALESCE(t.key_analyzed, t.musical_key) as musical_key,
                t.loudness_lufs, t.mood
         FROM tracks t
         JOIN albums a ON t.album_id = a.id
         JOIN artists ar ON a.artist_id = ar.id
         WHERE t.id IN (",
    );

    let mut sep = builder.separated(", ");
    for id in ids {
        sep.push_bind(id.clone());
    }
    sep.push_unseparated(")");

    let rows = builder.build().fetch_all(pool).await?;

    // Build a map for ordering
    let mut track_map: HashMap<String, SelectedTrack> = HashMap::new();
    for row in &rows {
        let id: String = row.get("id");
        track_map.insert(
            id.clone(),
            SelectedTrack {
                id,
                title: row.get("title"),
                track_number: row.get("track_number"),
                disc_number: row.get("disc_number"),
                duration_seconds: row.get("duration_seconds"),
                format: row.get("format"),
                album_id: row.get("album_id"),
                artist_name: row.get("artist_name"),
                sample_rate: row.try_get("sample_rate").ok().flatten(),
                bit_depth: row.try_get("bit_depth").ok().flatten(),
                composer: row.try_get("composer").ok().flatten(),
                bpm: row.try_get("bpm").ok().flatten(),
                musical_key: row.try_get("musical_key").ok().flatten(),
                loudness_lufs: row.try_get("loudness_lufs").ok().flatten(),
                mood: row.try_get("mood").ok().flatten(),
            },
        );
    }

    // Preserve AI ordering
    let ordered: Vec<SelectedTrack> = ids
        .iter()
        .filter_map(|id| track_map.remove(id))
        .collect();

    Ok(ordered)
}
