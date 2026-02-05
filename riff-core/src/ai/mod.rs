mod prompt;
mod provider;

use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::config::AiConfig;
use prompt::{AlbumContext, CreditInfo, TrackInfo, SYSTEM_PROMPT, RATING_SYSTEM_PROMPT, build_album_prompt, build_rating_prompt};
use provider::create_provider;

fn parse_ai_response(response: &str) -> (Option<f64>, String) {
    if let Some(first_newline) = response.find('\n') {
        let first_line = response[..first_newline].trim();
        if let Some(rating_str) = first_line.strip_prefix("RATING:") {
            if let Ok(rating) = rating_str.trim().parse::<f64>() {
                if (0.0..=10.0).contains(&rating) {
                    let summary = response[first_newline..].trim().to_string();
                    return (Some(rating), summary);
                }
            }
        }
    }
    (None, response.to_string())
}

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

        match provider.generate(SYSTEM_PROMPT, &user_prompt).await {
            Ok(response) => {
                let (rating, summary) = parse_ai_response(&response);
                if let Err(e) = sqlx::query("UPDATE albums SET ai_summary = ?, ai_rating = ? WHERE id = ?")
                    .bind(&summary)
                    .bind(rating)
                    .bind(album_id)
                    .execute(pool)
                    .await
                {
                    result.errors.push(format!("{}: db update failed: {}", title, e));
                    continue;
                }
                info!("summarized '{}' (rating: {:?})", title, rating);
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
    let response = provider.generate(SYSTEM_PROMPT, &user_prompt).await?;
    let (rating, summary) = parse_ai_response(&response);

    sqlx::query("UPDATE albums SET ai_summary = ?, ai_rating = ? WHERE id = ?")
        .bind(&summary)
        .bind(rating)
        .bind(album_id)
        .execute(pool)
        .await?;

    info!("summarized '{}' (rating: {:?})", title, rating);
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

    for (album_id, title, artist, year, genre_json) in &rows {
        let genre: Vec<String> = serde_json::from_str(genre_json).unwrap_or_default();
        let user_prompt = build_rating_prompt(title, artist, *year, &genre);

        match provider.generate(RATING_SYSTEM_PROMPT, &user_prompt).await {
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
    let response = provider.generate(RATING_SYSTEM_PROMPT, &user_prompt).await?;

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
