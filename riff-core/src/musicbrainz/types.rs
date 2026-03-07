use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct MBSearchResponse {
    pub releases: Vec<MBSearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct MBSearchResult {
    pub id: String,
    pub score: u32,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    #[serde(default, rename = "artist-credit")]
    pub artist_credit: Vec<MBArtistCredit>,
    #[serde(default, rename = "label-info")]
    pub label_info: Vec<MBLabelInfo>,
    #[serde(default, rename = "release-group")]
    pub release_group: Option<MBReleaseGroup>,
    #[serde(default, rename = "track-count")]
    pub track_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct MBArtistCredit {
    pub artist: MBArtistRef,
    pub name: Option<String>,
    #[serde(default)]
    pub joinphrase: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MBArtistRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct MBReleaseDetail {
    pub id: String,
    pub title: String,
    pub date: Option<String>,
    pub country: Option<String>,
    #[serde(default, rename = "artist-credit")]
    pub artist_credit: Vec<MBArtistCredit>,
    #[serde(default, rename = "label-info")]
    pub label_info: Vec<MBLabelInfo>,
    #[serde(default)]
    pub genres: Vec<MBGenre>,
    #[serde(default)]
    pub tags: Vec<MBTag>,
    pub annotation: Option<String>,
    #[serde(default)]
    pub relations: Vec<MBRelation>,
    #[serde(default, rename = "release-group")]
    pub release_group: Option<MBReleaseGroup>,
    #[serde(default)]
    pub media: Vec<MBMedium>,
}

#[derive(Debug, Deserialize)]
pub struct MBGenre {
    pub name: String,
    #[serde(default)]
    pub count: i32,
}

#[derive(Debug, Deserialize)]
pub struct MBTag {
    pub name: String,
    #[serde(default)]
    pub count: i32,
}

#[derive(Debug, Deserialize)]
pub struct MBRelation {
    #[serde(rename = "type")]
    pub relation_type: String,
    pub artist: Option<MBArtistRef>,
    pub url: Option<MBUrlResource>,
}

#[derive(Debug, Deserialize)]
pub struct MBUrlResource {
    pub resource: String,
}

#[derive(Debug, Deserialize)]
pub struct MBArtistDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub genres: Vec<MBGenre>,
    #[serde(default)]
    pub relations: Vec<MBRelation>,
}

#[derive(Debug, Deserialize)]
pub struct MBLabelInfo {
    #[serde(rename = "catalog-number")]
    pub catalog_number: Option<String>,
    pub label: Option<MBLabel>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct MBLabel {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct MBReleaseGroup {
    pub id: String,
    #[serde(rename = "primary-type")]
    pub primary_type: Option<String>,
    #[serde(default, rename = "secondary-types")]
    pub secondary_types: Vec<String>,
    #[serde(default)]
    pub genres: Vec<MBGenre>,
}

// Media / Track / Recording types for ISRC enrichment

#[derive(Debug, Deserialize)]
pub struct MBMedium {
    pub position: u32,
    #[serde(default)]
    pub tracks: Vec<MBTrack>,
}

#[derive(Debug, Deserialize)]
pub struct MBTrack {
    pub position: u32,
    pub title: String,
    pub recording: Option<MBRecording>,
}

#[derive(Debug, Deserialize)]
pub struct MBRecording {
    pub id: String,
    #[serde(default)]
    pub isrcs: Vec<String>,
}

