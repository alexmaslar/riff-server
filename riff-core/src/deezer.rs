use serde::Deserialize;
use tracing::debug;

pub struct DeezerTopTrack {
    pub name: String,
    pub rank: i32,
    pub popularity: i64,
}

#[derive(Deserialize)]
struct ArtistSearchResponse {
    data: Vec<ArtistResult>,
}

#[derive(Deserialize)]
struct ArtistResult {
    id: u64,
}

#[derive(Deserialize)]
struct TopTracksResponse {
    data: Vec<TrackResult>,
}

#[derive(Deserialize)]
struct TrackResult {
    title: String,
    rank: i64,
}

/// Fetch top tracks for an artist from Deezer.
pub async fn fetch_top_tracks(
    client: &reqwest::Client,
    artist_name: &str,
    limit: u32,
) -> anyhow::Result<Vec<DeezerTopTrack>> {
    debug!("Deezer: searching for artist '{}'", artist_name);

    let search_resp = client
        .get("https://api.deezer.com/search/artist")
        .query(&[("q", artist_name), ("limit", "1")])
        .send()
        .await?;

    if !search_resp.status().is_success() {
        anyhow::bail!("Deezer artist search returned status {}", search_resp.status());
    }

    let search_body: ArtistSearchResponse = search_resp.json().await?;
    let artist = match search_body.data.first() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };

    let artist_id = artist.id;

    debug!("Deezer: found artist id {} for '{}'", artist_id, artist_name);

    let top_resp = client
        .get(format!("https://api.deezer.com/artist/{}/top", artist_id))
        .query(&[("limit", limit)])
        .send()
        .await?;

    if !top_resp.status().is_success() {
        anyhow::bail!("Deezer top tracks returned status {}", top_resp.status());
    }

    let top_body: TopTracksResponse = top_resp.json().await?;

    let tracks = top_body
        .data
        .into_iter()
        .enumerate()
        .map(|(i, t)| DeezerTopTrack {
            name: t.title,
            rank: (i + 1) as i32,
            popularity: t.rank,
        })
        .collect();

    Ok(tracks)
}
