use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::EnrichmentConfig;
use crate::deezer;
use super::client::MusicBrainzClient;
use super::matching;

pub struct EnrichmentResult {
    pub albums_enriched: u32,
    pub artists_enriched: u32,
    pub covers_downloaded: u32,
    pub errors: Vec<String>,
}

pub async fn enrich_library(
    pool: &SqlitePool,
) -> anyhow::Result<EnrichmentResult> {
    let client = MusicBrainzClient::new()?;

    let config = crate::config::Config::load()
        .map(|c| c.metadata.enrichment)
        .unwrap_or_default();

    let mut result = EnrichmentResult {
        albums_enriched: 0,
        artists_enriched: 0,
        covers_downloaded: 0,
        errors: Vec::new(),
    };

    // Album enrichment
    enrich_albums(pool, &client, &config, &mut result).await;

    info!(
        "enrichment complete: {} albums, {} covers, {} errors",
        result.albums_enriched, result.covers_downloaded, result.errors.len()
    );

    Ok(result)
}

/// Enrich a single album by ID. Returns true if a MusicBrainz match was found.
pub async fn enrich_album(
    pool: &SqlitePool,
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

    let client = MusicBrainzClient::new()?;
    let config = crate::config::Config::load()
        .map(|c| c.metadata.enrichment)
        .unwrap_or_default();

    let matched = enrich_one_album(
        pool, &client, &config, album_id, &title, &artist_name, year, cover_path.as_deref(),
    ).await?;

    Ok(matched)
}

async fn enrich_albums(
    pool: &SqlitePool,
    client: &MusicBrainzClient,
    config: &EnrichmentConfig,
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

    info!("enriching {} albums from MusicBrainz", rows.len());

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
    client: &MusicBrainzClient,
    config: &EnrichmentConfig,
    album_id: &str,
    title: &str,
    artist_name: &str,
    year: Option<i32>,
    cover_path: Option<&str>,
) -> anyhow::Result<bool> {
    let results = client.search_release(artist_name, title, year).await?;

    if results.is_empty() {
        debug!(
            "no MusicBrainz search results for '{}' by '{}'",
            title, artist_name
        );
    } else {
        debug!(
            "MusicBrainz search returned {} results for '{}' by '{}'",
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
                "no MusicBrainz match above threshold for '{}' by '{}'",
                title, artist_name
            );
            sqlx::query("UPDATE albums SET metadata_status = 'not_found' WHERE id = ?")
                .bind(album_id)
                .execute(pool)
                .await?;
            return Ok(false);
        }
    };

    let mbid = &candidate.result.id;
    info!(
        "matched '{}' -> MusicBrainz release {} (score: {:.2})",
        title, mbid, candidate.score
    );

    let release = client.get_release(mbid).await?;

    let label = release.label_info.first().and_then(|li| li.label.as_ref().map(|l| l.name.clone()));
    let catno = release.label_info.first().and_then(|li| li.catalog_number.clone());

    // Genres sorted by vote count
    let mut genres: Vec<&str> = release.genres.iter()
        .map(|g| g.name.as_str())
        .collect();
    genres.sort();
    let genre_json = serde_json::to_string(&genres)?;

    // Tags sorted by vote count → style column
    let mut tags: Vec<&str> = release.tags.iter()
        .map(|t| t.name.as_str())
        .collect();
    tags.sort();
    let style_json = serde_json::to_string(&tags)?;

    // Serialize label_info for all_labels
    let all_labels: Vec<_> = release.label_info.iter()
        .filter_map(|li| li.label.as_ref().map(|l| serde_json::json!({
            "name": l.name,
            "catno": li.catalog_number,
        })))
        .collect();
    let all_labels_json = serde_json::to_string(&all_labels)?;

    let release_notes = release.annotation.as_deref();

    let is_compilation = release.release_group
        .as_ref()
        .map(|rg| rg.secondary_types.iter().any(|t| t.eq_ignore_ascii_case("compilation")))
        .unwrap_or(false);

    sqlx::query(
        "UPDATE albums SET discogs_id = ?, label = ?, catalog_number = ?, style = ?, genre = ?, country = ?, release_notes = ?, all_labels = ?, is_compilation = ?, metadata_status = 'matched' WHERE id = ?"
    )
    .bind(&release.id)
    .bind(&label)
    .bind(&catno)
    .bind(&style_json)
    .bind(&genre_json)
    .bind(&release.country)
    .bind(release_notes)
    .bind(&all_labels_json)
    .bind(is_compilation as i32)
    .bind(album_id)
    .execute(pool)
    .await?;

    // Set MBID on the artist
    if let Some(ac) = release.artist_credit.first() {
        sqlx::query(
            "UPDATE artists SET discogs_id = ? WHERE id = (SELECT artist_id FROM albums WHERE id = ?) AND discogs_id IS NULL"
        )
        .bind(&ac.artist.id)
        .bind(album_id)
        .execute(pool)
        .await?;
    }

    // Store album credits from relations
    sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
        .bind(album_id)
        .execute(pool)
        .await?;

    let mut credit_index = 0;
    for rel in &release.relations {
        if let Some(artist) = &rel.artist {
            let id = Uuid::new_v4().to_string();
            // Capitalize first letter of role
            let role = capitalize_first(&rel.relation_type);
            sqlx::query(
                "INSERT INTO album_credits (id, album_id, artist_name, role, discogs_artist_id, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?)"
            )
            .bind(&id)
            .bind(album_id)
            .bind(&artist.name)
            .bind(&role)
            .bind(&artist.id)
            .bind(credit_index)
            .execute(pool)
            .await?;
            credit_index += 1;
        }
    }

    // Download cover art from Cover Art Archive if missing and enabled
    if config.download_covers && cover_path.is_none() {
        match download_cover(client, mbid, pool, album_id).await {
            Ok(true) => {
                info!("downloaded cover for '{}'", title);
            }
            Ok(false) => {}
            Err(e) => warn!("cover download failed for '{}': {}", title, e),
        }
    }

    Ok(true)
}

async fn download_cover(
    client: &MusicBrainzClient,
    mbid: &str,
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

    let bytes = match client.download_cover(mbid).await? {
        Some(b) => b,
        None => return Ok(false), // 404 — no cover available
    };

    let cover_path = album_dir.join("cover.jpg");
    tokio::fs::write(&cover_path, &bytes).await?;

    let cover_str = cover_path.to_string_lossy().to_string();
    sqlx::query("UPDATE albums SET cover_art_path = ? WHERE id = ?")
        .bind(&cover_str)
        .bind(album_id)
        .execute(pool)
        .await?;

    Ok(true)
}

/// Fetch Deezer top tracks for artists that haven't been enriched yet (or stale > 30 days).
/// Also fetches artist images from Deezer when not already set.
pub async fn enrich_artist_top_tracks(
    pool: &SqlitePool,
) -> anyhow::Result<u32> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.id, a.name FROM artists a \
         WHERE a.name IS NOT NULL \
         AND NOT EXISTS ( \
             SELECT 1 FROM artist_top_tracks att \
             WHERE att.artist_id = a.id \
             AND att.updated_at > datetime('now', '-30 days') \
         )"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        info!("no artists need top tracks enrichment");
        return Ok(0);
    }

    info!("enriching {} artists with Deezer top tracks", rows.len());

    let client = reqwest::Client::new();
    let mut enriched = 0u32;

    for (artist_id, artist_name) in &rows {
        match deezer::fetch_top_tracks(&client, artist_name, 10).await {
            Ok((tracks, image_url)) if !tracks.is_empty() => {
                // Delete existing rows then insert fresh
                sqlx::query("DELETE FROM artist_top_tracks WHERE artist_id = ?")
                    .bind(artist_id)
                    .execute(pool)
                    .await?;

                for track in &tracks {
                    sqlx::query(
                        "INSERT INTO artist_top_tracks (artist_id, track_name, rank, playcount, listeners, updated_at) \
                         VALUES (?, ?, ?, ?, 0, datetime('now'))"
                    )
                    .bind(artist_id)
                    .bind(&track.name)
                    .bind(track.rank)
                    .bind(track.popularity)
                    .execute(pool)
                    .await?;
                }

                // Update artist image from Deezer if not already set
                if let Some(img_url) = &image_url {
                    sqlx::query("UPDATE artists SET image_url = ? WHERE id = ? AND image_url IS NULL")
                        .bind(img_url)
                        .bind(artist_id)
                        .execute(pool)
                        .await?;
                }

                enriched += 1;
                debug!("Deezer: {} top tracks for '{}'", tracks.len(), artist_name);
            }
            Ok(_) => {
                // No tracks returned — insert a sentinel so we don't re-query immediately
                sqlx::query(
                    "INSERT OR REPLACE INTO artist_top_tracks (artist_id, track_name, rank, playcount, listeners, updated_at) \
                     VALUES (?, '', 0, 0, 0, datetime('now'))"
                )
                .bind(artist_id)
                .execute(pool)
                .await?;
            }
            Err(e) => {
                warn!("Deezer error for '{}': {}", artist_name, e);
            }
        }

        // Rate limit: ~1 request/second (Deezer allows ~30/min)
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    info!("Deezer top tracks enrichment complete: {} artists", enriched);
    Ok(enriched)
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}
