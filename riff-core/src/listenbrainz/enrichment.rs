use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::client::ListenBrainzLabsClient;

pub struct LBEnrichmentResult {
    pub artists_enriched: u32,
}

/// Eagerly enrich all artists that have an `external_id` (MusicBrainz artist MBID)
/// with similar artist data from ListenBrainz Labs.
/// Skips artists already enriched within the last 30 days.
pub async fn enrich_similar_artists(pool: &SqlitePool) -> Result<LBEnrichmentResult> {
    let client = ListenBrainzLabsClient::new()?;

    // Find artists with MBIDs that haven't been enriched recently
    let artists: Vec<(String, String)> = sqlx::query_as(
        "SELECT ar.id, ar.external_id
         FROM artists ar
         WHERE ar.external_id IS NOT NULL AND ar.external_id != ''
           AND ar.external_id NOT IN (
               SELECT entity_mbid FROM lb_enrichment_status
               WHERE entity_type = 'artist'
                 AND updated_at > datetime('now', '-30 days')
           )
         LIMIT 500",
    )
    .fetch_all(pool)
    .await?;

    if artists.is_empty() {
        info!("ListenBrainz: all artists up to date, skipping");
        return Ok(LBEnrichmentResult { artists_enriched: 0 });
    }

    info!(
        "ListenBrainz: enriching similar artists for {} artists",
        artists.len()
    );

    let mut enriched = 0u32;

    for (artist_id, artist_mbid) in &artists {
        match client.similar_artists(artist_mbid).await {
            Ok(similar) => {
                let count = similar.len();
                for sa in &similar {
                    let score = sa.score as i32;
                    if let Err(e) = sqlx::query(
                        "INSERT OR REPLACE INTO lb_similar_artists \
                         (artist_mbid, similar_artist_mbid, similar_artist_name, score, updated_at) \
                         VALUES (?, ?, ?, ?, datetime('now'))",
                    )
                    .bind(artist_mbid)
                    .bind(&sa.artist_mbid)
                    .bind(&sa.artist_name)
                    .bind(score)
                    .execute(pool)
                    .await
                    {
                        warn!(error = %e, "failed to insert LB similar artist");
                    }
                }

                // Mark as done
                let _ = sqlx::query(
                    "INSERT OR REPLACE INTO lb_enrichment_status \
                     (entity_type, entity_mbid, status, updated_at) \
                     VALUES ('artist', ?, 'done', datetime('now'))",
                )
                .bind(artist_mbid)
                .execute(pool)
                .await;

                if count > 0 {
                    enriched += 1;
                    tracing::debug!(
                        artist_id,
                        similar_count = count,
                        "LB similar artists cached"
                    );
                }
            }
            Err(e) => {
                warn!(artist_id, error = %e, "LB similar artists fetch failed");
                // Mark as failed so we don't retry immediately
                let _ = sqlx::query(
                    "INSERT OR REPLACE INTO lb_enrichment_status \
                     (entity_type, entity_mbid, status, updated_at) \
                     VALUES ('artist', ?, 'failed', datetime('now'))",
                )
                .bind(artist_mbid)
                .execute(pool)
                .await;
            }
        }
    }

    info!(
        "ListenBrainz similarity enrichment complete: {enriched} artists enriched"
    );

    Ok(LBEnrichmentResult { artists_enriched: enriched })
}

/// Lazily fetch and cache similar recordings for a given recording MBID.
/// Used on-demand during playlist generation for seed tracks.
/// Returns cached data if fresh (< 30 days), otherwise fetches from API.
pub async fn get_similar_recordings(
    pool: &SqlitePool,
    recording_mbid: &str,
) -> Result<Vec<(String, i32)>> {
    // Check cache first
    let cached: Vec<(String, i32)> = sqlx::query_as(
        "SELECT similar_recording_mbid, score FROM lb_similar_recordings
         WHERE recording_mbid = ?
           AND updated_at > datetime('now', '-30 days')
         ORDER BY score DESC",
    )
    .bind(recording_mbid)
    .fetch_all(pool)
    .await?;

    if !cached.is_empty() {
        return Ok(cached);
    }

    // Fetch from API
    let client = ListenBrainzLabsClient::new()?;
    let similar = client.similar_recordings(recording_mbid).await?;

    // Cache results
    for sr in &similar {
        let score = sr.score as i32;
        let _ = sqlx::query(
            "INSERT OR REPLACE INTO lb_similar_recordings \
             (recording_mbid, similar_recording_mbid, similar_recording_name, similar_artist_name, score, updated_at) \
             VALUES (?, ?, ?, ?, ?, datetime('now'))",
        )
        .bind(recording_mbid)
        .bind(&sr.recording_mbid)
        .bind(&sr.recording_name)
        .bind(&sr.artist_name)
        .bind(score)
        .execute(pool)
        .await;
    }

    // Mark as done
    let _ = sqlx::query(
        "INSERT OR REPLACE INTO lb_enrichment_status \
         (entity_type, entity_mbid, status, updated_at) \
         VALUES ('recording', ?, 'done', datetime('now'))",
    )
    .bind(recording_mbid)
    .execute(pool)
    .await;

    Ok(similar
        .iter()
        .map(|sr| (sr.recording_mbid.clone(), sr.score as i32))
        .collect())
}
