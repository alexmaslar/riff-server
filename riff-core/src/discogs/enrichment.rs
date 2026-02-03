use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::DiscogsConfig;
use super::client::DiscogsClient;
use super::matching;

pub struct EnrichmentResult {
    pub albums_enriched: u32,
    pub artists_enriched: u32,
    pub covers_downloaded: u32,
    pub errors: Vec<String>,
}

pub async fn enrich_library(
    pool: &SqlitePool,
    config: &DiscogsConfig,
) -> anyhow::Result<EnrichmentResult> {
    let client = DiscogsClient::new(config)?;

    let mut result = EnrichmentResult {
        albums_enriched: 0,
        artists_enriched: 0,
        covers_downloaded: 0,
        errors: Vec::new(),
    };

    // Phase 1: Albums
    enrich_albums(pool, &client, config, &mut result).await;

    // Phase 2: Artists
    enrich_artists(pool, &client, &mut result).await;

    info!(
        "enrichment complete: {} albums, {} artists, {} covers, {} errors",
        result.albums_enriched, result.artists_enriched, result.covers_downloaded, result.errors.len()
    );

    Ok(result)
}

/// Enrich a single album by ID. Returns true if a Discogs match was found.
pub async fn enrich_album(
    pool: &SqlitePool,
    config: &DiscogsConfig,
    album_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(String, String, Option<i32>, Option<String>)> = sqlx::query_as(
        "SELECT a.title, ar.name, a.year, a.cover_art_path \
         FROM albums a JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.id = ?"
    )
    .bind(album_id)
    .fetch_optional(pool)
    .await?;

    let (title, artist_name, year, cover_path) = match row {
        Some(r) => r,
        None => anyhow::bail!("album not found: {}", album_id),
    };

    let client = DiscogsClient::new(config)?;
    let matched = enrich_one_album(
        pool, &client, config, album_id, &title, &artist_name, year, cover_path.as_deref(),
    ).await?;

    // Also enrich the artist if we got a discogs_id from the album match
    if matched {
        let artist_row: Option<(String, String)> = sqlx::query_as(
            "SELECT ar.id, ar.discogs_id FROM artists ar \
             JOIN albums a ON a.artist_id = ar.id \
             WHERE a.id = ? AND ar.discogs_id IS NOT NULL AND ar.bio IS NULL"
        )
        .bind(album_id)
        .fetch_optional(pool)
        .await?;

        if let Some((artist_id, discogs_id)) = artist_row {
            if let Ok(did) = discogs_id.parse::<u64>() {
                if let Err(e) = enrich_one_artist(pool, &client, &artist_id, did).await {
                    warn!("artist enrichment failed for {}: {}", artist_id, e);
                }
            }
        }
    }

    Ok(matched)
}

async fn enrich_albums(
    pool: &SqlitePool,
    client: &DiscogsClient,
    config: &DiscogsConfig,
    result: &mut EnrichmentResult,
) {
    let rows: Vec<(String, String, String, Option<i32>, Option<String>)> = match sqlx::query_as(
        "SELECT a.id, a.title, ar.name, a.year, a.cover_art_path \
         FROM albums a JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.metadata_status IN ('pending', 'not_found')"
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            result.errors.push(format!("failed to query albums: {}", e));
            return;
        }
    };

    info!("enriching {} albums from Discogs", rows.len());

    for (album_id, title, artist_name, year, cover_path) in &rows {
        match enrich_one_album(pool, client, config, album_id, title, artist_name, *year, cover_path.as_deref()).await {
            Ok(true) => result.albums_enriched += 1,
            Ok(false) => {} // no match found, skip
            Err(e) => {
                warn!("enrichment error for album '{}': {}", title, e);
                result.errors.push(format!("{}: {}", title, e));
            }
        }
    }
}

async fn enrich_one_album(
    pool: &SqlitePool,
    client: &DiscogsClient,
    config: &DiscogsConfig,
    album_id: &str,
    title: &str,
    artist_name: &str,
    year: Option<i32>,
    cover_path: Option<&str>,
) -> anyhow::Result<bool> {
    let results = client.search_release(artist_name, title, year).await?;

    if results.is_empty() {
        debug!(
            "no Discogs search results for '{}' by '{}'",
            title, artist_name
        );
    } else {
        debug!(
            "Discogs search returned {} results for '{}' by '{}'",
            results.len(), title, artist_name
        );
    }

    let track_count: Option<i64> = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?"
    )
    .bind(album_id)
    .fetch_optional(pool)
    .await?;

    let candidate = matching::best_match(
        results,
        artist_name,
        title,
        year,
        track_count.map(|c| c as usize),
    );

    let candidate = match candidate {
        Some(c) => c,
        None => {
            debug!(
                "no Discogs match above threshold for '{}' by '{}'",
                title, artist_name
            );
            sqlx::query("UPDATE albums SET metadata_status = 'not_found' WHERE id = ?")
                .bind(album_id)
                .execute(pool)
                .await?;
            return Ok(false);
        }
    };

    info!(
        "matched '{}' -> Discogs release {} (score: {:.2})",
        title, candidate.result.id, candidate.score
    );

    let release = client.get_release(candidate.result.id).await?;

    let label = release.labels.first().map(|l| l.name.clone());
    let catno = release.labels.first().and_then(|l| l.catno.clone());
    let style_json = serde_json::to_string(&release.styles)?;
    let genre_json = serde_json::to_string(&release.genres)?;
    let all_labels_json = serde_json::to_string(&release.labels)?;
    let release_notes = release.notes.as_deref().map(clean_discogs_markup);
    let is_compilation = release.formats.iter().any(|f| {
        f.descriptions.iter().any(|d| d.eq_ignore_ascii_case("compilation"))
    });

    sqlx::query(
        "UPDATE albums SET discogs_id = ?, label = ?, catalog_number = ?, style = ?, genre = ?, country = ?, release_notes = ?, all_labels = ?, is_compilation = ?, metadata_status = 'matched' WHERE id = ?"
    )
    .bind(release.id.to_string())
    .bind(&label)
    .bind(&catno)
    .bind(&style_json)
    .bind(&genre_json)
    .bind(&release.country)
    .bind(&release_notes)
    .bind(&all_labels_json)
    .bind(is_compilation as i32)
    .bind(album_id)
    .execute(pool)
    .await?;

    // Set discogs_id on the artist for Phase 2
    if let Some(discogs_artist) = release.artists.first() {
        sqlx::query(
            "UPDATE artists SET discogs_id = ? WHERE id = (SELECT artist_id FROM albums WHERE id = ?) AND discogs_id IS NULL"
        )
        .bind(discogs_artist.id.to_string())
        .bind(album_id)
        .execute(pool)
        .await?;
    }

    // Store album credits from extraartists
    sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
        .bind(album_id)
        .execute(pool)
        .await?;

    for (i, extra) in release.extraartists.iter().enumerate() {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO album_credits (id, album_id, artist_name, role, discogs_artist_id, sort_order) \
             VALUES (?, ?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(album_id)
        .bind(&extra.name)
        .bind(&extra.role)
        .bind(extra.id.to_string())
        .bind(i as i32)
        .execute(pool)
        .await?;
    }

    // Download cover art if missing and enabled
    if config.download_covers && cover_path.is_none() {
        if let Some(image) = release.images.iter().find(|i| i.image_type == "primary").or(release.images.first()) {
            match download_cover(client, &image.uri, pool, album_id).await {
                Ok(true) => info!("downloaded cover for '{}'", title),
                Ok(false) => {}
                Err(e) => warn!("cover download failed for '{}': {}", title, e),
            }
        }
    }

    Ok(true)
}

async fn download_cover(
    client: &DiscogsClient,
    image_url: &str,
    pool: &SqlitePool,
    album_id: &str,
) -> anyhow::Result<bool> {
    // Find the album directory from one of its tracks
    let track_path: Option<(String,)> = sqlx::query_as(
        "SELECT file_path FROM tracks WHERE album_id = ? LIMIT 1"
    )
    .bind(album_id)
    .fetch_optional(pool)
    .await?;

    let track_path = match track_path {
        Some((p,)) => p,
        None => return Ok(false),
    };

    let album_dir = match Path::new(&track_path).parent() {
        Some(d) => d,
        None => return Ok(false),
    };

    let cover_path = album_dir.join("cover.jpg");
    let bytes = client.download_bytes(image_url).await?;
    tokio::fs::write(&cover_path, &bytes).await?;

    let cover_str = cover_path.to_string_lossy().to_string();
    sqlx::query("UPDATE albums SET cover_art_path = ? WHERE id = ?")
        .bind(&cover_str)
        .bind(album_id)
        .execute(pool)
        .await?;

    Ok(true)
}

async fn enrich_artists(
    pool: &SqlitePool,
    client: &DiscogsClient,
    result: &mut EnrichmentResult,
) {
    let rows: Vec<(String, String)> = match sqlx::query_as(
        "SELECT id, discogs_id FROM artists WHERE discogs_id IS NOT NULL AND bio IS NULL"
    )
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            result.errors.push(format!("failed to query artists: {}", e));
            return;
        }
    };

    info!("enriching {} artists from Discogs", rows.len());

    for (artist_id, discogs_id) in &rows {
        let discogs_id: u64 = match discogs_id.parse() {
            Ok(id) => id,
            Err(e) => {
                result.errors.push(format!("invalid discogs_id for artist {}: {}", artist_id, e));
                continue;
            }
        };

        match enrich_one_artist(pool, client, &artist_id, discogs_id).await {
            Ok(()) => result.artists_enriched += 1,
            Err(e) => {
                warn!("artist enrichment error for {}: {}", artist_id, e);
                result.errors.push(format!("artist {}: {}", artist_id, e));
            }
        }
    }
}

async fn enrich_one_artist(
    pool: &SqlitePool,
    client: &DiscogsClient,
    artist_id: &str,
    discogs_id: u64,
) -> anyhow::Result<()> {
    let detail = client.get_artist(discogs_id).await?;

    let bio = detail.profile.map(|p| clean_discogs_markup(&p));
    let image_url = detail.images.first().map(|i| i.uri.clone());

    sqlx::query("UPDATE artists SET bio = ?, image_url = ? WHERE id = ?")
        .bind(&bio)
        .bind(&image_url)
        .bind(artist_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Clean Discogs markup like [a=Artist Name] -> Artist Name
fn clean_discogs_markup(text: &str) -> String {
    let mut result = text.to_string();
    // [a=Name] -> Name, [l=Label] -> Label, etc.
    while let Some(start) = result.find("[a=") {
        if let Some(end) = result[start..].find(']') {
            let name = &result[start + 3..start + end];
            result = format!("{}{}{}", &result[..start], name, &result[start + end + 1..]);
        } else {
            break;
        }
    }
    while let Some(start) = result.find("[l=") {
        if let Some(end) = result[start..].find(']') {
            let name = &result[start + 3..start + end];
            result = format!("{}{}{}", &result[..start], name, &result[start + end + 1..]);
        } else {
            break;
        }
    }
    result
}
