use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

use super::capabilities::{
    StreamUrl, StreamingAlbum, StreamingAlbumDetail, StreamingProvider, StreamingQuality,
    StreamingSearchResults,
};
use super::wasm_host::WasmPluginInstance;

pub struct WasmStreamingProvider {
    instance: Arc<WasmPluginInstance>,
    http_client: reqwest::Client,
}

#[derive(Serialize)]
struct SearchInput {
    query: String,
    limit: u32,
}

#[derive(Serialize)]
struct IdInput {
    id: String,
}

#[derive(Serialize)]
struct StreamUrlInput {
    id: String,
    quality: StreamingQuality,
}

impl WasmStreamingProvider {
    pub fn new(instance: Arc<WasmPluginInstance>, http_client: reqwest::Client) -> Self {
        Self {
            instance,
            http_client,
        }
    }
}

#[async_trait]
impl StreamingProvider for WasmStreamingProvider {
    fn provider_name(&self) -> &str {
        &self.instance.manifest.id
    }

    async fn search(&self, query: &str, limit: u32) -> anyhow::Result<StreamingSearchResults> {
        let input = serde_json::to_string(&SearchInput {
            query: query.to_string(),
            limit,
        })?;
        let mut plugin = self.instance.plugin.lock().await;
        let output = plugin.call::<&str, String>("riff_search", &input)?;
        let result: StreamingSearchResults = serde_json::from_str(&output)?;
        Ok(result)
    }

    async fn get_album(&self, provider_album_id: &str) -> anyhow::Result<StreamingAlbumDetail> {
        let input = serde_json::to_string(&IdInput {
            id: provider_album_id.to_string(),
        })?;
        let mut plugin = self.instance.plugin.lock().await;
        let output = plugin.call::<&str, String>("riff_get_album", &input)?;
        let result: StreamingAlbumDetail = serde_json::from_str(&output)?;
        Ok(result)
    }

    async fn get_artist_albums(
        &self,
        provider_artist_id: &str,
    ) -> anyhow::Result<Vec<StreamingAlbum>> {
        let input = serde_json::to_string(&IdInput {
            id: provider_artist_id.to_string(),
        })?;
        let mut plugin = self.instance.plugin.lock().await;
        let output = plugin.call::<&str, String>("riff_get_artist_albums", &input)?;
        let result: Vec<StreamingAlbum> = serde_json::from_str(&output)?;
        Ok(result)
    }

    async fn download_track(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
        dest: &std::path::Path,
    ) -> anyhow::Result<()> {
        let stream_url = self.get_stream_url(provider_track_id, quality).await?;

        let resp = self.http_client
            .get(&stream_url.url)
            .timeout(Duration::from_secs(300))
            .send()
            .await?
            .error_for_status()?;

        let bytes = resp.bytes().await?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }

    async fn get_stream_url(
        &self,
        provider_track_id: &str,
        quality: StreamingQuality,
    ) -> anyhow::Result<StreamUrl> {
        let input = serde_json::to_string(&StreamUrlInput {
            id: provider_track_id.to_string(),
            quality,
        })?;
        let mut plugin = self.instance.plugin.lock().await;
        let output = plugin.call::<&str, String>("riff_get_stream_url", &input)?;
        let result: StreamUrl = serde_json::from_str(&output)?;
        Ok(result)
    }
}
