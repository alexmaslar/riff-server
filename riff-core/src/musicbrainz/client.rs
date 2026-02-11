use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::{InMemoryState, NotKeyed}};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

use super::types::*;

const BASE_URL: &str = "https://musicbrainz.org/ws/2";

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub struct MusicBrainzClient {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
}

impl MusicBrainzClient {
    pub fn new() -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("RiffServer/0.1 (riff-music-server)"),
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;

        // MusicBrainz allows 1 request/second
        let quota = Quota::per_second(NonZeroU32::new(1).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));

        Ok(Self { http, limiter })
    }

    pub async fn search_release(
        &self,
        artist: &str,
        title: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Vec<MBSearchResult>> {
        self.limiter.until_ready().await;

        let mut query = format!("release:\"{}\" AND artist:\"{}\"", escape_lucene(title), escape_lucene(artist));
        if let Some(y) = year {
            query.push_str(&format!(" AND date:{}", y));
        }

        let url = format!(
            "{}release/?query={}&fmt=json&limit=10",
            BASE_URL.trim_end_matches('/').to_owned() + "/",
            urlencoded(&query),
        );

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz API error {}: {}", status, body);
        }

        let search: MBSearchResponse = resp.json().await?;
        Ok(search.releases)
    }

    pub async fn get_release(&self, mbid: &str) -> anyhow::Result<MBReleaseDetail> {
        self.limiter.until_ready().await;

        let url = format!(
            "{}/release/{}?inc=artist-credits+labels+genres+tags+annotation+artist-rels&fmt=json",
            BASE_URL, mbid
        );

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz release detail error {}: {}", status, body);
        }

        Ok(resp.json().await?)
    }

    /// Download cover art from Cover Art Archive. Returns None on 404 (no cover).
    pub async fn download_cover(&self, mbid: &str) -> anyhow::Result<Option<Vec<u8>>> {
        // CAA is a separate service with no rate limit, so no limiter wait
        let url = format!("https://coverartarchive.org/release/{}/front", mbid);
        let resp = self.http.get(&url).send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            anyhow::bail!("CAA download error {}: {}", resp.status(), url);
        }

        Ok(Some(resp.bytes().await?.to_vec()))
    }
}

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            c if c.is_alphanumeric() || "-_.~\"".contains(c) => c.to_string(),
            c => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Escape Lucene special characters for MusicBrainz search queries.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "+-&|!(){}[]^~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
