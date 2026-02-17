use serde::Deserialize;
use tracing::debug;

#[derive(Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct SearchResult {
    cover_image: Option<String>,
}

/// Search Discogs for an artist and return their high-res image URL.
pub async fn fetch_artist_image(
    client: &reqwest::Client,
    artist_name: &str,
    token: &str,
) -> anyhow::Result<Option<String>> {
    debug!("Discogs: searching for artist '{}'", artist_name);

    let resp = client
        .get("https://api.discogs.com/database/search")
        .query(&[
            ("q", artist_name),
            ("type", "artist"),
            ("per_page", "1"),
            ("token", token),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs search returned status {}", resp.status());
    }

    let body: SearchResponse = resp.json().await?;
    let image_url = body
        .results
        .first()
        .and_then(|r| r.cover_image.clone())
        .filter(|url| !url.is_empty());

    Ok(image_url)
}
