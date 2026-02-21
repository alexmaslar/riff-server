use serde::Deserialize;

// --- Search responses ---

#[derive(Debug, Deserialize)]
pub struct TidalResponse<T> {
    pub data: T,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TidalPage<T> {
    pub items: Vec<T>,
    pub total_number_of_items: u32,
}

#[derive(Debug, Deserialize)]
pub struct TidalNestedSearch {
    pub albums: TidalPage<TidalAlbum>,
    pub artists: TidalPage<TidalArtist>,
}

// --- Core types ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TidalAlbum {
    pub id: u64,
    pub title: String,
    pub cover: Option<String>,
    pub number_of_tracks: Option<u32>,
    pub release_date: Option<String>,
    pub duration: Option<u32>,
    pub explicit: Option<bool>,
    #[serde(rename = "type", default)]
    pub album_type: Option<String>,
    /// Present in album detail responses but NOT in search results.
    /// Search results only have `artists` (array). Use `primary_artist()` helper.
    #[serde(default)]
    pub artist: Option<TidalArtist>,
    #[serde(default)]
    pub artists: Vec<TidalArtist>,
}

impl TidalAlbum {
    /// Get the primary artist: prefers `artist` field, falls back to first in `artists`.
    pub fn primary_artist(&self) -> TidalArtist {
        self.artist
            .clone()
            .or_else(|| self.artists.first().cloned())
            .unwrap_or(TidalArtist {
                id: 0,
                name: "Unknown".to_string(),
                picture: None,
            })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TidalArtist {
    pub id: u64,
    pub name: String,
    pub picture: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TidalTrack {
    pub id: u64,
    pub title: String,
    pub duration: u32,
    pub track_number: u32,
    pub volume_number: Option<u32>,
    pub explicit: Option<bool>,
    pub isrc: Option<String>,
    pub artist: TidalArtist,
    #[serde(default)]
    pub artists: Vec<TidalArtist>,
    pub album: Option<TidalAlbumRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TidalAlbumRef {
    pub id: u64,
    pub title: String,
    pub cover: Option<String>,
}

// --- Album detail response ---

#[derive(Debug, Deserialize)]
pub struct TidalAlbumResponse {
    pub id: u64,
    pub title: String,
    pub cover: Option<String>,
    #[serde(rename = "numberOfTracks")]
    pub number_of_tracks: Option<u32>,
    #[serde(rename = "releaseDate")]
    pub release_date: Option<String>,
    pub duration: Option<u32>,
    pub explicit: Option<bool>,
    pub artist: TidalArtist,
    #[serde(default)]
    pub artists: Vec<TidalArtist>,
    pub items: Vec<TidalAlbumItem>,
}

#[derive(Debug, Deserialize)]
pub struct TidalAlbumItem {
    pub item: TidalTrack,
}

// --- Artist response ---

#[derive(Debug, Deserialize)]
pub struct TidalArtistResponse {
    pub artist: TidalArtist,
}

// --- Stream URL ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TidalTrackDownload {
    pub track_id: Option<u64>,
    pub audio_quality: Option<String>,
    pub manifest: String,
    pub manifest_mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TidalManifest {
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub codecs: Option<String>,
    pub urls: Vec<String>,
}

/// Convert a Tidal cover UUID to a full image URL.
/// UUID format: `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` → replace `-` with `/`.
pub fn tidal_cover_url(uuid: &str, size: u32) -> String {
    let path = uuid.replace('-', "/");
    format!("https://resources.tidal.com/images/{path}/{size}x{size}.jpg")
}
