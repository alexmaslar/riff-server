use serde::Deserialize;
use tracing::debug;

pub struct LastfmTopTrack {
    pub name: String,
    pub rank: i32,
    pub playcount: i64,
    pub listeners: i64,
}

#[derive(Deserialize)]
struct TopTracksResponse {
    toptracks: Option<TopTracksWrapper>,
}

#[derive(Deserialize)]
struct TopTracksWrapper {
    track: Vec<TrackEntry>,
}

#[derive(Deserialize)]
struct TrackEntry {
    name: String,
    playcount: String,
    listeners: String,
    #[serde(rename = "@attr")]
    attr: TrackAttr,
}

#[derive(Deserialize)]
struct TrackAttr {
    rank: String,
}

pub async fn fetch_top_tracks(
    client: &reqwest::Client,
    api_key: &str,
    artist_name: &str,
    limit: u32,
) -> anyhow::Result<Vec<LastfmTopTrack>> {
    debug!("Last.fm request for '{}'", artist_name);

    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "artist.gettoptracks"),
            ("artist", artist_name),
            ("api_key", api_key),
            ("format", "json"),
            ("autocorrect", "1"),
        ])
        .query(&[("limit", limit)])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Last.fm API returned status {}", resp.status());
    }

    let body: TopTracksResponse = resp.json().await?;

    let tracks = match body.toptracks {
        Some(tt) => tt
            .track
            .into_iter()
            .map(|t| LastfmTopTrack {
                name: t.name,
                rank: t.attr.rank.parse().unwrap_or(0),
                playcount: t.playcount.parse().unwrap_or(0),
                listeners: t.listeners.parse().unwrap_or(0),
            })
            .collect(),
        None => Vec::new(),
    };

    Ok(tracks)
}
