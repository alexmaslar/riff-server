mod prompt;
mod provider;

use serde::Deserialize;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::AiConfig;
use prompt::{AlbumContext, AlbumSummaryCompact, ArtistSummaryCompact, CreditInfo, TrackInfo, SYSTEM_PROMPT, RATING_SYSTEM_PROMPT, RECOMMEND_SYSTEM_PROMPT, ARTIST_RECOMMEND_SYSTEM_PROMPT, ARTIST_BIO_SYSTEM_PROMPT, build_album_prompt, build_rating_prompt, build_recommend_prompt, build_artist_recommend_prompt, build_artist_bio_prompt};
use provider::{GenerateOptions, create_provider};

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
             WHERE a.ai_summary IS NULL AND a.metadata_status = 'matched'"
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
    let provider = create_provider(config)?;

    let mut result = RatingResult {
        albums_rated: 0,
        albums_skipped: 0,
        errors: Vec::new(),
    };

    let rows: Vec<(String, String, String, Option<i32>, String)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.ai_summary IS NOT NULL AND a.ai_rating IS NULL"
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
    let provider = create_provider(config)?;
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
        "SELECT id, name FROM artists WHERE ai_bio IS NULL"
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
    // Query all albums
    let albums: Vec<(String, String, String, Option<i32>, String, String, Option<String>)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.label \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id"
        )
        .fetch_all(pool)
        .await?;

    if albums.is_empty() {
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    // Check staleness: if all albums already have recommendations, skip
    let (rec_album_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT album_id) FROM album_recommendations"
    )
    .fetch_one(pool)
    .await?;

    if rec_album_count as usize >= albums.len() {
        info!("recommendations up to date ({} albums covered), skipping", rec_album_count);
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    recommend_library_inner(pool, config, &albums).await
}

pub async fn recommend_library_force(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    let albums: Vec<(String, String, String, Option<i32>, String, String, Option<String>)> =
        sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.year, a.genre, a.style, a.label \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id"
        )
        .fetch_all(pool)
        .await?;

    if albums.is_empty() {
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    recommend_library_inner(pool, config, &albums).await
}

async fn recommend_library_inner(
    pool: &SqlitePool,
    config: &AiConfig,
    albums: &[(String, String, String, Option<i32>, String, String, Option<String>)],
) -> anyhow::Result<RecommendResult> {
    let provider = create_provider(config)?;

    // Build compact album list for the prompt
    let album_ids: std::collections::HashSet<&str> = albums.iter().map(|(id, ..)| id.as_str()).collect();

    let summaries: Vec<AlbumSummaryCompact> = albums.iter().map(|(id, title, artist, year, genre_json, style_json, label)| {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let style: Vec<String> = serde_json::from_str(style_json).unwrap_or_default();
        AlbumSummaryCompact {
            id: id.clone(),
            title: title.clone(),
            artist: artist.clone(),
            year: *year,
            genre: genre.join(", "),
            style: style.join(", "),
            label: label.clone().unwrap_or_default(),
        }
    }).collect();

    info!("generating recommendations for {} albums", summaries.len());
    let user_prompt = build_recommend_prompt(&summaries);

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

            // Retry once with an assertive follow-up
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

    // Clear existing recommendations
    sqlx::query("DELETE FROM album_recommendations")
        .execute(pool)
        .await?;

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
                continue; // skip self-references
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
    let artists: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, name FROM artists"
    )
    .fetch_all(pool)
    .await?;

    if artists.is_empty() {
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    // Check staleness
    let (rec_artist_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT artist_id) FROM artist_recommendations"
    )
    .fetch_one(pool)
    .await?;

    if rec_artist_count as usize >= artists.len() {
        info!("artist recommendations up to date ({} artists covered), skipping", rec_artist_count);
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    recommend_artists_inner(pool, config, &artists).await
}

pub async fn recommend_artists_force(
    pool: &SqlitePool,
    config: &AiConfig,
) -> anyhow::Result<RecommendResult> {
    let artists: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, name FROM artists"
    )
    .fetch_all(pool)
    .await?;

    if artists.is_empty() {
        return Ok(RecommendResult { albums_processed: 0, recommendations_generated: 0 });
    }

    recommend_artists_inner(pool, config, &artists).await
}

async fn recommend_artists_inner(
    pool: &SqlitePool,
    config: &AiConfig,
    artists: &[(String, String)],
) -> anyhow::Result<RecommendResult> {
    let provider = create_provider(config)?;

    let artist_ids: std::collections::HashSet<&str> = artists.iter().map(|(id, _)| id.as_str()).collect();

    // Build compact artist list with albums, genres, styles
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

        summaries.push(ArtistSummaryCompact {
            id: artist_id.clone(),
            name: name.clone(),
            genres: genres.join(", "),
            styles: styles.join(", "),
            albums,
        });
    }

    info!("generating recommendations for {} artists", summaries.len());
    let user_prompt = build_artist_recommend_prompt(&summaries);

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

    // Clear existing artist recommendations
    sqlx::query("DELETE FROM artist_recommendations")
        .execute(pool)
        .await?;

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
