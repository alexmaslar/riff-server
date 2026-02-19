pub mod client;
pub mod models;

use std::path::Path;

use async_trait::async_trait;

use super::capabilities::{
    StreamUrl, StreamingAlbum, StreamingAlbumDetail, StreamingArtist, StreamingProvider,
    StreamingQuality, StreamingSearchResults, StreamingTrack,
};
use super::{Capability, HealthStatus, Plugin, PluginContext};
use client::TidalClient;
use models::{TidalAlbum, TidalArtist, TidalTrack, tidal_cover_url};

pub struct TidalPlugin {
    client: Option<TidalClient>,
    quality: String,
}

impl TidalPlugin {
    pub fn new() -> Self {
        Self {
            client: None,
            quality: "LOSSLESS".to_string(),
        }
    }

    fn client(&self) -> anyhow::Result<&TidalClient> {
        self.client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tidal plugin not initialized"))
    }
}

fn quality_to_tidal(q: StreamingQuality) -> &'static str {
    match q {
        StreamingQuality::HiRes => "HI_RES_LOSSLESS",
        StreamingQuality::Lossless => "LOSSLESS",
        StreamingQuality::High => "HIGH",
        StreamingQuality::Low => "LOW",
    }
}

fn convert_artist(a: &TidalArtist) -> StreamingArtist {
    StreamingArtist {
        provider_id: a.id.to_string(),
        name: a.name.clone(),
        image_url: a.picture.as_ref().map(|p| tidal_cover_url(p, 480)),
    }
}

fn convert_album(a: &TidalAlbum) -> StreamingAlbum {
    let year = a.release_date.as_ref().and_then(|d| {
        d.split('-')
            .next()
            .and_then(|y| y.parse::<i32>().ok())
    });
    StreamingAlbum {
        provider_id: a.id.to_string(),
        title: a.title.clone(),
        artist: convert_artist(&a.primary_artist()),
        year,
        cover_url: a.cover.as_ref().map(|c| tidal_cover_url(c, 640)),
        track_count: a.number_of_tracks.unwrap_or(0),
        available_qualities: vec![
            StreamingQuality::HiRes,
            StreamingQuality::Lossless,
            StreamingQuality::High,
            StreamingQuality::Low,
        ],
    }
}

fn convert_track(t: &TidalTrack) -> StreamingTrack {
    StreamingTrack {
        provider_id: t.id.to_string(),
        title: t.title.clone(),
        artist_name: t.artist.name.clone(),
        album_title: t
            .album
            .as_ref()
            .map(|a| a.title.clone())
            .unwrap_or_default(),
        track_number: t.track_number,
        disc_number: t.volume_number.unwrap_or(1),
        duration_secs: t.duration,
        available_qualities: vec![
            StreamingQuality::HiRes,
            StreamingQuality::Lossless,
            StreamingQuality::High,
            StreamingQuality::Low,
        ],
    }
}

#[async_trait]
impl Plugin for TidalPlugin {
    fn name(&self) -> &str {
        "tidal"
    }

    fn display_name(&self) -> &str {
        "Tidal"
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
            .unwrap_or("LOSSLESS")
            .to_string();
        let timeout: u64 = ctx
            .config
            .get("instance_timeout")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        self.quality = quality;
        let client = TidalClient::new(timeout);
        client.load_instances().await;
        self.client = Some(client);
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        match self.client() {
            Ok(client) => match client.search_tracks("test", 1).await {
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
impl StreamingProvider for TidalPlugin {
    fn provider_name(&self) -> &str {
        "tidal"
    }

    async fn search(&self, query: &str, limit: u32) -> anyhow::Result<StreamingSearchResults> {
        let client = self.client()?;

        let (tracks_res, albums_res, artists_res) = tokio::join!(
            client.search_tracks(query, limit),
            client.search_albums(query, limit),
            client.search_artists(query, limit),
        );

        Ok(StreamingSearchResults {
            tracks: tracks_res
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_track)
                .collect(),
            albums: albums_res
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_album)
                .collect(),
            artists: artists_res
                .unwrap_or_default()
                .items
                .iter()
                .map(convert_artist)
                .collect(),
        })
    }

    async fn get_album(&self, provider_album_id: &str) -> anyhow::Result<StreamingAlbumDetail> {
        let client = self.client()?;
        let id: u64 = provider_album_id.parse()?;
        let resp = client.get_album(id).await?;

        let year = resp.release_date.as_ref().and_then(|d| {
            d.split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
        });

        let album = StreamingAlbum {
            provider_id: resp.id.to_string(),
            title: resp.title.clone(),
            artist: convert_artist(&resp.artist),
            year,
            cover_url: resp.cover.as_ref().map(|c| tidal_cover_url(c, 640)),
            track_count: resp.number_of_tracks.unwrap_or(resp.items.len() as u32),
            available_qualities: vec![
                StreamingQuality::HiRes,
                StreamingQuality::Lossless,
                StreamingQuality::High,
                StreamingQuality::Low,
            ],
        };

        let tracks: Vec<StreamingTrack> =
            resp.items.iter().map(|item| convert_track(&item.item)).collect();

        Ok(StreamingAlbumDetail { album, tracks })
    }

    async fn get_artist_albums(
        &self,
        provider_artist_id: &str,
    ) -> anyhow::Result<Vec<StreamingAlbum>> {
        let client = self.client()?;
        let id: u64 = provider_artist_id.parse()?;
        // Artist endpoint doesn't include albums, so fetch artist name then search albums
        let artist = client.get_artist(id).await?;
        let albums = client.search_albums(&artist.artist.name, 50).await?;
        // Filter to albums by this specific artist
        let artist_id = id;
        Ok(albums
            .items
            .iter()
            .filter(|a| {
                a.primary_artist().id == artist_id
                    || a.artists.iter().any(|ar| ar.id == artist_id)
            })
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
        let tidal_quality = quality_to_tidal(quality);
        client.download_track(id, tidal_quality, dest).await
    }

    async fn get_stream_url(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
    ) -> anyhow::Result<StreamUrl> {
        let client = self.client()?;
        let id: u64 = provider_track_id.parse()?;
        let tidal_quality = quality_to_tidal(quality);
        let (url, mime_type) = client.get_stream_url(id, tidal_quality).await?;
        Ok(StreamUrl {
            url,
            mime_type,
            quality,
        })
    }
}

// Default impl for TidalPage to handle search error gracefully
impl<T> Default for models::TidalPage<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            total_number_of_items: 0,
        }
    }
}
