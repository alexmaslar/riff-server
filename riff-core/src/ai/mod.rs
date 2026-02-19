pub mod prompt;
pub mod provider;

use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::AiConfig;
use prompt::{AlbumContext, AlbumSummaryCompact, ArtistSummaryCompact, CreditInfo, TrackInfo, SYSTEM_PROMPT, RATING_SYSTEM_PROMPT, RECOMMEND_SYSTEM_PROMPT, ARTIST_RECOMMEND_SYSTEM_PROMPT, ARTIST_BIO_SYSTEM_PROMPT, TAG_EXTRACTION_SYSTEM_PROMPT, build_album_prompt, build_rating_prompt, build_recommend_prompt, build_recommend_prompt_incremental, build_artist_recommend_prompt, build_artist_recommend_prompt_incremental, build_artist_bio_prompt, build_tag_extraction_prompt};
use provider::{GenerateOptions, create_provider, create_provider_with_model};

pub struct SummarizationResult {
    pub albums_summarized: u32,
    pub albums_skipped: u32,
    pub errors: Vec<String>,
}

pub async fn summarize_library(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<SummarizationResult> {
    let provider = create_provider(config)?;

    let mut result = SummarizationResult {
        albums_summarized: 0,
        albums_skipped: 0,
        errors: Vec::new(),
    };

    let rows: Vec<(String, String, String, Option<i32>, String, String, Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.country, a.release_notes \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             JOIN libraries l ON a.library_id = l.id \
             WHERE a.ai_summary IS NULL AND a.metadata_status = 'matched' \
             AND COALESCE(l.album_summaries, 1) = 1"
        )
        .fetch_all(pool)
        .await?;

    info!("summarizing {} albums with AI", rows.len());

    let opts = GenerateOptions {
        max_tokens: 2048,
        temperature: Some(0.7),
        web_search: true,
        json_output: false,
    };

    for (album_id, title, artist, year, genre_json, style_json, label, catno, country, notes) in &rows {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let style: Vec<String> = serde_json::from_str(style_json).unwrap_or_default();

        let tracks: Vec<(i32, i32, String, i32)> = match sqlx::query_as(
            "SELECT track_number, disc_number, title, duration_seconds \
             FROM tracks WHERE album_id = ? ORDER BY disc_number, track_number"
        )
        .bind(album_id)
        .fetch_all(pool)
        .await
        {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: failed to query tracks: {}", title, e));
                continue;
            }
        };

        let credits: Vec<(String, String)> = match sqlx::query_as(
            "SELECT artist_name, role FROM album_credits \
             WHERE album_id = ? ORDER BY sort_order"
        )
        .bind(album_id)
        .fetch_all(pool)
        .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to query credits for '{}': {}", title, e);
                Vec::new()
            }
        };

        let ctx = AlbumContext {
            title: title.clone(),
            artist: artist.clone(),
            year: *year,
            genre,
            style,
            label: label.clone(),
            catalog_number: catno.clone(),
            country: country.clone(),
            release_notes: notes.clone(),
            tracks: tracks.into_iter().map(|(num, disc, t, dur)| TrackInfo {
                number: num,
                disc,
                title: t,
                duration_seconds: dur,
            }).collect(),
            credits: credits.into_iter().map(|(name, role)| CreditInfo {
                name,
                role,
            }).collect(),
        };

        let user_prompt = build_album_prompt(&ctx);

        match provider.generate(SYSTEM_PROMPT, &user_prompt, &opts).await {
            Ok(summary) => {
                let summary = summary.trim().to_string();
                if let Err(e) = sqlx::query("UPDATE albums SET ai_summary = ? WHERE id = ?")
                    .bind(&summary)
                    .bind(album_id)
                    .execute(pool)
                    .await
                {
                    result.errors.push(format!("{}: db update failed: {}", title, e));
                    continue;
                }
                info!("summarized '{}'", title);
                result.albums_summarized += 1;
            }
            Err(e) => {
                warn!("AI summary failed for '{}': {}", title, e);
                result.errors.push(format!("{}: {}", title, e));
            }
        }
    }

    info!(
        "summarization complete: {} summarized, {} skipped, {} errors",
        result.albums_summarized, result.albums_skipped, result.errors.len()
    );

    Ok(result)
}

pub async fn summarize_album(
    pool: &SqlitePool,
    config: &AiConfig,
    album_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i32>, String, String, Option<String>, Option<String>, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT a.title, ar.name, a.year, a.genre, a.style, a.label, a.catalog_number, a.country, a.release_notes \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.id = ?"
        )
        .bind(album_id)
        .fetch_optional(pool)
        .await?;

    let (title, artist, year, genre_json, style_json, label, catno, country, notes) = match row {
        Some(r) => r,
        None => anyhow::bail!("album not found: {}", album_id),
    };

    let genre: Vec<String> = serde_json::from_str(&genre_json).unwrap_or_default();
    let style: Vec<String> = serde_json::from_str(&style_json).unwrap_or_default();

    let tracks: Vec<(i32, i32, String, i32)> = sqlx::query_as(
        "SELECT track_number, disc_number, title, duration_seconds \
         FROM tracks WHERE album_id = ? ORDER BY disc_number, track_number"
    )
    .bind(album_id)
    .fetch_all(pool)
    .await?;

    let credits: Vec<(String, String)> = sqlx::query_as(
        "SELECT artist_name, role FROM album_credits \
         WHERE album_id = ? ORDER BY sort_order"
    )
    .bind(album_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let ctx = AlbumContext {
        title: title.clone(),
        artist,
        year,
        genre,
        style,
        label,
        catalog_number: catno,
        country,
        release_notes: notes,
        tracks: tracks.into_iter().map(|(num, disc, t, dur)| TrackInfo {
            number: num,
            disc,
            title: t,
            duration_seconds: dur,
        }).collect(),
        credits: credits.into_iter().map(|(name, role)| CreditInfo {
            name,
            role,
        }).collect(),
    };

    let provider = create_provider(config)?;
    let user_prompt = build_album_prompt(&ctx);

    let opts = GenerateOptions {
        max_tokens: 2048,
        temperature: Some(0.7),
        web_search: true,
        json_output: false,
    };

    let summary = provider.generate(SYSTEM_PROMPT, &user_prompt, &opts).await?;
    let summary = summary.trim().to_string();

    sqlx::query("UPDATE albums SET ai_summary = ? WHERE id = ?")
        .bind(&summary)
        .bind(album_id)
        .execute(pool)
        .await?;

    info!("summarized '{}'", title);
    Ok(true)
}

fn parse_rating_only(response: &str) -> Option<f64> {
    for line in response.lines() {
        let trimmed = line.trim();
        if let Some(rating_str) = trimmed.strip_prefix("RATING:") {
            if let Ok(rating) = rating_str.trim().parse::<f64>() {
                if (0.0..=10.0).contains(&rating) {
                    return Some(rating);
                }
            }
        }
    }
    None
}

pub struct RatingResult {
    pub albums_rated: u32,
    pub albums_skipped: u32,
    pub errors: Vec<String>,
}

pub async fn rate_library(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RatingResult> {
    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;

    let mut result = RatingResult {
        albums_rated: 0,
        albums_skipped: 0,
        errors: Vec::new(),
    };

    let rows: Vec<(String, String, String, Option<i32>, String)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             JOIN libraries l ON a.library_id = l.id \
             WHERE a.ai_summary IS NOT NULL AND a.ai_rating IS NULL \
             AND COALESCE(l.album_ratings, 1) = 1"
        )
        .fetch_all(pool)
        .await?;

    info!("rating {} albums with AI", rows.len());

    let opts = GenerateOptions {
        max_tokens: 256,
        temperature: Some(0.2),
        web_search: true,
        json_output: false,
    };

    for (album_id, title, artist, year, genre_json) in &rows {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let user_prompt = build_rating_prompt(title, artist, *year, &genre);

        match provider.generate(RATING_SYSTEM_PROMPT, &user_prompt, &opts).await {
            Ok(response) => {
                match parse_rating_only(&response) {
                    Some(rating) => {
                        if let Err(e) = sqlx::query("UPDATE albums SET ai_rating = ? WHERE id = ?")
                            .bind(rating)
                            .bind(album_id)
                            .execute(pool)
                            .await
                        {
                            result.errors.push(format!("{}: db update failed: {}", title, e));
                            continue;
                        }
                        info!("rated '{}': {}", title, rating);
                        result.albums_rated += 1;
                    }
                    None => {
                        warn!("could not parse rating for '{}': {}", title, response);
                        result.errors.push(format!("{}: unparseable rating response", title));
                    }
                }
            }
            Err(e) => {
                warn!("AI rating failed for '{}': {}", title, e);
                result.errors.push(format!("{}: {}", title, e));
            }
        }
    }

    info!(
        "rating complete: {} rated, {} skipped, {} errors",
        result.albums_rated, result.albums_skipped, result.errors.len()
    );

    Ok(result)
}

pub async fn rate_album(
    pool: &SqlitePool,
    config: &AiConfig,
    album_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i32>, String)> =
        sqlx::query_as(
            "SELECT a.title, ar.name, a.year, a.genre \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.id = ?"
        )
        .bind(album_id)
        .fetch_optional(pool)
        .await?;

    let (title, artist, year, genre_json) = match row {
        Some(r) => r,
        None => anyhow::bail!("album not found: {}", album_id),
    };

    let genre: Vec<String> = serde_json::from_str(&genre_json).unwrap_or_default();
    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;
    let user_prompt = build_rating_prompt(&title, &artist, year, &genre);

    let opts = GenerateOptions {
        max_tokens: 256,
        temperature: Some(0.2),
        web_search: true,
        json_output: false,
    };

    let response = provider.generate(RATING_SYSTEM_PROMPT, &user_prompt, &opts).await?;

    match parse_rating_only(&response) {
        Some(rating) => {
            sqlx::query("UPDATE albums SET ai_rating = ? WHERE id = ?")
                .bind(rating)
                .bind(album_id)
                .execute(pool)
                .await?;
            info!("rated '{}': {}", title, rating);
            Ok(true)
        }
        None => {
            anyhow::bail!("could not parse rating from AI response for '{}'", title);
        }
    }
}

// --- Tag Extraction ---

/// Extract the first N sentences from a text.
/// Splits on sentence-ending punctuation followed by whitespace.
fn truncate_to_sentences(text: &str, max_sentences: usize) -> String {
    let mut count = 0;
    let mut end = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if (bytes[i] == b'.' || bytes[i] == b'!' || bytes[i] == b'?')
            && (i + 1 >= len || bytes[i + 1].is_ascii_whitespace())
        {
            // Skip trailing punctuation (e.g. "..." or "?!")
            while i + 1 < len && (bytes[i + 1] == b'.' || bytes[i + 1] == b'!' || bytes[i + 1] == b'?') {
                i += 1;
            }
            count += 1;
            end = i + 1;
            if count >= max_sentences {
                break;
            }
        }
        i += 1;
    }

    if count == 0 {
        // No sentence boundary found — return the whole thing
        text.to_string()
    } else {
        text[..end].trim().to_string()
    }
}

#[derive(Deserialize)]
struct TagExtractionResponse {
    #[serde(default)]
    moods: Vec<String>,
    #[serde(default)]
    descriptors: Vec<String>,
    #[serde(default)]
    keywords: Vec<String>,
}

pub struct TagExtractionResult {
    pub albums_tagged: u32,
    pub albums_skipped: u32,
    pub errors: Vec<String>,
}

pub async fn extract_tags_library(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<TagExtractionResult> {
    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;

    let mut result = TagExtractionResult {
        albums_tagged: 0,
        albums_skipped: 0,
        errors: Vec::new(),
    };

    let rows: Vec<(String, String, String, Option<i32>, String, String, String)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.ai_summary \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             JOIN libraries l ON a.library_id = l.id \
             WHERE a.ai_summary IS NOT NULL AND a.ai_moods = '[]' \
             AND COALESCE(l.album_tags, 1) = 1"
        )
        .fetch_all(pool)
        .await?;

    info!("extracting tags for {} albums", rows.len());

    let opts = GenerateOptions {
        max_tokens: 256,
        temperature: Some(0.0),
        web_search: false,
        json_output: true,
    };

    for (album_id, title, artist, year, genre_json, style_json, summary) in &rows {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let style: Vec<String> = serde_json::from_str(style_json).unwrap_or_default();
        let user_prompt = build_tag_extraction_prompt(title, artist, *year, &genre, &style, summary);

        match provider.generate(TAG_EXTRACTION_SYSTEM_PROMPT, &user_prompt, &opts).await {
            Ok(response) => {
                let json_str = strip_code_fences(&response);
                match serde_json::from_str::<TagExtractionResponse>(json_str) {
                    Ok(tags) => {
                        let moods = normalize_tags(tags.moods, 5);
                        let descriptors = normalize_tags(tags.descriptors, 5);
                        let keywords = normalize_tags(tags.keywords, 5);

                        let moods_json = serde_json::to_string(&moods).unwrap_or_else(|_| "[]".to_string());
                        let descriptors_json = serde_json::to_string(&descriptors).unwrap_or_else(|_| "[]".to_string());
                        let keywords_json = serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".to_string());

                        if let Err(e) = sqlx::query(
                            "UPDATE albums SET ai_moods = ?, ai_descriptors = ?, ai_keywords = ? WHERE id = ?"
                        )
                        .bind(&moods_json)
                        .bind(&descriptors_json)
                        .bind(&keywords_json)
                        .bind(album_id)
                        .execute(pool)
                        .await
                        {
                            result.errors.push(format!("{}: db update failed: {}", title, e));
                            continue;
                        }
                        info!("tagged '{}': moods={:?}, descriptors={:?}, keywords={:?}", title, moods, descriptors, keywords);
                        result.albums_tagged += 1;
                    }
                    Err(e) => {
                        warn!("could not parse tags for '{}': {} — raw: {}", title, e, &json_str[..json_str.len().min(300)]);
                        result.errors.push(format!("{}: parse error: {}", title, e));
                    }
                }
            }
            Err(e) => {
                warn!("AI tag extraction failed for '{}': {}", title, e);
                result.errors.push(format!("{}: {}", title, e));
            }
        }
    }

    info!(
        "tag extraction complete: {} tagged, {} skipped, {} errors",
        result.albums_tagged, result.albums_skipped, result.errors.len()
    );

    Ok(result)
}

pub async fn extract_tags_album(
    pool: &SqlitePool,
    config: &AiConfig,
    album_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i32>, String, String, Option<String>)> =
        sqlx::query_as(
            "SELECT a.title, ar.name, a.year, a.genre, a.style, a.ai_summary \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.id = ?"
        )
        .bind(album_id)
        .fetch_optional(pool)
        .await?;

    let (title, artist, year, genre_json, style_json, summary) = match row {
        Some(r) => r,
        None => anyhow::bail!("album not found: {}", album_id),
    };

    let summary = match summary {
        Some(s) => s,
        None => anyhow::bail!("album '{}' has no AI summary — summarize first", title),
    };

    let genre: Vec<String> = serde_json::from_str(&genre_json).unwrap_or_default();
    let style: Vec<String> = serde_json::from_str(&style_json).unwrap_or_default();

    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;
    let user_prompt = build_tag_extraction_prompt(&title, &artist, year, &genre, &style, &summary);

    let opts = GenerateOptions {
        max_tokens: 256,
        temperature: Some(0.0),
        web_search: false,
        json_output: true,
    };

    let response = provider.generate(TAG_EXTRACTION_SYSTEM_PROMPT, &user_prompt, &opts).await?;
    let json_str = strip_code_fences(&response);
    let tags: TagExtractionResponse = serde_json::from_str(json_str)
        .map_err(|e| anyhow::anyhow!("failed to parse tag extraction response for '{}': {}", title, e))?;

    let moods = normalize_tags(tags.moods, 5);
    let descriptors = normalize_tags(tags.descriptors, 5);
    let keywords = normalize_tags(tags.keywords, 5);

    let moods_json = serde_json::to_string(&moods)?;
    let descriptors_json = serde_json::to_string(&descriptors)?;
    let keywords_json = serde_json::to_string(&keywords)?;

    sqlx::query("UPDATE albums SET ai_moods = ?, ai_descriptors = ?, ai_keywords = ? WHERE id = ?")
        .bind(&moods_json)
        .bind(&descriptors_json)
        .bind(&keywords_json)
        .bind(album_id)
        .execute(pool)
        .await?;

    info!("tagged '{}': moods={:?}, descriptors={:?}, keywords={:?}", title, moods, descriptors, keywords);
    Ok(true)
}

/// Normalize tags: lowercase, trim, deduplicate, cap at max_count.
fn normalize_tags(tags: Vec<String>, max_count: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    tags.into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .take(max_count)
        .collect()
}

// --- Artist Bios ---

pub struct ArtistBioResult {
    pub artists_processed: u32,
    pub artists_skipped: u32,
    pub errors: Vec<String>,
}

pub async fn bio_artists(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<ArtistBioResult> {
    let provider = create_provider(config)?;

    let mut result = ArtistBioResult {
        artists_processed: 0,
        artists_skipped: 0,
        errors: Vec::new(),
    };

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ar.id, ar.name FROM artists ar \
         JOIN albums a ON a.artist_id = ar.id \
         JOIN libraries l ON a.library_id = l.id \
         WHERE ar.ai_bio IS NULL \
         AND COALESCE(l.artist_bios, 1) = 1"
    )
    .fetch_all(pool)
    .await?;

    info!("generating bios for {} artists", rows.len());

    let opts = GenerateOptions {
        max_tokens: 2048,
        temperature: Some(0.7),
        web_search: true,
        json_output: false,
    };

    for (artist_id, name) in &rows {
        // Gather albums for this artist
        let albums: Vec<(String, Option<i32>)> = match sqlx::query_as(
            "SELECT title, year FROM albums WHERE artist_id = ? ORDER BY year, title"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        {
            Ok(a) => a,
            Err(e) => {
                result.errors.push(format!("{}: failed to query albums: {}", name, e));
                continue;
            }
        };

        // Gather genres from this artist's albums
        let genre_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT genre FROM albums WHERE artist_id = ? AND genre != '[]'"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let genres: Vec<String> = genre_rows
            .iter()
            .flat_map(|(g,)| serde_json::from_str::<Vec<String>>(g).unwrap_or_default())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let user_prompt = build_artist_bio_prompt(name, &albums, &genres);

        match provider.generate(ARTIST_BIO_SYSTEM_PROMPT, &user_prompt, &opts).await {
            Ok(bio) => {
                let bio = bio.trim().to_string();
                if let Err(e) = sqlx::query("UPDATE artists SET ai_bio = ? WHERE id = ?")
                    .bind(&bio)
                    .bind(artist_id)
                    .execute(pool)
                    .await
                {
                    result.errors.push(format!("{}: db update failed: {}", name, e));
                    continue;
                }
                info!("generated bio for '{}'", name);
                result.artists_processed += 1;
            }
            Err(e) => {
                warn!("AI bio failed for '{}': {}", name, e);
                result.errors.push(format!("{}: {}", name, e));
            }
        }
    }

    info!(
        "artist bios complete: {} processed, {} skipped, {} errors",
        result.artists_processed, result.artists_skipped, result.errors.len()
    );

    Ok(result)
}

pub async fn bio_artist(
    pool: &SqlitePool,
    config: &AiConfig,
    artist_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM artists WHERE id = ?"
    )
    .bind(artist_id)
    .fetch_optional(pool)
    .await?;

    let (name,) = match row {
        Some(r) => r,
        None => anyhow::bail!("artist not found: {}", artist_id),
    };

    let albums: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT title, year FROM albums WHERE artist_id = ? ORDER BY year, title"
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await?;

    let genre_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT genre FROM albums WHERE artist_id = ? AND genre != '[]'"
    )
    .bind(artist_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let genres: Vec<String> = genre_rows
        .iter()
        .flat_map(|(g,)| serde_json::from_str::<Vec<String>>(g).unwrap_or_default())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let provider = create_provider(config)?;
    let user_prompt = build_artist_bio_prompt(&name, &albums, &genres);

    let opts = GenerateOptions {
        max_tokens: 2048,
        temperature: Some(0.7),
        web_search: true,
        json_output: false,
    };

    let bio = provider.generate(ARTIST_BIO_SYSTEM_PROMPT, &user_prompt, &opts).await?;
    let bio = bio.trim().to_string();

    sqlx::query("UPDATE artists SET ai_bio = ? WHERE id = ?")
        .bind(&bio)
        .bind(artist_id)
        .execute(pool)
        .await?;

    info!("generated bio for '{}'", name);
    Ok(true)
}

// --- Recommendations ---

#[derive(Deserialize)]
struct RecommendResponse {
    recommendations: Vec<AlbumRecommendation>,
}

#[derive(Deserialize)]
struct AlbumRecommendation {
    album_id: String,
    similar: Vec<SimilarEntry>,
}

#[derive(Deserialize)]
struct SimilarEntry {
    album_id: String,
    reason: String,
    score: f64,
}

pub struct RecommendResult {
    pub albums_processed: u32,
    pub recommendations_generated: u32,
}

fn strip_code_fences(s: &str) -> &str {
    let trimmed = s.trim();
    let trimmed = trimmed.strip_prefix("```json").or_else(|| trimmed.strip_prefix("```")).unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix("```").unwrap_or(trimmed);
    trimmed.trim()
}

pub async fn recommend_library(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    recommend_library_partitioned(pool, config, false).await
}

pub async fn recommend_library_force(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    recommend_library_partitioned(pool, config, true).await
}

/// Run album recommendations partitioned by library isolation.
/// Isolated libraries only recommend within themselves; non-isolated libraries are grouped.
async fn recommend_library_partitioned(
    pool: &SqlitePool,
    config: &AiConfig,
    force_full: bool,
) -> anyhow::Result<RecommendResult> {
    let mut total = RecommendResult { albums_processed: 0, recommendations_generated: 0 };

    // Non-isolated libraries grouped together
    let non_isolated: Vec<(String, String, String, Option<i32>, String, String, Option<String>, String, String, String, Option<String>)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.label, \
                    a.ai_moods, a.ai_descriptors, a.ai_keywords, a.ai_summary \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             JOIN libraries l ON a.library_id = l.id \
             WHERE COALESCE(l.album_recommendations, 1) = 1 AND l.isolated = 0"
        )
        .fetch_all(pool)
        .await?;

    if !non_isolated.is_empty() {
        let r = recommend_library_inner(pool, config, &non_isolated, force_full).await?;
        total.albums_processed += r.albums_processed;
        total.recommendations_generated += r.recommendations_generated;
    }

    // Each isolated library gets its own pass
    let isolated_libs: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM libraries WHERE isolated = 1"
    )
    .fetch_all(pool)
    .await?;

    for (lib_id,) in &isolated_libs {
        let albums: Vec<(String, String, String, Option<i32>, String, String, Option<String>, String, String, String, Option<String>)> =
            sqlx::query_as(
                "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.label, \
                        a.ai_moods, a.ai_descriptors, a.ai_keywords, a.ai_summary \
                 FROM albums a JOIN artists ar ON a.artist_id = ar.id \
                 JOIN libraries l ON a.library_id = l.id \
                 WHERE COALESCE(l.album_recommendations, 1) = 1 AND a.library_id = ?"
            )
            .bind(lib_id)
            .fetch_all(pool)
            .await?;

        if !albums.is_empty() {
            let r = recommend_library_inner(pool, config, &albums, force_full).await?;
            total.albums_processed += r.albums_processed;
            total.recommendations_generated += r.recommendations_generated;
        }
    }

    Ok(total)
}

async fn recommend_library_inner(
    pool: &SqlitePool,
    config: &AiConfig,
    albums: &[(String, String, String, Option<i32>, String, String, Option<String>, String, String, String, Option<String>)],
    force_full: bool,
) -> anyhow::Result<RecommendResult> {
    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;

    let album_ids: std::collections::HashSet<&str> = albums.iter().map(|(id, ..)| id.as_str()).collect();

    let summaries: Vec<AlbumSummaryCompact> = albums.iter().map(|(id, title, artist, year, genre_json, style_json, label, moods_json, descriptors_json, keywords_json, summary)| {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let style: Vec<String> = serde_json::from_str(style_json).unwrap_or_default();
        let moods: Vec<String> = serde_json::from_str(moods_json).unwrap_or_default();
        let descriptors: Vec<String> = serde_json::from_str(descriptors_json).unwrap_or_default();
        let keywords: Vec<String> = serde_json::from_str(keywords_json).unwrap_or_default();
        let summary_excerpt = summary.as_deref().map(|s| truncate_to_sentences(s, 2)).unwrap_or_default();
        AlbumSummaryCompact {
            id: id.clone(),
            title: title.clone(),
            artist: artist.clone(),
            year: *year,
            genre: genre.join(", "),
            style: style.join(", "),
            label: label.clone().unwrap_or_default(),
            moods: moods.join(", "),
            descriptors: descriptors.join(", "),
            keywords: keywords.join(", "),
            summary_excerpt,
        }
    }).collect();

    // Determine which albums need recommendations
    let user_prompt = if force_full {
        info!("generating recommendations for all {} albums (force full)", summaries.len());
        build_recommend_prompt(&summaries)
    } else {
        let covered_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT album_id FROM album_recommendations"
        )
        .fetch_all(pool)
        .await?;

        let covered: std::collections::HashSet<&str> = covered_rows.iter().map(|(id,)| id.as_str()).collect();
        let target_ids: Vec<&str> = album_ids.iter().copied().filter(|id| !covered.contains(id)).collect();

        if target_ids.is_empty() {
            info!("album recommendations up to date ({} albums covered), skipping", covered.len());
            return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
        }

        info!("generating recommendations for {} of {} albums (incremental)", target_ids.len(), summaries.len());
        build_recommend_prompt_incremental(&summaries, &target_ids)
    };

    let opts = GenerateOptions {
        max_tokens: 8192,
        temperature: Some(0.0),
        web_search: false,
        json_output: true,
    };

    let response = provider.generate(RECOMMEND_SYSTEM_PROMPT, &user_prompt, &opts).await?;
    let json_str = strip_code_fences(&response);

    let parsed: RecommendResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(first_err) => {
            warn!(
                "failed to parse AI recommendation response (retrying): {} — raw: {}",
                first_err,
                &json_str[..json_str.len().min(500)]
            );

            let retry_prompt = "Your previous response was not valid JSON. Respond with ONLY the JSON object, no code fences or explanations.";
            let retry_response = provider.generate(RECOMMEND_SYSTEM_PROMPT, retry_prompt, &opts).await?;
            let retry_json = strip_code_fences(&retry_response);

            serde_json::from_str(retry_json)
                .map_err(|e| anyhow::anyhow!(
                    "failed to parse AI recommendation response after retry: {} — raw: {}",
                    e,
                    &retry_json[..retry_json.len().min(500)]
                ))?
        }
    };

    if force_full {
        sqlx::query("DELETE FROM album_recommendations")
            .execute(pool)
            .await?;
    }

    let mut total_recs: u32 = 0;
    let mut albums_with_recs: u32 = 0;

    for rec in &parsed.recommendations {
        if !album_ids.contains(rec.album_id.as_str()) {
            warn!("skipping recommendation for unknown album_id: {}", rec.album_id);
            continue;
        }

        let mut order = 0;
        for entry in &rec.similar {
            if !album_ids.contains(entry.album_id.as_str()) {
                warn!("skipping recommended album_id {} (not in library)", entry.album_id);
                continue;
            }
            if entry.album_id == rec.album_id {
                continue;
            }

            let id = Uuid::new_v4().to_string();
            let score = entry.score.clamp(0.0, 1.0);

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO album_recommendations (id, album_id, recommended_album_id, reason, score, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?)"
            )
            .bind(&id)
            .bind(&rec.album_id)
            .bind(&entry.album_id)
            .bind(&entry.reason)
            .bind(score)
            .bind(order)
            .execute(pool)
            .await
            {
                warn!("failed to insert recommendation: {}", e);
                continue;
            }

            order += 1;
            total_recs += 1;
        }

        if order > 0 {
            albums_with_recs += 1;
        }
    }

    info!(
        "recommendations complete: {} albums processed, {} recommendations generated",
        albums_with_recs, total_recs
    );

    Ok(RecommendResult {
        albums_processed: albums_with_recs,
        recommendations_generated: total_recs,
    })
}

// --- Artist Recommendations ---

#[derive(Deserialize)]
struct ArtistRecommendResponse {
    recommendations: Vec<ArtistRecommendation>,
}

#[derive(Deserialize)]
struct ArtistRecommendation {
    artist_id: String,
    similar: Vec<SimilarArtistEntry>,
}

#[derive(Deserialize)]
struct SimilarArtistEntry {
    artist_id: String,
    reason: String,
    score: f64,
}

pub async fn recommend_artists(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    recommend_artists_partitioned(pool, config, false).await
}

pub async fn recommend_artists_force(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    recommend_artists_partitioned(pool, config, true).await
}

/// Run artist recommendations partitioned by library isolation.
/// Isolated libraries only recommend within themselves; non-isolated libraries are grouped.
async fn recommend_artists_partitioned(
    pool: &SqlitePool,
    config: &AiConfig,
    force_full: bool,
) -> anyhow::Result<RecommendResult> {
    let mut total = RecommendResult { albums_processed: 0, recommendations_generated: 0 };

    // Non-isolated libraries grouped together
    let non_isolated: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ar.id, ar.name FROM artists ar \
         JOIN albums a ON a.artist_id = ar.id \
         JOIN libraries l ON a.library_id = l.id \
         WHERE COALESCE(l.artist_recommendations, 1) = 1 AND l.isolated = 0"
    )
    .fetch_all(pool)
    .await?;

    if !non_isolated.is_empty() {
        let r = recommend_artists_inner(pool, config, &non_isolated, force_full).await?;
        total.albums_processed += r.albums_processed;
        total.recommendations_generated += r.recommendations_generated;
    }

    // Each isolated library gets its own pass
    let isolated_libs: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM libraries WHERE isolated = 1"
    )
    .fetch_all(pool)
    .await?;

    for (lib_id,) in &isolated_libs {
        let artists: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT ar.id, ar.name FROM artists ar \
             JOIN albums a ON a.artist_id = ar.id \
             JOIN libraries l ON a.library_id = l.id \
             WHERE COALESCE(l.artist_recommendations, 1) = 1 AND a.library_id = ?"
        )
        .bind(lib_id)
        .fetch_all(pool)
        .await?;

        if !artists.is_empty() {
            let r = recommend_artists_inner(pool, config, &artists, force_full).await?;
            total.albums_processed += r.albums_processed;
            total.recommendations_generated += r.recommendations_generated;
        }
    }

    Ok(total)
}

async fn recommend_artists_inner(
    pool: &SqlitePool,
    config: &AiConfig,
    artists: &[(String, String)],
    force_full: bool,
) -> anyhow::Result<RecommendResult> {
    let provider = create_provider_with_model(config, config.fast_model.as_deref())?;

    let artist_ids: std::collections::HashSet<&str> = artists.iter().map(|(id, _)| id.as_str()).collect();

    // Build compact artist list with albums, genres, styles, aggregated tags, bio excerpt
    let mut summaries = Vec::new();
    for (artist_id, name) in artists {
        let albums: Vec<(String, Option<i32>)> = sqlx::query_as(
            "SELECT title, year FROM albums WHERE artist_id = ? ORDER BY year, title"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let genre_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT genre FROM albums WHERE artist_id = ? AND genre != '[]'"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let genres: Vec<String> = genre_rows
            .iter()
            .flat_map(|(g,)| serde_json::from_str::<Vec<String>>(g).unwrap_or_default())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let style_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT style FROM albums WHERE artist_id = ? AND style != '[]'"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let styles: Vec<String> = style_rows
            .iter()
            .flat_map(|(s,)| serde_json::from_str::<Vec<String>>(s).unwrap_or_default())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Aggregate AI tags from all albums by this artist
        let tag_rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT ai_moods, ai_descriptors, ai_keywords FROM albums \
             WHERE artist_id = ? AND ai_moods != '[]'"
        )
        .bind(artist_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        let mut all_moods = std::collections::HashSet::new();
        let mut all_descriptors = std::collections::HashSet::new();
        let mut all_keywords = std::collections::HashSet::new();
        for (m, d, k) in &tag_rows {
            for tag in serde_json::from_str::<Vec<String>>(m).unwrap_or_default() {
                all_moods.insert(tag);
            }
            for tag in serde_json::from_str::<Vec<String>>(d).unwrap_or_default() {
                all_descriptors.insert(tag);
            }
            for tag in serde_json::from_str::<Vec<String>>(k).unwrap_or_default() {
                all_keywords.insert(tag);
            }
        }

        // Fetch artist bio excerpt (first 2 sentences)
        let bio_row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT ai_bio FROM artists WHERE id = ?"
        )
        .bind(artist_id)
        .fetch_optional(pool)
        .await
        .unwrap_or(None);

        let bio_excerpt = bio_row
            .and_then(|(bio,)| bio)
            .map(|b| truncate_to_sentences(&b, 2))
            .unwrap_or_default();

        summaries.push(ArtistSummaryCompact {
            id: artist_id.clone(),
            name: name.clone(),
            genres: genres.join(", "),
            styles: styles.join(", "),
            albums,
            moods: all_moods.into_iter().collect::<Vec<_>>().join(", "),
            descriptors: all_descriptors.into_iter().collect::<Vec<_>>().join(", "),
            keywords: all_keywords.into_iter().collect::<Vec<_>>().join(", "),
            bio_excerpt,
        });
    }

    // Determine which artists need recommendations
    let user_prompt = if force_full {
        info!("generating recommendations for all {} artists (force full)", summaries.len());
        build_artist_recommend_prompt(&summaries)
    } else {
        let covered_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT artist_id FROM artist_recommendations"
        )
        .fetch_all(pool)
        .await?;

        let covered: std::collections::HashSet<&str> = covered_rows.iter().map(|(id,)| id.as_str()).collect();
        let target_ids: Vec<&str> = artist_ids.iter().copied().filter(|id| !covered.contains(id)).collect();

        if target_ids.is_empty() {
            info!("artist recommendations up to date ({} artists covered), skipping", covered.len());
            return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
        }

        info!("generating recommendations for {} of {} artists (incremental)", target_ids.len(), summaries.len());
        build_artist_recommend_prompt_incremental(&summaries, &target_ids)
    };

    let opts = GenerateOptions {
        max_tokens: 8192,
        temperature: Some(0.0),
        web_search: false,
        json_output: true,
    };

    let response = provider.generate(ARTIST_RECOMMEND_SYSTEM_PROMPT, &user_prompt, &opts).await?;
    let json_str = strip_code_fences(&response);

    let parsed: ArtistRecommendResponse = match serde_json::from_str(json_str) {
        Ok(p) => p,
        Err(first_err) => {
            warn!(
                "failed to parse AI artist recommendation response (retrying): {} — raw: {}",
                first_err,
                &json_str[..json_str.len().min(500)]
            );

            let retry_prompt = "Your previous response was not valid JSON. Respond with ONLY the JSON object, no code fences or explanations.";
            let retry_response = provider.generate(ARTIST_RECOMMEND_SYSTEM_PROMPT, retry_prompt, &opts).await?;
            let retry_json = strip_code_fences(&retry_response);

            serde_json::from_str(retry_json)
                .map_err(|e| anyhow::anyhow!(
                    "failed to parse AI artist recommendation response after retry: {} — raw: {}",
                    e,
                    &retry_json[..retry_json.len().min(500)]
                ))?
        }
    };

    if force_full {
        sqlx::query("DELETE FROM artist_recommendations")
            .execute(pool)
            .await?;
    }

    let mut total_recs: u32 = 0;
    let mut artists_with_recs: u32 = 0;

    for rec in &parsed.recommendations {
        if !artist_ids.contains(rec.artist_id.as_str()) {
            warn!("skipping recommendation for unknown artist_id: {}", rec.artist_id);
            continue;
        }

        let mut order = 0;
        for entry in &rec.similar {
            if !artist_ids.contains(entry.artist_id.as_str()) {
                warn!("skipping recommended artist_id {} (not in library)", entry.artist_id);
                continue;
            }
            if entry.artist_id == rec.artist_id {
                continue;
            }

            let id = Uuid::new_v4().to_string();
            let score = entry.score.clamp(0.0, 1.0);

            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO artist_recommendations (id, artist_id, recommended_artist_id, reason, score, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?)"
            )
            .bind(&id)
            .bind(&rec.artist_id)
            .bind(&entry.artist_id)
            .bind(&entry.reason)
            .bind(score)
            .bind(order)
            .execute(pool)
            .await
            {
                warn!("failed to insert artist recommendation: {}", e);
                continue;
            }

            order += 1;
            total_recs += 1;
        }

        if order > 0 {
            artists_with_recs += 1;
        }
    }

    info!(
        "artist recommendations complete: {} artists processed, {} recommendations generated",
        artists_with_recs, total_recs
    );

    Ok(RecommendResult {
        albums_processed: artists_with_recs,
        recommendations_generated: total_recs,
    })
}
