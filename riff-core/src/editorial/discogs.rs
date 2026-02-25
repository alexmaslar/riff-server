use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    community: Option<Community>,
    notes: Option<String>,
    #[serde(default)]
    styles: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Community {
    rating: Option<CommunityRating>,
}

#[derive(Debug, Deserialize)]
struct CommunityRating {
    average: Option<f64>,
    count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ArtistResponse {
    profile: Option<String>,
}

pub struct AlbumResult {
    pub rating: Option<f64>,
    pub rating_count: u32,
    pub notes: Option<String>,
    pub styles: Vec<String>,
}

pub struct ArtistProfileResult {
    pub profile: Option<String>,
}

/// Fetch album rating and notes from Discogs by searching for a release.
pub async fn get_album_info(
    client: &reqwest::Client,
    token: &str,
    artist: &str,
    album: &str,
) -> anyhow::Result<AlbumResult> {
    let query = format!("{} {}", artist, album);
    let resp = client
        .get("https://api.discogs.com/database/search")
        .query(&[
            ("q", query.as_str()),
            ("type", "release"),
            ("per_page", "1"),
            ("token", token),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs search returned status {}", resp.status());
    }

    let body: SearchResponse = resp.json().await?;
    let release_id = match body.results.first().and_then(|r| r.id) {
        Some(id) => id,
        None => {
            return Ok(AlbumResult {
                rating: None,
                rating_count: 0,
                notes: None,
                styles: Vec::new(),
            })
        }
    };

    // Fetch the full release details
    let resp = client
        .get(format!("https://api.discogs.com/releases/{}", release_id))
        .header("Authorization", format!("Discogs token={}", token))
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Discogs release/{} returned status {}", release_id, resp.status());
    }

    let release: ReleaseResponse = resp.json().await?;

    let (rating, rating_count) = release
        .community
        .and_then(|c| c.rating)
        .map(|r| (r.average, r.count.unwrap_or(0)))
        .unwrap_or((None, 0));

    Ok(AlbumResult {
        rating,
        rating_count,
        notes: release.notes.filter(|s| !s.is_empty()),
        styles: release.styles.into_iter().map(|s| s.to_lowercase()).collect(),
    })
}

/// Fetch artist profile text from Discogs by searching for the artist.
pub async fn get_artist_profile(
    client: &reqwest::Client,
    token: &str,
    artist_name: &str,
) -> anyhow::Result<ArtistProfileResult> {
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
        anyhow::bail!("Discogs artist search returned status {}", resp.status());
    }

    let body: SearchResponse = resp.json().await?;
    let artist_id = match body.results.first().and_then(|r| r.id) {
        Some(id) => id,
        None => return Ok(ArtistProfileResult { profile: None }),
    };

    let resp = client
        .get(format!("https://api.discogs.com/artists/{}", artist_id))
        .header("Authorization", format!("Discogs token={}", token))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(ArtistProfileResult { profile: None });
    }

    let artist: ArtistResponse = resp.json().await?;
    Ok(ArtistProfileResult {
        profile: artist.profile.filter(|s| !s.is_empty()),
    })
}
