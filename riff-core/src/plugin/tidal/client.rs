use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use tokio::sync::RwLock;

use super::models::*;

const DEFAULT_FALLBACK: &str = "https://tidal-api.binimum.org";
const INSTANCES_URL: &str = "https://monochrome.tf/instances.json";
const CLIENT_HEADER: &str = "BiniLossless/v3.4";

pub struct TidalClient {
    http: reqwest::Client,
    instances: RwLock<Vec<String>>,
    current_idx: AtomicUsize,
    timeout: Duration,
}

impl TidalClient {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default(),
            instances: RwLock::new(vec![DEFAULT_FALLBACK.to_string()]),
            current_idx: AtomicUsize::new(0),
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Fetch available API instances from monochrome.tf, falling back to the default.
    pub async fn load_instances(&self) {
        let result: anyhow::Result<Vec<String>> = async {
            let resp = self
                .http
                .get(INSTANCES_URL)
                .timeout(Duration::from_secs(10))
                .send()
                .await?;
            let json: serde_json::Value = resp.json().await?;
            let apis = json
                .as_object()
                .and_then(|o| o.get("api"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.trim_end_matches('/').to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if apis.is_empty() {
                bail!("empty instance list");
            }
            Ok(apis)
        }
        .await;

        match result {
            Ok(apis) => {
                tracing::info!("loaded {} tidal instances", apis.len());
                *self.instances.write().await = apis;
            }
            Err(e) => {
                tracing::warn!("failed to load tidal instances: {e}, using fallback");
                *self.instances.write().await = vec![DEFAULT_FALLBACK.to_string()];
            }
        }
    }

    /// Execute a GET request with round-robin failover across instances.
    async fn request_with_failover(&self, path: &str) -> anyhow::Result<reqwest::Response> {
        let instances = self.instances.read().await;
        let count = instances.len();
        if count == 0 {
            bail!("no tidal instances available");
        }
        let start = self.current_idx.load(Ordering::Relaxed) % count;

        for i in 0..count {
            let idx = (start + i) % count;
            let url = format!("{}{}", instances[idx], path);

            let result = self
                .http
                .get(&url)
                .header("x-client", CLIENT_HEADER)
                .timeout(self.timeout)
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 403 || status == 429 || status >= 500 {
                        tracing::debug!(
                            "tidal instance {} returned {}, trying next",
                            instances[idx],
                            status
                        );
                        self.current_idx.store((idx + 1) % count, Ordering::Relaxed);
                        continue;
                    }
                    // Success — advance index so next request starts here
                    self.current_idx.store(idx, Ordering::Relaxed);
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::debug!("tidal instance {} error: {e}, trying next", instances[idx]);
                    self.current_idx.store((idx + 1) % count, Ordering::Relaxed);
                    continue;
                }
            }
        }

        bail!("all tidal instances failed for path: {path}");
    }

    pub async fn search_tracks(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<TidalPage<TidalTrack>> {
        let path = format!("/search?s={}&limit={}", urlencoding::encode(query), limit);
        let resp = self.request_with_failover(&path).await?;
        let page: TidalResponse<TidalPage<TidalTrack>> =
            resp.json().await.context("parsing tidal track search")?;
        Ok(page.data)
    }

    pub async fn search_albums(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<TidalPage<TidalAlbum>> {
        let path = format!("/search?al={}&limit={}", urlencoding::encode(query), limit);
        let resp = self.request_with_failover(&path).await?;
        let nested: TidalResponse<TidalNestedSearch> =
            resp.json().await.context("parsing tidal album search")?;
        Ok(nested.data.albums)
    }

    pub async fn search_artists(
        &self,
        query: &str,
        limit: u32,
    ) -> anyhow::Result<TidalPage<TidalArtist>> {
        let path = format!("/search?a={}&limit={}", urlencoding::encode(query), limit);
        let resp = self.request_with_failover(&path).await?;
        let nested: TidalResponse<TidalNestedSearch> = resp
            .json()
            .await
            .context("parsing tidal artist search")?;
        Ok(nested.data.artists)
    }

    pub async fn get_album(&self, id: u64) -> anyhow::Result<TidalAlbumResponse> {
        let path = format!("/album/?id={}", id);
        let resp = self.request_with_failover(&path).await?;
        let wrapped: TidalResponse<TidalAlbumResponse> =
            resp.json().await.context("parsing tidal album")?;
        Ok(wrapped.data)
    }

    pub async fn get_artist(&self, id: u64) -> anyhow::Result<TidalArtistResponse> {
        let path = format!("/artist/?id={}", id);
        let resp = self.request_with_failover(&path).await?;
        let resp: TidalArtistResponse = resp.json().await.context("parsing tidal artist")?;
        Ok(resp)
    }

    /// Get a stream URL for a track. Decodes the base64 manifest.
    /// On DASH manifest (HI_RES_LOSSLESS), falls back to LOSSLESS quality.
    pub fn get_stream_url(
        &self,
        track_id: u64,
        quality: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<(String, String)>> + Send + '_>> {
        let quality = quality.to_string();
        Box::pin(async move {
            let path = format!("/track/?id={}&quality={}", track_id, quality);
            let resp = self.request_with_failover(&path).await?;
            let wrapped: TidalResponse<TidalTrackDownload> =
                resp.json().await.context("parsing tidal track download")?;
            let download = wrapped.data;

            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&download.manifest)
                .context("base64 decode manifest")?;
            let manifest_str = String::from_utf8(decoded).context("manifest utf8")?;

            // Try to parse as JSON manifest
            if let Ok(manifest) = serde_json::from_str::<TidalManifest>(&manifest_str) {
                if let Some(url) = manifest.urls.first() {
                    let mime = manifest
                        .mime_type
                        .unwrap_or_else(|| "audio/flac".to_string());
                    return Ok((url.clone(), mime));
                }
            }

            // If the manifest is a DASH XML (HI_RES_LOSSLESS), fallback to LOSSLESS
            if manifest_str.contains("</MPD>") || manifest_str.contains("dash") {
                if quality != "LOSSLESS" {
                    tracing::info!("tidal DASH manifest, falling back to LOSSLESS");
                    return self.get_stream_url(track_id, "LOSSLESS").await;
                }
                bail!("tidal returned DASH manifest even at LOSSLESS quality");
            }

            bail!("could not extract URL from tidal manifest");
        })
    }

    /// Download a track to a file.
    pub async fn download_track(
        &self,
        track_id: u64,
        quality: &str,
        dest: &std::path::Path,
    ) -> anyhow::Result<()> {
        let (url, _mime) = self.get_stream_url(track_id, quality).await?;

        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .context("downloading tidal track")?;

        let bytes = resp.bytes().await.context("reading tidal track bytes")?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }
}
