use serde::Deserialize;

/// A single similar artist from the ListenBrainz Labs API.
#[derive(Debug, Deserialize)]
pub struct LBSimilarArtist {
    pub artist_mbid: String,
    #[serde(rename = "name")]
    pub artist_name: Option<String>,
    pub score: f64,
}

/// A single similar recording from the ListenBrainz Labs API.
#[derive(Debug, Deserialize)]
pub struct LBSimilarRecording {
    pub recording_mbid: String,
    #[serde(rename = "recording_name")]
    pub recording_name: Option<String>,
    #[serde(rename = "artist_credit_name")]
    pub artist_name: Option<String>,
    pub score: f64,
}
