use std::path::Path;

use sqlx::SqlitePool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use std::sync::Arc;

use crate::deezer;
use crate::plugin::capabilities::StreamingProvider;
use super::client::MusicBrainzClient;
use super::matching;

pub struct EnrichmentResult {
    pub albums_enriched: u32,
    pub artists_enriched: u32,
    pub covers_downloaded: u32,
    pub errors: Vec<String>,
    pub enriched_album_ids: Vec<String>,
    pub enriched_artist_ids: Vec<String>,
}

pub async fn enrich_library(
    pool: &SqlitePool,
) -> anyhow::Result<EnrichmentResult> {
    let client = MusicBrainzClient::new()?;

    let mut result = EnrichmentResult {
        albums_enriched: 0,
        artists_enriched: 0,
        covers_downloaded: 0,
        errors: Vec::new(),
        enriched_album_ids: Vec::new(),
        enriched_artist_ids: Vec::new(),
    };

    // Album enrichment
    enrich_albums(pool, &client, &mut result).await;

    info!(
        albums = result.albums_enriched,
        covers = result.covers_downloaded,
        errors = result.errors.len(),
        "enrichment complete",
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

    let matched = enrich_one_album(
        pool, &client, album_id, &title, &artist_name, year, cover_path.as_deref(),
    ).await?;

    Ok(matched)
}

async fn enrich_albums(
    pool: &SqlitePool,
    client: &MusicBrainzClient,
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

    info!(count = rows.len(), "enriching albums from MusicBrainz");

    for (album_id, title, artist_name, year, cover_path) in &rows {
        match enrich_one_album(pool, client, album_id, title, artist_name, *year, cover_path.as_deref()).await {
            Ok(true) => {
                result.albums_enriched += 1;
                result.enriched_album_ids.push(album_id.clone());
            }
            Ok(false) => {} // no match found, skip
            Err(e) => {
                warn!(album = %title, error = %e, "enrichment error");
                result.errors.push(format!("{}: {}", title, e));
            }
        }
    }

    // Collect artist IDs from enriched albums
    if !result.enriched_album_ids.is_empty() {
        let placeholders: String = result.enriched_album_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!("SELECT DISTINCT artist_id FROM albums WHERE id IN ({})", placeholders);
        let mut q = sqlx::query_scalar::<_, String>(&query);
        for id in &result.enriched_album_ids {
            q = q.bind(id);
        }
        if let Ok(artist_ids) = q.fetch_all(pool).await {
            result.enriched_artist_ids = artist_ids;
        }
    }
}

async fn enrich_one_album(
    pool: &SqlitePool,
    client: &MusicBrainzClient,
    album_id: &str,
    title: &str,
    artist_name: &str,
    year: Option<i32>,
    cover_path: Option<&str>,
) -> anyhow::Result<bool> {
    // Strip trailing parenthetical suffixes like "(Deluxe Edition)" for better search results
    let clean_title = title
        .rfind('(')
        .filter(|&p| p > 0)
        .map(|p| title[..p].trim_end())
        .unwrap_or(title);

    let results = client.search_release(artist_name, clean_title, year).await?;

    if results.is_empty() {
        debug!(album = %title, artist = %artist_name, "no MusicBrainz search results");
    } else {
        debug!(album = %title, artist = %artist_name, count = results.len(), "MusicBrainz search results");
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
            debug!(album = %title, artist = %artist_name, "no MusicBrainz match above threshold");
            sqlx::query("UPDATE albums SET metadata_status = 'not_found' WHERE id = ?")
                .bind(album_id)
                .execute(pool)
                .await?;
            return Ok(false);
        }
    };

    let mbid = &candidate.result.id;
    info!(album = %title, mbid = %mbid, score = format_args!("{:.2}", candidate.score), "matched MusicBrainz release");

    let release = client.get_release(mbid).await?;

    let label = release.label_info.first().and_then(|li| li.label.as_ref().map(|l| l.name.clone()));
    let catno = release.label_info.first().and_then(|li| li.catalog_number.clone());

    // Genres sorted alphabetically, title-cased
    let mut genres: Vec<String> = release.genres.iter()
        .map(|g| crate::db::title_case_genre(&g.name))
        .collect();

    // Fallback: release-group → artist genres
    if genres.is_empty() {
        genres = fetch_genre_fallbacks(client, &release).await;
    }

    genres.sort();
    genres.dedup();
    let genre_json = serde_json::to_string(&genres)?;

    // Tags sorted alphabetically, title-cased → style column
    let mut tags: Vec<String> = release.tags.iter()
        .map(|t| crate::db::title_case_genre(&t.name))
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
        "UPDATE albums SET external_id = ?, label = ?, catalog_number = ?, style = ?, genre = ?, country = ?, release_notes = ?, all_labels = ?, is_compilation = ?, release_date = ?, metadata_status = 'matched' WHERE id = ?"
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
    .bind(&release.date)
    .bind(album_id)
    .execute(pool)
    .await?;

    // Set MBID on the artist
    if let Some(ac) = release.artist_credit.first() {
        sqlx::query(
            "UPDATE artists SET external_id = ? WHERE id = (SELECT artist_id FROM albums WHERE id = ?) AND external_id IS NULL"
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
                "INSERT INTO album_credits (id, album_id, artist_name, role, external_artist_id, sort_order) \
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

    // Download cover art from Cover Art Archive if missing
    if cover_path.is_none() {
        match download_cover(client, mbid, pool, album_id).await {
            Ok(true) => {
                info!(album = %title, "downloaded cover");
            }
            Ok(false) => {}
            Err(e) => warn!(album = %title, error = %e, "cover download failed"),
        }
    }

    // Enrich tracks with ISRCs from MusicBrainz recordings
    enrich_track_isrcs(pool, album_id, &release).await;

    Ok(true)
}

async fn fetch_genre_fallbacks(
    client: &MusicBrainzClient,
    release: &super::types::MBReleaseDetail,
) -> Vec<String> {
    // 1. Try release-group genres
    if let Some(rg) = &release.release_group {
        if let Ok(rg_detail) = client.get_release_group(&rg.id).await {
            if !rg_detail.genres.is_empty() {
                let mut genres: Vec<String> = rg_detail.genres.iter()
                    .map(|g| crate::db::title_case_genre(&g.name))
                    .collect();
                genres.sort();
                genres.dedup();
                return genres;
            }
        }
    }

    // 2. Try artist genres
    if let Some(ac) = release.artist_credit.first() {
        if let Ok(artist_detail) = client.get_artist(&ac.artist.id).await {
            if !artist_detail.genres.is_empty() {
                let mut genres: Vec<String> = artist_detail.genres.iter()
                    .map(|g| crate::db::title_case_genre(&g.name))
                    .collect();
                genres.sort();
                genres.dedup();
                return genres;
            }
        }
    }

    Vec::new()
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

/// Fetch artist images from Spotify via MusicBrainz URL relations, with streaming provider fallback.
/// Phase 1: MusicBrainz → Spotify artist link → oEmbed → 640px image (no auth needed).
/// Phase 2: For remaining artists, search registered streaming providers by name.
pub async fn enrich_artist_images_spotify(
    pool: &SqlitePool,
    streaming_providers: &[Arc<dyn StreamingProvider>],
) -> anyhow::Result<Vec<String>> {
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("RiffServer/0.1 (riff-music-server)")
        .build()?;
    let mb_client = MusicBrainzClient::new()?;
    let mut enriched_ids: Vec<String> = Vec::new();

    // Phase 1: Spotify via MusicBrainz links
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, external_id FROM artists \
         WHERE image_url IS NULL AND external_id IS NOT NULL"
    )
    .fetch_all(pool)
    .await?;

    if !rows.is_empty() {
        info!(count = rows.len(), "Spotify image enrichment: artists with MusicBrainz IDs");
    }

    for (artist_id, artist_name, mbid) in &rows {
        let spotify_id = match mb_client.get_artist(mbid).await {
            Ok(detail) => extract_spotify_artist_id(&detail.relations),
            Err(e) => {
                warn!(artist = %artist_name, error = %e, "MusicBrainz artist lookup failed");
                None
            }
        };

        if let Some(ref sid) = spotify_id {
            match crate::spotify::fetch_artist_image(&http_client, sid).await {
                Ok(Some(url)) => {
                    sqlx::query("UPDATE artists SET image_url = ? WHERE id = ? AND image_url IS NULL")
                        .bind(&url)
                        .bind(artist_id)
                        .execute(pool)
                        .await?;
                    enriched_ids.push(artist_id.clone());
                    debug!(artist = %artist_name, "Spotify image found");
                }
                Ok(None) => debug!(artist = %artist_name, "no Spotify image"),
                Err(e) => warn!(artist = %artist_name, error = %e, "Spotify image error"),
            }
        } else {
            debug!(artist = %artist_name, "no Spotify link");
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // Phase 2: Streaming provider fallback for remaining artists
    if !streaming_providers.is_empty() {
        let remaining: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, name FROM artists WHERE image_url IS NULL"
        )
        .fetch_all(pool)
        .await?;

        if !remaining.is_empty() {
            info!(count = remaining.len(), "streaming provider image fallback: artists remaining");
        }

        for (artist_id, artist_name) in &remaining {
            let mut found = false;
            for provider in streaming_providers {
                match provider.search(artist_name, 1).await {
                    Ok(results) => {
                        if let Some(artist) = results.artists.iter().find(|a| {
                            a.name.eq_ignore_ascii_case(artist_name)
                        }) {
                            if let Some(ref url) = artist.image_url {
                                sqlx::query("UPDATE artists SET image_url = ? WHERE id = ? AND image_url IS NULL")
                                    .bind(url)
                                    .bind(artist_id)
                                    .execute(pool)
                                    .await?;
                                enriched_ids.push(artist_id.clone());
                                debug!(artist = %artist_name, provider = %provider.provider_name(), "streaming provider image found");
                                found = true;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        debug!(provider = %provider.provider_name(), artist = %artist_name, error = %e, "streaming provider search failed");
                    }
                }
            }
            if !found {
                debug!(artist = %artist_name, "no streaming provider image");
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    if !enriched_ids.is_empty() {
        info!(count = enriched_ids.len(), "artist image enrichment complete");
    }
    Ok(enriched_ids)
}

/// Extract a Spotify artist ID from MusicBrainz URL relations.
/// Looks for a URL matching `open.spotify.com/artist/` and extracts the 22-char ID.
fn extract_spotify_artist_id(relations: &[super::types::MBRelation]) -> Option<String> {
    for rel in relations {
        if let Some(url_res) = &rel.url {
            if let Some(pos) = url_res.resource.find("open.spotify.com/artist/") {
                let after = &url_res.resource[pos + "open.spotify.com/artist/".len()..];
                let id: String = after.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if !id.is_empty() {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Fetch Deezer top tracks for artists that haven't been enriched yet (or stale > 30 days).
/// Also fetches artist images from Deezer when not already set.
pub async fn enrich_artist_top_tracks(
    pool: &SqlitePool,
    client: &reqwest::Client,
) -> anyhow::Result<Vec<String>> {
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
        info!("no artists need top-tracks enrichment");
        return Ok(Vec::new());
    }

    info!(count = rows.len(), "enriching artists with Deezer top tracks");
    let mut enriched_ids: Vec<String> = Vec::new();

    for (artist_id, artist_name) in &rows {
        match deezer::fetch_top_tracks(&client, artist_name, 10).await {
            Ok(tracks) if !tracks.is_empty() => {
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

                enriched_ids.push(artist_id.clone());
                debug!(artist = %artist_name, count = tracks.len(), "Deezer top tracks found");
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
                warn!(artist = %artist_name, error = %e, "Deezer error");
            }
        }

        // Rate limit: ~1 request/second (Deezer allows ~30/min)
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    info!(count = enriched_ids.len(), "Deezer top-tracks enrichment complete");
    Ok(enriched_ids)
}

/// Fetch artist biographies from Wikipedia via MusicBrainz Wikidata relations.
/// Resolves Wikidata ID → English Wikipedia article title → Wikipedia summary extract.
pub async fn enrich_artist_bios_wikipedia(
    pool: &SqlitePool,
    http_client: &reqwest::Client,
) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, external_id FROM artists \
         WHERE bio_status = 'pending' AND external_id IS NOT NULL"
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        info!("no artists need Wikipedia bio enrichment");
        return Ok(Vec::new());
    }

    info!(count = rows.len(), "enriching artists with Wikipedia bios");

    let mb_client = MusicBrainzClient::new()?;

    let mut enriched_ids: Vec<String> = Vec::new();

    for (artist_id, artist_name, mbid) in &rows {
        // Get Wikidata ID from MusicBrainz relations
        let wikidata_id = match mb_client.get_artist(mbid).await {
            Ok(detail) => extract_wikidata_id(&detail.relations),
            Err(e) => {
                warn!(artist = %artist_name, error = %e, "MusicBrainz lookup failed");
                continue;
            }
        };

        let Some(qid) = wikidata_id else {
            debug!(artist = %artist_name, "no Wikidata link");
            sqlx::query("UPDATE artists SET bio_status = 'not_found' WHERE id = ?")
                .bind(artist_id)
                .execute(pool)
                .await?;
            continue;
        };

        // Resolve Wikidata ID → English Wikipedia title via sitelinks
        let wiki_title = match resolve_wikidata_to_wikipedia(&http_client, &qid).await {
            Ok(Some(title)) => title,
            Ok(None) => {
                debug!(artist = %artist_name, wikidata_id = %qid, "no English Wikipedia article");
                sqlx::query("UPDATE artists SET bio_status = 'not_found' WHERE id = ?")
                    .bind(artist_id)
                    .execute(pool)
                    .await?;
                continue;
            }
            Err(e) => {
                warn!(artist = %artist_name, wikidata_id = %qid, error = %e, "Wikidata lookup failed, will retry");
                continue;
            }
        };

        // Rate limit between Wikidata and Wikipedia calls
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Fetch plain-text extract from Wikipedia REST API
        let url = format!(
            "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
            wiki_title
        );
        match http_client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                #[derive(serde::Deserialize)]
                struct WikiSummary {
                    extract: Option<String>,
                }
                match resp.json::<WikiSummary>().await {
                    Ok(summary) => {
                        if let Some(extract) = summary.extract.filter(|s| s.len() > 20) {
                            sqlx::query(
                                "UPDATE artists SET bio = ?, bio_status = 'found' WHERE id = ?"
                            )
                            .bind(&extract)
                            .bind(artist_id)
                            .execute(pool)
                            .await?;
                            enriched_ids.push(artist_id.clone());
                            debug!(artist = %artist_name, chars = extract.len(), "Wikipedia bio found");
                        } else {
                            sqlx::query("UPDATE artists SET bio_status = 'not_found' WHERE id = ?")
                                .bind(artist_id)
                                .execute(pool)
                                .await?;
                        }
                    }
                    Err(e) => warn!(artist = %artist_name, error = %e, "Wikipedia JSON parse error"),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                if status == reqwest::StatusCode::NOT_FOUND {
                    // Definitive: article doesn't exist
                    debug!(artist = %artist_name, "Wikipedia article not found (404)");
                    sqlx::query("UPDATE artists SET bio_status = 'not_found' WHERE id = ?")
                        .bind(artist_id)
                        .execute(pool)
                        .await?;
                } else {
                    // Transient (429, 500, etc.) — leave as pending to retry
                    warn!(artist = %artist_name, status = %status, "Wikipedia transient error, will retry");
                }
            }
            Err(e) => {
                // Network error — leave as pending to retry
                warn!(artist = %artist_name, error = %e, "Wikipedia fetch error, will retry");
            }
        }

        // Rate limit: Wikipedia asks for courtesy (1 req/sec)
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    info!(count = enriched_ids.len(), "Wikipedia bio enrichment complete");
    Ok(enriched_ids)
}

/// Extract a Wikidata entity ID (e.g. "Q47293799") from MusicBrainz URL relations.
fn extract_wikidata_id(relations: &[super::types::MBRelation]) -> Option<String> {
    for rel in relations {
        if let Some(url_res) = &rel.url {
            if let Some(pos) = url_res.resource.find("wikidata.org/wiki/") {
                let after = &url_res.resource[pos + "wikidata.org/wiki/".len()..];
                let id: String = after.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if id.starts_with('Q') && id.len() > 1 {
                    return Some(id);
                }
            }
        }
    }
    None
}

/// Resolve a Wikidata entity ID to an English Wikipedia article title via sitelinks.
async fn resolve_wikidata_to_wikipedia(
    client: &reqwest::Client,
    wikidata_id: &str,
) -> anyhow::Result<Option<String>> {
    let url = format!(
        "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=sitelinks&sitefilter=enwiki&format=json",
        wikidata_id
    );

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Wikidata returned status {}", resp.status());
    }

    #[derive(serde::Deserialize)]
    struct WdResponse {
        entities: std::collections::HashMap<String, WdEntity>,
    }
    #[derive(serde::Deserialize)]
    struct WdEntity {
        #[serde(default)]
        sitelinks: std::collections::HashMap<String, WdSitelink>,
    }
    #[derive(serde::Deserialize)]
    struct WdSitelink {
        title: String,
    }

    let body: WdResponse = resp.json().await?;
    let title = body.entities
        .get(wikidata_id)
        .and_then(|e| e.sitelinks.get("enwiki"))
        .map(|sl| sl.title.clone());

    Ok(title)
}

/// Update tracks in this album with ISRCs from MusicBrainz recordings.
/// Matches by disc_number + track_number position.
async fn enrich_track_isrcs(
    pool: &SqlitePool,
    album_id: &str,
    release: &super::types::MBReleaseDetail,
) {
    let mut updated = 0u32;

    for medium in &release.media {
        for track in &medium.tracks {
            let recording = match &track.recording {
                Some(rec) => rec,
                None => continue,
            };

            let isrc = recording.isrcs.first().map(|s| s.as_str());
            let recording_id = &recording.id;

            // Skip if no useful data to store
            if isrc.is_none() && recording_id.is_empty() {
                continue;
            }

            let result = sqlx::query(
                "UPDATE tracks SET isrc = COALESCE(isrc, ?), musicbrainz_recording_id = COALESCE(musicbrainz_recording_id, ?) \
                 WHERE album_id = ? AND disc_number = ? AND track_number = ?"
            )
            .bind(isrc)
            .bind(recording_id)
            .bind(album_id)
            .bind(medium.position as i32)
            .bind(track.position as i32)
            .execute(pool)
            .await;

            match result {
                Ok(r) if r.rows_affected() > 0 => updated += 1,
                Ok(_) => {}
                Err(e) => {
                    warn!(disc = medium.position, track = track.position, error = %e, "failed to update ISRC");
                }
            }
        }
    }

    if updated > 0 {
        debug!(count = updated, album_id = %album_id, "enriched tracks with ISRCs");
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}
