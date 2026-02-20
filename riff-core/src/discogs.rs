use std::collections::HashSet;

use serde::Deserialize;
use tracing::debug;

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    cover_image: Option<String>,
    #[serde(default)]
    genre: Vec<String>,
    #[serde(default)]
    style: Vec<String>,
}

/// Search Discogs for an album cover image by artist + title.
/// Returns the image bytes if found, or None.
pub async fn fetch_album_cover(
    client: &reqwest::Client,
    artist_name: &str,
    album_title: &str,
    token: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    debug!("Discogs: searching for album '{}' by '{}'", album_title, artist_name);

    let query = format!("{} {}", artist_name, album_title);
    let resp = client
        .get("https://api.discogs.com/database/search")
        .query(&[
            ("q", query.as_str()),
            ("type", "release"),
            ("per_page", "3"),
            ("token", token),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs search returned status {}", resp.status());
    }

    let body: SearchResponse = resp.json().await?;

    let cover_url = body
        .results
        .iter()
        .find_map(|r| r.cover_image.clone())
        .filter(|url| !url.is_empty());

    let Some(url) = cover_url else {
        return Ok(None);
    };

    // Download the image
    let img_resp = client
        .get(&url)
        .header("Authorization", format!("Discogs token={}", token))
        .send()
        .await?;

    if !img_resp.status().is_success() {
        return Ok(None);
    }

    let bytes = img_resp.bytes().await?.to_vec();
    if bytes.is_empty() {
        return Ok(None);
    }

    Ok(Some(bytes))
}

/// Fetch an artist image directly by Discogs artist ID (exact, no ambiguity).
pub async fn fetch_artist_image_by_id(
    client: &reqwest::Client,
    discogs_id: &str,
    token: &str,
) -> anyhow::Result<Option<String>> {
    debug!("Discogs: fetching by ID {} ", discogs_id);

    let resp = client
        .get(format!("https://api.discogs.com/artists/{}", discogs_id))
        .header("Authorization", format!("Discogs token={}", token))
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs artist/{} returned status {}", discogs_id, resp.status());
    }

    let body: ArtistResponse = resp.json().await?;

    let image_url = body
        .images
        .iter()
        .find(|img| img.image_type == "primary")
        .or_else(|| body.images.first())
        .map(|img| img.uri.clone());

    Ok(image_url)
}

#[derive(Deserialize)]
struct ArtistResponse {
    #[serde(default)]
    images: Vec<ArtistImage>,
}

#[derive(Deserialize)]
struct ArtistImage {
    #[serde(rename = "type")]
    image_type: String,
    uri: String,
}

/// Jaccard similarity between two sets: |A ∩ B| / |A ∪ B|.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    intersection as f64 / union as f64
}

/// Search Discogs for an artist and return their high-res image URL.
/// When `genre_hints` is non-empty, fetches multiple results and picks the one
/// whose genre/style tags best match the hints (Jaccard similarity).
pub async fn fetch_artist_image(
    client: &reqwest::Client,
    artist_name: &str,
    token: &str,
    genre_hints: &[String],
) -> anyhow::Result<Option<String>> {
    let has_hints = !genre_hints.is_empty();
    let per_page = if has_hints { "5" } else { "1" };

    debug!("Discogs: searching for artist '{}' (per_page={}, hints={})", artist_name, per_page, genre_hints.len());

    let resp = client
        .get("https://api.discogs.com/database/search")
        .query(&[
            ("q", artist_name),
            ("type", "artist"),
            ("per_page", per_page),
            ("token", token),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs search returned status {}", resp.status());
    }

    let body: SearchResponse = resp.json().await?;

    if body.results.is_empty() {
        return Ok(None);
    }

    // No hints — return first result (original behavior)
    if !has_hints {
        let image_url = body
            .results
            .first()
            .and_then(|r| r.cover_image.clone())
            .filter(|url| !url.is_empty());
        return Ok(image_url);
    }

    // Score each result by genre/style overlap with hints
    let hint_set: HashSet<String> = genre_hints.iter().map(|s| s.to_lowercase()).collect();

    let mut best_idx = 0;
    let mut best_score = -1.0_f64;

    for (i, result) in body.results.iter().enumerate() {
        let result_tags: HashSet<String> = result
            .genre
            .iter()
            .chain(result.style.iter())
            .map(|s| s.to_lowercase())
            .collect();
        let score = jaccard(&hint_set, &result_tags);
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }

    debug!(
        "Discogs: selected result #{} (score={:.3}) for '{}'",
        best_idx + 1,
        best_score,
        artist_name
    );

    let image_url = body.results[best_idx]
        .cover_image
        .clone()
        .filter(|url| !url.is_empty());

    Ok(image_url)
}
