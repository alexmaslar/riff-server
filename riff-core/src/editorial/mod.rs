use std::sync::Arc;

use crate::plugin::capabilities::EditorialProvider;
use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{info, warn};
use uuid::Uuid;

pub struct EditorialEnrichResult {
    pub reviews_added: u32,
    pub errors: Vec<String>,
}

/// Enrich all albums missing editorial content via editorial plugins.
pub async fn enrich_library_editorial(
    pool: &SqlitePool,
    editorial_providers: &[Arc<dyn EditorialProvider>],
) -> Result<EditorialEnrichResult> {
    let mut result = EditorialEnrichResult {
        reviews_added: 0,
        errors: Vec::new(),
    };

    if editorial_providers.is_empty() {
        return Ok(result);
    }

    let provider_count = editorial_providers.len() as i64;
    let batch_size = 100;
    let mut offset: i64 = 0;

    loop {
        // Find next batch of albums not yet fully enriched by all current providers.
        let albums: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT a.id, a.title, ar.name, a.release_date \
             FROM albums a JOIN artists ar ON a.artist_id = ar.id \
             WHERE a.metadata_status = 'matched' \
             AND a.id NOT IN ( \
                 SELECT entity_id FROM editorial_reviews \
                 WHERE entity_type = 'album' AND text != '' \
                 GROUP BY entity_id \
                 HAVING COUNT(DISTINCT source) >= ? \
             ) \
             ORDER BY a.id \
             LIMIT ? OFFSET ?",
        )
        .bind(provider_count)
        .bind(batch_size)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        if albums.is_empty() {
            break;
        }

        let batch_count = albums.len();
        info!(
            "editorial: enriching batch of {} albums (offset {})",
            batch_count, offset
        );

        for (album_id, title, artist_name, release_date) in &albums {
            match enrich_album(
                pool,
                editorial_providers,
                album_id,
                title,
                artist_name,
                release_date.as_deref(),
            )
            .await
            {
                Ok(added) => result.reviews_added += added,
                Err(e) => {
                    let msg = format!("{} - {}: {}", artist_name, title, e);
                    warn!("editorial album error: {}", msg);
                    result.errors.push(msg);
                }
            }
        }

        if batch_count < batch_size as usize {
            break;
        }
        offset += batch_size;
    }

    info!(
        "editorial enrichment complete: {} reviews added, {} errors",
        result.reviews_added,
        result.errors.len()
    );

    Ok(result)
}

/// Check if an album was released within the last 7 days.
/// Requires a full YYYY-MM-DD date; partial dates (YYYY or YYYY-MM) are treated as old.
fn is_recent_release(release_date: Option<&str>) -> bool {
    let date_str = match release_date {
        Some(d) if d.len() >= 10 => &d[..10],
        _ => return false,
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    let cutoff_str = cutoff.format("%Y-%m-%d").to_string();
    date_str >= cutoff_str.as_str()
}

/// Enrich a single album from editorial plugins. Returns number of reviews added.
async fn enrich_album(
    pool: &SqlitePool,
    editorial_providers: &[Arc<dyn EditorialProvider>],
    album_id: &str,
    title: &str,
    artist_name: &str,
    release_date: Option<&str>,
) -> Result<u32> {
    let recent = is_recent_release(release_date);
    let mut reviews_added = 0u32;

    // Strip parenthetical suffixes like "(Deluxe Edition)" so search queries
    // match the canonical album title across all editorial plugins.
    let clean_title = title
        .rfind('(')
        .filter(|&p| p > 0)
        .map(|p| title[..p].trim_end())
        .unwrap_or(title);

    for provider in editorial_providers {
        let source = provider.provider_name().to_string();

        // Check existing row for this source
        let existing: Option<(String, String)> = sqlx::query_as(
            "SELECT text, created_at FROM editorial_reviews \
             WHERE entity_type = 'album' AND entity_id = ? AND source = ?",
        )
        .bind(album_id)
        .bind(&source)
        .fetch_optional(pool)
        .await?;

        match &existing {
            Some((text, _)) if !text.is_empty() => {
                // Has review content → skip
                continue;
            }
            Some((_, _)) if !recent => {
                // Empty text, old album → skip (won't retry)
                continue;
            }
            Some((_, created_at)) => {
                // Empty text, recent album → retry only if last check was >24h ago
                let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string();
                if *created_at > cutoff {
                    continue;
                }
            }
            None => {
                // No row → query provider
            }
        }

        info!(
            "editorial: querying '{}' for '{}' by '{}'",
            source, clean_title, artist_name
        );

        let review_data = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            provider.get_album_reviews(clean_title, artist_name, None),
        )
        .await
        {
            Ok(Ok(Some(result))) if !result.reviews.is_empty() => {
                info!(
                    "editorial: '{}' found review for '{}' by '{}'",
                    source, clean_title, artist_name
                );
                result.reviews.into_iter().next()
            }
            Ok(Ok(_)) => {
                info!(
                    "editorial: '{}' no review found for '{}' by '{}'",
                    source, clean_title, artist_name
                );
                None
            }
            Ok(Err(e)) => {
                warn!(
                    "editorial: '{}' error for '{}' by '{}': {}",
                    source, clean_title, artist_name, e
                );
                None
            }
            Err(_) => {
                warn!(
                    "editorial: '{}' timed out for '{}' by '{}'",
                    source, clean_title, artist_name
                );
                None
            }
        };

        let review_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();

        match review_data {
            Some(review) => {
                let text = review.excerpt.unwrap_or_default();
                if text.is_empty() {
                    // Review with no excerpt → placeholder
                    insert_placeholder(pool, &review_id, album_id, &source, &now).await?;
                } else {
                    sqlx::query(
                        "INSERT OR REPLACE INTO editorial_reviews \
                         (id, entity_type, entity_id, source, source_url, text, rating, \
                          rating_count, reviewer, review_date, created_at) \
                         VALUES (?, 'album', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&review_id)
                    .bind(album_id)
                    .bind(&source)
                    .bind(&review.source_url)
                    .bind(&text)
                    .bind(review.rating)
                    .bind(review.rating_count.map(|c| c as i64))
                    .bind(&review.reviewer)
                    .bind(&review.review_date)
                    .bind(&now)
                    .execute(pool)
                    .await?;
                    reviews_added += 1;
                }
            }
            None => {
                // No review → placeholder
                insert_placeholder(pool, &review_id, album_id, &source, &now).await?;
            }
        }
    }

    Ok(reviews_added)
}

/// Insert a placeholder row (empty text) to mark a source as checked.
async fn insert_placeholder(
    pool: &SqlitePool,
    id: &str,
    album_id: &str,
    source: &str,
    now: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO editorial_reviews \
         (id, entity_type, entity_id, source, text, created_at) \
         VALUES (?, 'album', ?, ?, '', ?)",
    )
    .bind(id)
    .bind(album_id)
    .bind(source)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}
