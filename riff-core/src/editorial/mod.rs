pub mod critiquebrainz;
pub mod discogs;
pub mod lastfm;
pub mod merge;
pub mod wikipedia;

use crate::config::MetadataConfig;
use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

pub struct EditorialEnrichResult {
    pub albums_enriched: u32,
    pub artists_enriched: u32,
    pub errors: Vec<String>,
}

/// Build an HTTP client with a User-Agent (required by Wikipedia).
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("riff-music-server/1.0 (https://github.com/alexmaslar/riff)")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("failed to build HTTP client")
}

/// Enrich all albums and artists missing editorial content.
pub async fn enrich_library_editorial(
    pool: &SqlitePool,
    config: &MetadataConfig,
) -> Result<EditorialEnrichResult> {
    let client = build_client();
    let mut result = EditorialEnrichResult {
        albums_enriched: 0,
        artists_enriched: 0,
        errors: Vec::new(),
    };

    // Albums: enrich those missing a summary and already matched via MusicBrainz
    let albums: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT a.id, a.title, ar.name, a.external_id \
         FROM albums a JOIN artists ar ON a.artist_id = ar.id \
         WHERE a.summary IS NULL AND a.metadata_status = 'matched' \
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;

    let total_albums = albums.len();
    if total_albums > 0 {
        info!("editorial: enriching {} albums", total_albums);
    }

    for (album_id, title, artist_name, external_id) in &albums {
        match enrich_album(pool, &client, config, album_id, title, artist_name, external_id.as_deref()).await {
            Ok(true) => result.albums_enriched += 1,
            Ok(false) => {}
            Err(e) => {
                let msg = format!("{} - {}: {}", artist_name, title, e);
                warn!("editorial album error: {}", msg);
                result.errors.push(msg);
            }
        }
        // Rate limit: MusicBrainz requires 1 req/sec; the MB pre-resolve + concurrent
        // sources need ~1.5s spacing to avoid 503s
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    // Artists: enrich those missing an editorial bio
    let artists: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, name, external_id FROM artists \
         WHERE editorial_bio IS NULL \
         AND id IN (SELECT DISTINCT artist_id FROM albums) \
         LIMIT 100",
    )
    .fetch_all(pool)
    .await?;

    let total_artists = artists.len();
    if total_artists > 0 {
        info!("editorial: enriching {} artists", total_artists);
    }

    for (i, (artist_id, artist_name, external_id)) in artists.iter().enumerate() {
        info!("editorial: artist {}/{} '{}'", i + 1, total_artists, artist_name);
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            enrich_artist(pool, &client, config, artist_id, artist_name, external_id.as_deref()),
        ).await {
            Ok(Ok(true)) => result.artists_enriched += 1,
            Ok(Ok(false)) => {}
            Ok(Err(e)) => {
                let msg = format!("{}: {}", artist_name, e);
                warn!("editorial artist error: {}", msg);
                result.errors.push(msg);
            }
            Err(_) => {
                let msg = format!("{}: timed out after 20s", artist_name);
                warn!("editorial artist timeout: {}", msg);
                result.errors.push(msg);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }

    info!(
        "editorial enrichment complete: {} albums, {} artists, {} errors",
        result.albums_enriched, result.artists_enriched, result.errors.len()
    );

    Ok(result)
}

/// Enrich a single album from all available editorial sources.
async fn enrich_album(
    pool: &SqlitePool,
    client: &reqwest::Client,
    config: &MetadataConfig,
    album_id: &str,
    title: &str,
    artist_name: &str,
    release_group_mbid: Option<&str>,
) -> Result<bool> {
    debug!("editorial: enriching album '{}' by '{}'", title, artist_name);

    // Pre-resolve Wikipedia title via MusicBrainz (with timeout to avoid blocking)
    let wp_title_from_mb = if let Some(mbid) = release_group_mbid {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wikipedia::resolve_title_from_mbid(client, "release-group", mbid),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
    } else {
        None
    };

    // Fetch from all sources concurrently
    let lastfm_future = async {
        if let Some(ref api_key) = config.lastfm_api_key {
            lastfm::get_album_info(client, api_key, artist_name, title).await.ok()
        } else {
            None
        }
    };

    let wp_title_ref = wp_title_from_mb.as_deref();
    let wikipedia_future = async {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wikipedia::search_album(client, title, artist_name, wp_title_ref),
        ).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                debug!("wikipedia album lookup failed for '{}': {}", title, e);
                None
            }
            Err(_) => {
                debug!("wikipedia album lookup timed out for '{}'", title);
                None
            }
        }
    };

    let critiquebrainz_future = async {
        if let Some(mbid) = release_group_mbid {
            critiquebrainz::get_reviews(client, mbid).await.ok()
        } else {
            None
        }
    };

    let discogs_future = async {
        if let Some(ref token) = config.discogs_api_key {
            discogs::get_album_info(client, token, artist_name, title).await.ok()
        } else {
            None
        }
    };

    let (lastfm_result, wikipedia_result, cb_result, discogs_result) =
        tokio::join!(lastfm_future, wikipedia_future, critiquebrainz_future, discogs_future);

    // Merge results
    let merged = merge::merge_album(
        lastfm_result.as_ref(),
        wikipedia_result.as_deref(),
        cb_result.as_ref(),
        discogs_result.as_ref(),
    );

    // Skip if we got nothing useful
    if merged.summary.is_none() && merged.rating.is_none() && merged.keywords.is_empty() && merged.reviews.is_empty() {
        return Ok(false);
    }

    let now = chrono::Utc::now().to_rfc3339();
    let moods_json = serde_json::to_string(&merged.moods).unwrap_or_else(|_| "[]".to_string());
    let descriptors_json = serde_json::to_string(&merged.descriptors).unwrap_or_else(|_| "[]".to_string());
    let keywords_json = serde_json::to_string(&merged.keywords).unwrap_or_else(|_| "[]".to_string());

    // Update album
    sqlx::query(
        "UPDATE albums SET summary = ?, summary_source = ?, rating = ?, rating_sources = ?, \
         moods = ?, descriptors = ?, keywords = ?, summary_updated_at = ? \
         WHERE id = ?",
    )
    .bind(&merged.summary)
    .bind(&merged.summary_source)
    .bind(merged.rating)
    .bind(&merged.rating_sources)
    .bind(&moods_json)
    .bind(&descriptors_json)
    .bind(&keywords_json)
    .bind(&now)
    .bind(album_id)
    .execute(pool)
    .await?;

    // Insert editorial reviews
    for review in &merged.reviews {
        let review_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT OR REPLACE INTO editorial_reviews \
             (id, entity_type, entity_id, source, text, rating, rating_count, license, source_updated_at) \
             VALUES (?, 'album', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&review_id)
        .bind(album_id)
        .bind(&review.source)
        .bind(&review.text)
        .bind(review.rating)
        .bind(review.rating_count.map(|c| c as i64))
        .bind(&review.license)
        .bind(&review.source_updated_at)
        .execute(pool)
        .await?;
    }

    Ok(true)
}

/// Enrich a single artist from all available editorial sources.
async fn enrich_artist(
    pool: &SqlitePool,
    client: &reqwest::Client,
    config: &MetadataConfig,
    artist_id: &str,
    artist_name: &str,
    artist_mbid: Option<&str>,
) -> Result<bool> {
    info!("editorial: enriching artist '{}'", artist_name);

    // Pre-resolve Wikipedia title via MusicBrainz (with timeout)
    let wp_title_from_mb = if let Some(mbid) = artist_mbid {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wikipedia::resolve_title_from_mbid(client, "artist", mbid),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
    } else {
        None
    };

    let wp_title_ref = wp_title_from_mb.as_deref();
    let wikipedia_future = async {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wikipedia::search_artist(client, artist_name, wp_title_ref),
        ).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                debug!("wikipedia artist lookup failed for '{}': {}", artist_name, e);
                None
            }
            Err(_) => {
                debug!("wikipedia artist lookup timed out for '{}'", artist_name);
                None
            }
        }
    };

    let lastfm_future = async {
        if let Some(ref api_key) = config.lastfm_api_key {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                lastfm::get_artist_info(client, api_key, artist_name),
            ).await {
                Ok(Ok(result)) => Some(result),
                Ok(Err(e)) => {
                    debug!("lastfm artist lookup failed for '{}': {}", artist_name, e);
                    None
                }
                Err(_) => {
                    debug!("lastfm artist lookup timed out for '{}'", artist_name);
                    None
                }
            }
        } else {
            None
        }
    };

    let discogs_future = async {
        if let Some(ref token) = config.discogs_api_key {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                discogs::get_artist_profile(client, token, artist_name),
            ).await {
                Ok(Ok(result)) => Some(result),
                Ok(Err(e)) => {
                    debug!("discogs artist lookup failed for '{}': {}", artist_name, e);
                    None
                }
                Err(_) => {
                    debug!("discogs artist lookup timed out for '{}'", artist_name);
                    None
                }
            }
        } else {
            None
        }
    };

    let (wikipedia_result, lastfm_result, discogs_result) =
        tokio::join!(wikipedia_future, lastfm_future, discogs_future);

    let merged = merge::merge_artist(
        wikipedia_result.as_deref(),
        lastfm_result.as_ref(),
        discogs_result.as_ref(),
    );

    if merged.bio.is_none() {
        return Ok(false);
    }

    let now = chrono::Utc::now().to_rfc3339();

    sqlx::query(
        "UPDATE artists SET editorial_bio = ?, editorial_bio_source = ?, editorial_bio_updated_at = ? \
         WHERE id = ?",
    )
    .bind(&merged.bio)
    .bind(&merged.bio_source)
    .bind(&now)
    .bind(artist_id)
    .execute(pool)
    .await?;

    Ok(true)
}
