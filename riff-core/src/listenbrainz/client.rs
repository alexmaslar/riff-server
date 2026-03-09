use std::num::NonZeroU32;
use std::sync::Arc;

use governor::{Quota, RateLimiter, clock::DefaultClock, state::{InMemoryState, NotKeyed}};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde_json::json;

use super::types::*;

const LABS_BASE_URL: &str = "https://labs.api.listenbrainz.org";
const ARTIST_ALGORITHM: &str = "session_based_days_7500_session_300_contribution_3_threshold_10_limit_100_filter_True_skip_30";
const RECORDING_ALGORITHM: &str = "session_based_days_7500_session_300_contribution_5_threshold_15_limit_50_skip_30";

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub struct ListenBrainzLabsClient {
    http: reqwest::Client,
    limiter: Arc<Limiter>,
}

impl ListenBrainzLabsClient {
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

        // Conservative: 1 request/second
        let quota = Quota::per_second(NonZeroU32::new(1).unwrap());
        let limiter = Arc::new(RateLimiter::direct(quota));

        Ok(Self { http, limiter })
    }

    /// Fetch similar artists for a given artist MBID.
    /// Returns up to ~100 similar artists with scores.
    pub async fn similar_artists(&self, artist_mbid: &str) -> anyhow::Result<Vec<LBSimilarArtist>> {
        self.limiter.until_ready().await;

        let body = json!([{
            "artist_mbids": [artist_mbid],
            "algorithm": ARTIST_ALGORITHM
        }]);

        let resp = self.http
            .post(format!("{}/similar-artists/json", LABS_BASE_URL))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("ListenBrainz similar-artists error {}: {}", status, text);
        }

        // Response is a flat JSON array of objects
        let results: Vec<LBSimilarArtist> = resp.json().await?;
        Ok(results)
    }

    /// Fetch similar recordings for a given recording MBID.
    /// Returns up to ~100 similar recordings with scores.
    pub async fn similar_recordings(&self, recording_mbid: &str) -> anyhow::Result<Vec<LBSimilarRecording>> {
        self.limiter.until_ready().await;

        let body = json!([{
            "recording_mbids": [recording_mbid],
            "algorithm": RECORDING_ALGORITHM
        }]);

        let resp = self.http
            .post(format!("{}/similar-recordings/json", LABS_BASE_URL))
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("ListenBrainz similar-recordings error {}: {}", status, text);
        }

        // Response is a flat JSON array of objects
        let results: Vec<LBSimilarRecording> = resp.json().await?;
        Ok(results)
    }
}
