use std::time::Duration;

use anyhow::Context;

use super::models::*;

const DEFAULT_BASE_URL: &str = "https://qobuz.squid.wtf";

pub struct QobuzClient {
    http: reqwest::Client,
    base_url: String,
    country: String,
}

impl QobuzClient {
    pub fn new(country: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base_url: DEFAULT_BASE_URL.to_string(),
            country: country.to_string(),
        }
    }

    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{}", self.base_url, path))
            .header("Token-Country", &self.country)
    }

    /// Single search call returns albums, tracks, and artists.
    pub async fn search(&self, query: &str, limit: u32) -> anyhow::Result<QobuzSearchResponse> {
        let resp = self
            .request(&format!(
                "/api/get-music?q={}&limit={}",
                urlencoding::encode(query),
                limit
            ))
            .send()
            .await
            .context("qobuz search request")?;
        let result: QobuzSearchResponse =
            resp.json().await.context("parsing qobuz search")?;
        Ok(result)
    }

    pub async fn get_album(&self, album_id: &str) -> anyhow::Result<QobuzAlbum> {
        let resp = self
            .request(&format!(
                "/api/get-album?album_id={}",
                urlencoding::encode(album_id)
            ))
            .send()
            .await
            .context("qobuz get album")?;
        let album: QobuzAlbum = resp.json().await.context("parsing qobuz album")?;
        Ok(album)
    }

    pub async fn get_artist(&self, artist_id: u64) -> anyhow::Result<QobuzArtistResponse> {
        let resp = self
            .request(&format!("/api/get-artist?artist_id={}", artist_id))
            .send()
            .await
            .context("qobuz get artist")?;
        let artist: QobuzArtistResponse = resp.json().await.context("parsing qobuz artist")?;
        Ok(artist)
    }

    /// Get a direct stream/download URL for a track.
    pub async fn get_stream_url(
        &self,
        track_id: u64,
        quality: &str,
    ) -> anyhow::Result<String> {
        let resp = self
            .request(&format!(
                "/api/download-music?track_id={}&quality={}",
                track_id, quality
            ))
            .send()
            .await
            .context("qobuz stream url")?;
        let download: QobuzDownloadResponse =
            resp.json().await.context("parsing qobuz download")?;
        download
            .url
            .ok_or_else(|| anyhow::anyhow!("no URL in qobuz download response"))
    }

    /// Download a track to a file.
    pub async fn download_track(
        &self,
        track_id: u64,
        quality: &str,
        dest: &std::path::Path,
    ) -> anyhow::Result<()> {
        let url = self.get_stream_url(track_id, quality).await?;

        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .context("downloading qobuz track")?;

        let bytes = resp.bytes().await.context("reading qobuz track bytes")?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }
}
