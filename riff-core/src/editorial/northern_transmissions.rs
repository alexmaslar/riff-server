use super::html::strip_html_tags;
use super::util::{clean_title, slugify, url_encode};
use crate::plugin::capabilities::{EditorialProvider, EditorialResult, EditorialReview};
use async_trait::async_trait;
use serde::Deserialize;

pub struct NorthernTransmissionsProvider {
    http: reqwest::Client,
}

impl NorthernTransmissionsProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

#[async_trait]
impl EditorialProvider for NorthernTransmissionsProvider {
    fn provider_name(&self) -> &str {
        "northern-transmissions"
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

#[derive(Deserialize)]
struct WpPost {
    slug: String,
    link: String,
    date: Option<String>,
    content: Option<WpContent>,
}

#[derive(Deserialize)]
struct WpContent {
    rendered: Option<String>,
}

async fn fetch_review(
    http: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Option<EditorialReview> {
    let cleaned = clean_title(title);
    let (review_url, content_html, date) = search_for_review(http, artist, cleaned).await?;

    // Extract excerpt from REST API content
    let excerpt = content_html
        .as_ref()
        .map(|html| strip_html_tags(html))
        .map(|text| {
            let trimmed = text.trim();
            if trimmed.len() > 2000 {
                if let Some(pos) = trimmed[..2000].rfind(". ") {
                    trimmed[..=pos].to_string()
                } else {
                    let mut s = trimmed[..2000].to_string();
                    s.push_str("...");
                    s
                }
            } else {
                trimmed.to_string()
            }
        })
        .filter(|s| !s.is_empty());

    // Fetch the actual page HTML for rating and reviewer
    let resp = http
        .get(&review_url)
        .header("Accept", "text/html")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return Some(EditorialReview {
            source: "northern-transmissions".to_string(),
            source_url: review_url,
            excerpt,
            rating: None,
            rating_count: None,
            reviewer: None,
            review_date: date,
        });
    }

    let page_html = resp.text().await.ok()?;
    let rating = parse_rating(&page_html);
    let reviewer = parse_reviewer(&page_html);

    if rating.is_none() && excerpt.is_none() {
        return None;
    }

    Some(EditorialReview {
        source: "northern-transmissions".to_string(),
        source_url: review_url,
        excerpt,
        rating,
        rating_count: None,
        reviewer,
        review_date: date,
    })
}

async fn search_for_review(
    http: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let title_slug = slugify(title);
    let artist_slug = slugify(artist);

    let query = format!("{} {}", artist, title);
    if let Some(result) = search_and_match(http, &query, &title_slug, &artist_slug).await {
        return Some(result);
    }

    search_and_match(http, artist, &title_slug, &artist_slug).await
}

async fn search_and_match(
    http: &reqwest::Client,
    query: &str,
    title_slug: &str,
    artist_slug: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    let encoded = url_encode(query);
    let search_url = format!(
        "https://northerntransmissions.com/wp-json/wp/v2/posts?categories=15&search={}&per_page=5",
        encoded
    );

    let resp = http
        .get(&search_url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;
    let posts: Vec<WpPost> = serde_json::from_str(&body).ok()?;

    let mut best_match: Option<&WpPost> = None;
    let mut best_has_artist = false;

    for post in &posts {
        if !post.slug.contains(title_slug) {
            continue;
        }

        if !title_slug.is_empty() && !post.slug.is_empty() {
            let ratio = title_slug.len() as f64 / post.slug.len() as f64;
            if ratio < 0.3 {
                continue;
            }
        }

        let has_artist = !artist_slug.is_empty() && post.slug.contains(artist_slug);

        if has_artist && !best_has_artist {
            best_match = Some(post);
            best_has_artist = true;
        } else if best_match.is_none() {
            best_match = Some(post);
        }
    }

    best_match.map(|post| {
        let content_html = post.content.as_ref().and_then(|c| c.rendered.clone());
        (post.link.clone(), content_html, post.date.clone())
    })
}

fn parse_rating(html: &str) -> Option<f64> {
    if let Some(rating) = extract_rating_from_tags(html, "<h2 class=\"review\"", "</h2>") {
        return Some(rating);
    }
    if let Some(rating) = extract_rating_from_tags(html, "<h2", "</h2>") {
        return Some(rating);
    }
    extract_rating_from_tags(html, "<span", "</span>")
}

fn extract_rating_from_tags(html: &str, open_prefix: &str, close_tag: &str) -> Option<f64> {
    let mut search_from = 0;

    loop {
        let tag_pos = html[search_from..].find(open_prefix)?;
        let abs_tag_start = search_from + tag_pos;

        let Some(gt_offset) = html[abs_tag_start..].find('>') else {
            break;
        };
        let abs_start = abs_tag_start + gt_offset + 1;

        let Some(end_offset) = html[abs_start..].find(close_tag) else {
            break;
        };
        let abs_end = abs_start + end_offset;

        let inner = strip_html_tags(&html[abs_start..abs_end]);
        let text = inner.trim();

        if let Some(rating) = try_parse_rating(text) {
            return Some(rating);
        }

        search_from = abs_end + close_tag.len();
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    None
}

fn try_parse_rating(text: &str) -> Option<f64> {
    let text = text.strip_suffix("/10").unwrap_or(text).trim();
    if text.len() > 5 || text.is_empty() {
        return None;
    }
    let val: f64 = text.parse().ok()?;
    if (0.0..=10.0).contains(&val) {
        Some(val)
    } else {
        None
    }
}

fn parse_reviewer(html: &str) -> Option<String> {
    let marker = "Words by ";
    let pos = html.find(marker)?;
    let name_start = pos + marker.len();
    let rest = &html[name_start..];
    let end = rest.find(['<', '\n']).unwrap_or(rest.len());
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}
