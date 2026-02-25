use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// --- Streaming Provider ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamingQuality {
    Low,
    High,
    Lossless,
    HiRes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingArtist {
    pub provider_id: String,
    pub name: String,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingAlbum {
    pub provider_id: String,
    pub title: String,
    pub artist: StreamingArtist,
    pub year: Option<i32>,
    pub cover_url: Option<String>,
    pub track_count: u32,
    pub available_qualities: Vec<StreamingQuality>,
    pub album_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingTrack {
    pub provider_id: String,
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub track_number: u32,
    pub disc_number: u32,
    pub duration_secs: u32,
    pub available_qualities: Vec<StreamingQuality>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingAlbumDetail {
    pub album: StreamingAlbum,
    pub tracks: Vec<StreamingTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingSearchResults {
    pub albums: Vec<StreamingAlbum>,
    pub artists: Vec<StreamingArtist>,
    pub tracks: Vec<StreamingTrack>,
}

#[async_trait]
pub trait StreamingProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;

    async fn search(&self, query: &str, limit: u32) -> anyhow::Result<StreamingSearchResults>;

    async fn get_album(&self, provider_album_id: &str) -> anyhow::Result<StreamingAlbumDetail>;

    async fn get_artist_albums(
        &self,
        provider_artist_id: &str,
    ) -> anyhow::Result<Vec<StreamingAlbum>>;

    async fn download_track(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
        dest: &Path,
    ) -> anyhow::Result<()>;

    /// Resolve a stream URL for proxied playback (no local download).
    async fn get_stream_url(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
    ) -> anyhow::Result<StreamUrl> {
        let _ = (provider_track_id, quality);
        Err(anyhow::anyhow!("get_stream_url not implemented"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: String,
    pub mime_type: String,
    pub quality: StreamingQuality,
}

// --- Lyrics Provider ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricsResult {
    pub plain_text: String,
    pub synced_lines: Option<Vec<SyncedLine>>,
    pub source: String,
    pub source_url: Option<String>,
}

#[async_trait]
pub trait LyricsProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;

    async fn get_lyrics(
        &self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<Option<LyricsResult>>;
}

// --- Scrobble Provider ---

#[async_trait]
pub trait ScrobbleProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;

    async fn now_playing(
        &self,
        user_token: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: u32,
    ) -> anyhow::Result<()>;

    async fn scrobble(
        &self,
        user_token: &str,
        title: &str,
        artist: &str,
        album: &str,
        duration_secs: u32,
        played_at: i64,
    ) -> anyhow::Result<()>;
}

// --- Metadata Provider ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataEnrichment {
    pub genre: Option<Vec<String>>,
    pub style: Option<Vec<String>>,
    pub label: Option<String>,
    pub year: Option<i32>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
    pub extra: serde_json::Value,
}

#[async_trait]
pub trait MetadataProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;

    async fn enrich_album(
        &self,
        title: &str,
        artist: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Option<MetadataEnrichment>>;

    async fn enrich_artist(&self, name: &str) -> anyhow::Result<Option<MetadataEnrichment>>;
}

// --- Editorial Provider ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorialReview {
    pub source: String,
    pub source_url: String,
    pub excerpt: Option<String>,
    pub rating: Option<f64>,
    pub rating_count: Option<u32>,
    pub reviewer: Option<String>,
    pub review_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorialResult {
    pub reviews: Vec<EditorialReview>,
}

#[async_trait]
pub trait EditorialProvider: Send + Sync + 'static {
    fn provider_name(&self) -> &str;

    async fn get_album_reviews(
        &self,
        title: &str,
        artist: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Option<EditorialResult>>;
}
