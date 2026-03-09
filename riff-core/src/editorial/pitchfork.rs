use super::html::extract_json_ld;
use super::util::{clean_title, slugify, url_encode};
use crate::plugin::capabilities::{EditorialProvider, EditorialResult, EditorialReview};
use async_trait::async_trait;
use serde::Deserialize;

pub struct PitchforkProvider {
    http: reqwest::Client,
}

impl PitchforkProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

#[async_trait]
impl EditorialProvider for PitchforkProvider {
    fn provider_name(&self) -> &str {
        "pitchfork"
    }

    fn icon_url(&self) -> Option<&str> {
        None
    }

    async fn get_album_reviews(
        &self,
        title: &str,
        artist: &str,
        _year: Option<i32>,
    ) -> anyhow::Result<Option<EditorialResult>> {
        let review = fetch_review(&self.http, artist, title).await;
        match review {
            Some(r) => Ok(Some(EditorialResult { reviews: vec![r] })),
            None => Ok(None),
        }
    }
}

async fn fetch_review(
    http: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Option<EditorialReview> {
    let review_url = search_for_review(http, artist, title).await?;

    let resp = http.get(&review_url)
        .header("Accept", "text/html")
        .send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;
    parse_review_page(&review_url, &body)
}

async fn search_for_review(
    http: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Option<String> {
    let cleaned = clean_title(title);
    let title_slug = slugify(cleaned);

    let query = format!("{} {}", artist, cleaned);
    if let Some(url) = search_and_match(http, &query, &title_slug).await {
        return Some(url);
    }

    search_and_match(http, artist, &title_slug).await
}

async fn search_and_match(
    http: &reqwest::Client,
    query: &str,
    title_slug: &str,
) -> Option<String> {
    let encoded = url_encode(query);
    let search_url = format!("https://pitchfork.com/search/?q={}", encoded);

    let resp = http.get(&search_url)
        .header("Accept", "text/html")
        .send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;
    let urls = extract_review_urls(&html);

    urls.into_iter().find(|url| {
        if let Some(slug_part) = url.split("/reviews/albums/").nth(1) {
            let slug = slug_part.trim_end_matches('/');
            let slug = if let Some(pos) = slug.find('-') {
                if slug[..pos].chars().all(|c| c.is_ascii_digit()) {
                    &slug[pos + 1..]
                } else {
                    slug
                }
            } else {
                slug
            };
            slug.contains(title_slug)
        } else {
            false
        }
    })
}

fn extract_review_urls(html: &str) -> Vec<String> {
    let pattern = "href=\"/reviews/albums/";
    let mut urls = Vec::new();
    let mut search_from = 0;

    loop {
        let Some(pos) = html[search_from..].find(pattern) else { break };
        let abs_pos = search_from + pos;
        let path_start = abs_pos + "href=\"".len();
        let Some(end_offset) = html[path_start..].find('"') else { break };
        let path_end = path_start + end_offset;
        let path = &html[path_start..path_end];

        if path != "/reviews/albums/" && path.len() > "/reviews/albums/".len() {
            let full_url = format!("https://pitchfork.com{}", path);
            if !urls.contains(&full_url) {
                urls.push(full_url);
            }
        }

        search_from = path_end;
        if search_from >= html.len().saturating_sub(50) { break }
    }

    urls
}

#[derive(Deserialize)]
struct JsonLdReview {
    #[serde(rename = "reviewBody")]
    review_body: Option<String>,
    author: Option<serde_json::Value>,
    #[serde(rename = "datePublished")]
    date_published: Option<String>,
}

fn parse_review_page(url: &str, html: &str) -> Option<EditorialReview> {
    let rating = extract_rating_from_preloaded(html);

    let json_ld = extract_json_ld(html);
    let (excerpt, reviewer, review_date) = if let Some(ref ld_str) = json_ld {
        if let Ok(review) = serde_json::from_str::<JsonLdReview>(ld_str) {
            let excerpt = review.review_body;
            let reviewer = review.author.and_then(|a| match a {
                serde_json::Value::Array(arr) => arr
                    .first()
                    .and_then(|v| v.get("name"))
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string()),
                serde_json::Value::Object(obj) => {
                    obj.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
                }
                _ => None,
            });
            let review_date = review.date_published;
            (excerpt, reviewer, review_date)
        } else {
            (None, None, None)
        }
    } else {
        (None, None, None)
    };

    if rating.is_none() && excerpt.is_none() {
        return None;
    }

    Some(EditorialReview {
        source: "pitchfork".to_string(),
        source_url: url.to_string(),
        excerpt,
        rating,
        rating_count: None,
        reviewer,
        review_date,
    })
}

fn extract_rating_from_preloaded(html: &str) -> Option<f64> {
    let state_marker = "__PRELOADED_STATE__";
    let state_pos = html.find(state_marker)?;
    let state_region = &html[state_pos..];

    let pattern = "\"rating\":";
    let mut search_from = 0;

    while let Some(pos) = state_region[search_from..].find(pattern) {
        let abs_pos = search_from + pos;
        let value_start = abs_pos + pattern.len();

        if abs_pos > 0 {
            let before = state_region.as_bytes().get(abs_pos - 1).copied().unwrap_or(b'"');
            if before.is_ascii_alphabetic() {
                search_from = value_start;
                continue;
            }
        }

        let rest = &state_region[value_start..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let num_str = &rest[..end];

        if let Ok(val) = num_str.parse::<f64>() {
            if (0.0..=10.0).contains(&val) {
                return Some(val);
            }
        }

        search_from = value_start;
    }

    None
}
