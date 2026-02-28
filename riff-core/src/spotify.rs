use serde::Deserialize;
use tracing::debug;

#[derive(Deserialize)]
struct OEmbedResponse {
    thumbnail_url: Option<String>,
}

/// Fetch an artist image from Spotify's public oEmbed endpoint.
/// Takes a Spotify artist ID (22-char alphanumeric), returns a 640px image URL.
pub async fn fetch_artist_image(
    client: &reqwest::Client,
    spotify_id: &str,
) -> anyhow::Result<Option<String>> {
    let url = format!(
        "https://open.spotify.com/oembed?url=https://open.spotify.com/artist/{}",
        spotify_id
    );

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        debug!("Spotify oEmbed returned {} for {}", resp.status(), spotify_id);
        return Ok(None);
    }

    let body: OEmbedResponse = resp.json().await?;
    let Some(thumb) = body.thumbnail_url else {
        return Ok(None);
    };

    // Upgrade 320px thumbnail to 640px by swapping the size prefix
    let upgraded = thumb.replace("ab67616100005174", "ab6761610000e5eb");
    Ok(Some(upgraded))
}
