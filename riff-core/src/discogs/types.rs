use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult {
    pub id: u64,
    pub title: String,
    pub year: Option<String>,
    pub cover_image: Option<String>,
    #[serde(rename = "type")]
    pub result_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseDetail {
    pub id: u64,
    pub title: String,
    pub year: Option<u32>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub styles: Vec<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(default)]
    pub artists: Vec<DiscogsArtistRef>,
    #[serde(default)]
    pub tracklist: Vec<DiscogsTrack>,
    #[serde(default)]
    pub extraartists: Vec<DiscogsExtraArtist>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub formats: Vec<DiscogsFormat>,
}

#[derive(Debug, Deserialize)]
pub struct DiscogsFormat {
    pub name: String,
    #[serde(default)]
    pub descriptions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiscogsExtraArtist {
    pub id: u64,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct Label {
    pub name: String,
    pub catno: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Image {
    #[serde(rename = "type")]
    pub image_type: String,
    pub uri: String,
}

#[derive(Debug, Deserialize)]
pub struct DiscogsArtistRef {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct DiscogsTrack {
    pub position: String,
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtistDetail {
    pub id: u64,
    pub name: String,
    pub profile: Option<String>,
    #[serde(default)]
    pub images: Vec<Image>,
}
