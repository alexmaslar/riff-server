use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::{InMemoryState, NotKeyed}};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use serde::de::DeserializeOwned;

use crate::config::DiscogsConfig;
use super::types::*;

const BASE_URL: &str = "https://api.discogs.com";

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub struct DiscogsClient {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
}

impl DiscogsClient {
    pub fn new(config: &DiscogsConfig) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("RiffServer/0.1"));

        if let Some(token) = &config.api_token {
            let val = HeaderValue::from_str(&format!("Discogs token={}", token))
                .map_err(|e| anyhow::anyhow!("invalid discogs token: {}", e))?;
            headers.insert(AUTHORIZATION, val);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()?;

        // Discogs allows 60 requests/minute for authenticated users → 1/sec
        let quota = Quota::per_second(NonZeroU32::new(1).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));

        Ok(Self { http, limiter })
    }

    async fn get<T: DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        self.limiter.until_ready().await;
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("discogs API error {}: {}", status, body);
        }
        Ok(resp.json::<T>().await?)
    }

    pub async fn search_release(
        &self,
        artist: &str,
        title: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        // Try specific artist + release_title search first
        let mut url = format!(
            "{}/database/search?type=release&artist={}&release_title={}",
            BASE_URL,
            urlencoded(artist),
            urlencoded(title),
        );
        if let Some(y) = year {
            url.push_str(&format!("&year={}", y));
        }
        let resp: SearchResponse = self.get(&url).await?;
        if !resp.results.is_empty() {
            return Ok(resp.results);
        }

        // Fallback: general q= query for fuzzy full-text matching.
        // Don't include year here — it acts as a hard filter on Discogs and
        // can exclude valid matches when the local year differs from Discogs.
        // Year comparison is handled in the scoring phase instead.
        let query = format!("{} {}", artist, title);
        let url = format!(
            "{}/database/search?type=release&q={}",
            BASE_URL,
            urlencoded(&query),
        );
        let resp: SearchResponse = self.get(&url).await?;
        Ok(resp.results)
    }

    pub async fn get_release(&self, id: u64) -> anyhow::Result<ReleaseDetail> {
        self.get(&format!("{}/releases/{}", BASE_URL, id)).await
    }

    pub async fn get_artist(&self, id: u64) -> anyhow::Result<ArtistDetail> {
        self.get(&format!("{}/artists/{}", BASE_URL, id)).await
    }

    pub async fn download_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        self.limiter.until_ready().await;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("failed to download {}: {}", url, resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            c if c.is_alphanumeric() || "-_.~".contains(c) => c.to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}
