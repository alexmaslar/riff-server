use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::types::*;

const BASE_URL: &str = "https://musicbrainz.org/ws/2";

pub struct MusicBrainzClient {
    http: reqwest::Client,
    last_request: Arc<Mutex<Instant>>,
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

        // Start in the past so the first request fires immediately
        let last_request = Arc::new(Mutex::new(Instant::now() - std::time::Duration::from_secs(2)));

        Ok(Self { http, last_request })
    }

    /// Wait until at least 1 second has passed since the last request.
    async fn rate_limit(&self) {
        let mut last = self.last_request.lock().await;
        let elapsed = last.elapsed();
        let interval = std::time::Duration::from_secs(1);
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
        *last = Instant::now();
    }

    pub async fn search_release(
        &self,
        artist: &str,
        title: &str,
        year: Option<i32>,
    ) -> anyhow::Result<Vec<MBSearchResult>> {
        self.rate_limit().await;

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
        self.rate_limit().await;

        let url = format!(
            "{}/release/{}?inc=artist-credits+labels+genres+tags+annotation+artist-rels+recordings+isrcs&fmt=json",
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

    pub async fn get_artist(&self, mbid: &str) -> anyhow::Result<MBArtistDetail> {
        self.rate_limit().await;

        let url = format!(
            "{}/artist/{}?inc=url-rels+genres&fmt=json",
            BASE_URL, mbid
        );

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz artist detail error {}: {}", status, body);
        }

        Ok(resp.json().await?)
    }

    pub async fn get_release_group(&self, mbid: &str) -> anyhow::Result<MBReleaseGroup> {
        self.rate_limit().await;

        let url = format!(
            "{}/release-group/{}?inc=genres&fmt=json",
            BASE_URL, mbid
        );

        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MusicBrainz release-group detail error {}: {}", status, body);
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
