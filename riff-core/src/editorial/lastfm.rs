use serde::Deserialize;

const BASE_URL: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Debug, Deserialize)]
struct AlbumInfoResponse {
    album: Option<AlbumInfo>,
}

#[derive(Debug, Deserialize)]
struct AlbumInfo {
    wiki: Option<Wiki>,
    tags: Option<Tags>,
}

#[derive(Debug, Deserialize)]
struct Wiki {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Tags {
    tag: Vec<Tag>,
}

#[derive(Debug, Deserialize)]
struct Tag {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ArtistInfoResponse {
    artist: Option<ArtistInfo>,
}

#[derive(Debug, Deserialize)]
struct ArtistInfo {
    bio: Option<Bio>,
    tags: Option<Tags>,
}

#[derive(Debug, Deserialize)]
struct Bio {
    content: Option<String>,
}

pub struct AlbumResult {
    pub summary: Option<String>,
    pub tags: Vec<String>,
}

pub struct ArtistResult {
    pub bio: Option<String>,
    pub tags: Vec<String>,
}

/// Fetch album info from Last.fm.
pub async fn get_album_info(
    client: &reqwest::Client,
    api_key: &str,
    artist: &str,
    album: &str,
) -> anyhow::Result<AlbumResult> {
    let resp = client
        .get(BASE_URL)
        .query(&[
            ("method", "album.getinfo"),
            ("artist", artist),
            ("album", album),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Last.fm album.getinfo returned status {}", resp.status());
    }

    let body: AlbumInfoResponse = resp.json().await?;

    let album_info = match body.album {
        Some(a) => a,
        None => return Ok(AlbumResult { summary: None, tags: Vec::new() }),
    };

    let summary = album_info
        .wiki
        .and_then(|w| w.content)
        .map(|s| clean_lastfm_html(&s))
        .filter(|s| !s.is_empty());

    let tags = album_info
        .tags
        .map(|t| t.tag.into_iter().map(|t| t.name.to_lowercase()).collect())
        .unwrap_or_default();

    Ok(AlbumResult { summary, tags })
}

/// Fetch artist info from Last.fm.
pub async fn get_artist_info(
    client: &reqwest::Client,
    api_key: &str,
    artist: &str,
) -> anyhow::Result<ArtistResult> {
    let resp = client
        .get(BASE_URL)
        .query(&[
            ("method", "artist.getinfo"),
            ("artist", artist),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Last.fm artist.getinfo returned status {}", resp.status());
    }

    let body: ArtistInfoResponse = resp.json().await?;

    let artist_info = match body.artist {
        Some(a) => a,
        None => return Ok(ArtistResult { bio: None, tags: Vec::new() }),
    };

    let bio = artist_info
        .bio
        .and_then(|b| b.content)
        .map(|s| clean_lastfm_html(&s))
        .filter(|s| !s.is_empty());

    let tags = artist_info
        .tags
        .map(|t| t.tag.into_iter().map(|t| t.name.to_lowercase()).collect())
        .unwrap_or_default();

    Ok(ArtistResult { bio, tags })
}

/// Strip HTML tags and the Last.fm attribution link from wiki content.
fn clean_lastfm_html(html: &str) -> String {
    // Remove <a href="...">Read more on Last.fm</a> suffix
    let cleaned = if let Some(idx) = html.find("<a href=") {
        &html[..idx]
    } else {
        html
    };
    // Strip remaining HTML tags
    let mut result = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for ch in cleaned.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result.trim().to_string()
}
