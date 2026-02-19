pub mod client;
pub mod models;

use std::path::Path;

use async_trait::async_trait;

use super::capabilities::{
    StreamUrl, StreamingAlbum, StreamingAlbumDetail, StreamingArtist, StreamingProvider,
    StreamingQuality, StreamingSearchResults, StreamingTrack,
};
use super::{Capability, HealthStatus, Plugin, PluginContext};
use client::QobuzClient;
use models::{QobuzAlbum, QobuzArtist, QobuzTrack};

pub struct QobuzPlugin {
    client: Option<QobuzClient>,
    quality: String,
}

impl QobuzPlugin {
    pub fn new() -> Self {
        Self {
            client: None,
            quality: "6".to_string(),
        }
    }

    fn client(&self) -> anyhow::Result<&QobuzClient> {
        self.client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("qobuz plugin not initialized"))
    }
}

fn quality_to_qobuz(q: StreamingQuality) -> &'static str {
    match q {
        StreamingQuality::HiRes => "27",
        StreamingQuality::Lossless => "6",
        StreamingQuality::High => "5",
        StreamingQuality::Low => "5",
    }
}

fn convert_artist(a: &QobuzArtist) -> StreamingArtist {
    StreamingArtist {
        provider_id: a.id.map(|id| id.to_string()).unwrap_or_default(),
        name: a.name.0.clone(),
        image_url: a
            .image
            .as_ref()
            .and_then(|img| img.large.clone().or_else(|| img.small.clone())),
    }
}

fn convert_album(a: &QobuzAlbum) -> StreamingAlbum {
    let year = a.release_date_original.as_ref().and_then(|d| {
        d.split('-')
            .next()
            .and_then(|y| y.parse::<i32>().ok())
    });
    let artist = a
        .artist
        .as_ref()
        .map(convert_artist)
        .unwrap_or_else(|| StreamingArtist {
            provider_id: String::new(),
            name: "Unknown Artist".to_string(),
            image_url: None,
        });
    StreamingAlbum {
        provider_id: a.id.0.clone(),
        title: a.title.clone(),
        artist,
        year,
        cover_url: a
            .image
            .as_ref()
            .and_then(|img| img.large.clone().or_else(|| img.small.clone())),
        track_count: a.tracks_count.unwrap_or(0),
        available_qualities: vec![
            StreamingQuality::HiRes,
            StreamingQuality::Lossless,
            StreamingQuality::High,
        ],
    }
}

fn convert_track(t: &QobuzTrack) -> StreamingTrack {
    StreamingTrack {
        provider_id: t.id.to_string(),
        title: t.title.clone(),
        artist_name: t
            .performer
            .as_ref()
            .map(|p| p.name.0.clone())
            .unwrap_or_else(|| "Unknown Artist".to_string()),
        album_title: t
            .album
            .as_ref()
            .and_then(|a| a.title.clone())
            .unwrap_or_default(),
        track_number: t.track_number.unwrap_or(1),
        disc_number: 1,
        duration_secs: t.duration.unwrap_or(0),
        available_qualities: vec![
            StreamingQuality::HiRes,
            StreamingQuality::Lossless,
            StreamingQuality::High,
        ],
    }
}

#[async_trait]
impl Plugin for QobuzPlugin {
    fn name(&self) -> &str {
        "qobuz"
    }

    fn display_name(&self) -> &str {
        "Qobuz"
    }

    fn version(&self) -> &str {
        "1.0.0"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Streaming]
    }

    async fn init(&mut self, ctx: PluginContext) -> anyhow::Result<()> {
        let quality = ctx
            .config
            .get("quality")
            .and_then(|v| v.as_str())
            .unwrap_or("6")
            .to_string();
        let country = ctx
            .config
            .get("country")
            .and_then(|v| v.as_str())
            .unwrap_or("US")
            .to_string();

        self.quality = quality;
        self.client = Some(QobuzClient::new(&country));
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        match self.client() {
            Ok(client) => match client.search("test", 1).await {
                Ok(_) => HealthStatus {
                    healthy: true,
                    message: None,
                },
                Err(e) => HealthStatus {
                    healthy: false,
                    message: Some(format!("search failed: {e}")),
                },
            },
            Err(e) => HealthStatus {
                healthy: false,
                message: Some(e.to_string()),
            },
        }
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl StreamingProvider for QobuzPlugin {
    fn provider_name(&self) -> &str {
        "qobuz"
    }

    async fn search(&self, query: &str, limit: u32) -> anyhow::Result<StreamingSearchResults> {
        let client = self.client()?;
        let resp = client.search(query, limit).await?;

        Ok(StreamingSearchResults {
            albums: resp
                .albums
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_album)
                .collect(),
            tracks: resp
                .tracks
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_track)
                .collect(),
            artists: resp
                .artists
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_artist)
                .collect(),
        })
    }

    async fn get_album(&self, provider_album_id: &str) -> anyhow::Result<StreamingAlbumDetail> {
        let client = self.client()?;
        let album_data = client.get_album(provider_album_id).await?;

        let album = convert_album(&album_data);
        let tracks: Vec<StreamingTrack> = album_data
            .tracks
            .unwrap_or_default()
            .items
            .iter()
            .map(convert_track)
            .collect();

        Ok(StreamingAlbumDetail { album, tracks })
    }

    async fn get_artist_albums(
        &self,
        provider_artist_id: &str,
    ) -> anyhow::Result<Vec<StreamingAlbum>> {
        let client = self.client()?;
        let id: u64 = provider_artist_id.parse()?;
        let resp = client.get_artist(id).await?;
        Ok(resp
            .albums
            .unwrap_or_default()
            .items
            .iter()
            .map(convert_album)
            .collect())
    }

    async fn download_track(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
        dest: &Path,
    ) -> anyhow::Result<()> {
        let client = self.client()?;
        let id: u64 = provider_track_id.parse()?;
        let qobuz_quality = quality_to_qobuz(quality);
        client.download_track(id, qobuz_quality, dest).await
    }

    async fn get_stream_url(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
    ) -> anyhow::Result<StreamUrl> {
        let client = self.client()?;
        let id: u64 = provider_track_id.parse()?;
        let qobuz_quality = quality_to_qobuz(quality);
        let url = client.get_stream_url(id, qobuz_quality).await?;

        // Qobuz returns FLAC for quality 6/7/27, MP3 for quality 5
        let mime_type = if qobuz_quality == "5" {
            "audio/mpeg"
        } else {
            "audio/flac"
        };

        Ok(StreamUrl {
            url,
            mime_type: mime_type.to_string(),
            quality,
        })
    }
}
