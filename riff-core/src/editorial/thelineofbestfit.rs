use super::html::strip_html_tags;
use super::util::{clean_title, slugify};
use crate::plugin::capabilities::{EditorialProvider, EditorialResult, EditorialReview};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::sync::Mutex;

const BASE_URL: &str = "https://www.thelineofbestfit.com";
const LISTING_URL: &str = "https://www.thelineofbestfit.com/albums";
const BATCH_SIZE: u32 = 25;
const MAX_PAGES: u32 = 348;

pub struct TheLineOfBestFitProvider {
    http: reqwest::Client,
    cache: Mutex<UrlCache>,
}

#[derive(Default)]
struct UrlCache {
    next_page: u32,
    slugs: Vec<String>,
}

impl TheLineOfBestFitProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            cache: Mutex::new(UrlCache::default()),
        }
    }
}

#[async_trait]
impl EditorialProvider for TheLineOfBestFitProvider {
    fn provider_name(&self) -> &str {
        "thelineofbestfit"
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
        let review = fetch_review(&self.http, &self.cache, artist, title).await;
        match review {
            Some(r) => Ok(Some(EditorialResult { reviews: vec![r] })),
            None => Ok(None),
        }
    }
}

async fn fetch_review(
    http: &reqwest::Client,
    cache: &Mutex<UrlCache>,
    artist: &str,
    title: &str,
) -> Option<EditorialReview> {
    let review_url = find_review_url(http, cache, artist, title).await?;

    let resp = http
        .get(&review_url)
        .header("Accept", "text/html")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;

    let mut review = parse_json_ld(&html, &review_url)?;
    if let Some(body_text) = extract_article_body(&html) {
        review.excerpt = Some(body_text);
    }
    Some(review)
}

async fn find_review_url(
    http: &reqwest::Client,
    cache: &Mutex<UrlCache>,
    artist: &str,
    title: &str,
) -> Option<String> {
    let cleaned = clean_title(title);
    let artist_slug = slugify(artist);
    let album_slug = slugify(cleaned);
    let prefix = format!("{}-{}", artist_slug, album_slug);

    if prefix.is_empty() {
        return None;
    }

    let mut cache = cache.lock().await;

    // Extend the cache if incomplete
    if cache.next_page < MAX_PAGES {
        fetch_next_batch(http, &mut cache).await;
    }

    match_url(&cache, &prefix)
}

fn match_url(cache: &UrlCache, prefix: &str) -> Option<String> {
    let prefix_with_dash = format!("{}-", prefix);
    for slug in &cache.slugs {
        if slug == prefix || slug.starts_with(&prefix_with_dash) {
            return Some(format!("{}/albums/{}", BASE_URL, slug));
        }
    }
    None
}

async fn fetch_next_batch(http: &reqwest::Client, cache: &mut UrlCache) {
    let start = cache.next_page + 1;
    let end = (start + BATCH_SIZE).min(MAX_PAGES + 1);

    for page in start..end {
        let url = format!("{}?page={}", LISTING_URL, page);

        let resp = match http
            .get(&url)
            .header("Accept", "text/html")
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        if !resp.status().is_success() {
            continue;
        }

        if let Ok(html) = resp.text().await {
            let new_slugs = extract_album_slugs(&html);
            for slug in new_slugs {
                if !cache.slugs.iter().any(|s| s == &slug) {
                    cache.slugs.push(slug);
                }
            }
        }

        cache.next_page = page;
    }
}

fn extract_album_slugs(html: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    let patterns: &[&str] = &[
        "href=\"/albums/",
        "href=\"https://www.thelineofbestfit.com/albums/",
    ];

    for pattern in patterns {
        let mut search_from = 0;
        while let Some(pos) = html[search_from..].find(pattern) {
            let abs_pos = search_from + pos;
            let slug_start = abs_pos + pattern.len();

            if let Some(end_offset) = html[slug_start..].find('"') {
                let slug = &html[slug_start..slug_start + end_offset];

                if !slug.is_empty() && !slug.contains('?') && !slug.contains('#') {
                    if seen.insert(slug.to_string()) {
                        results.push(slug.to_string());
                    }
                }

                search_from = slug_start + end_offset;
            } else {
                break;
            }
        }
    }

    results
}

fn extract_article_body(html: &str) -> Option<String> {
    let marker = "c--article-copy__sections";
    let marker_pos = html.find(marker)?;
    let content_start = html[marker_pos..].find('>')? + marker_pos + 1;

    // Walk nested divs to find the matching close
    let mut depth: u32 = 1;
    let mut pos = content_start;
    let content_end;

    loop {
        let next_open = html[pos..].find("<div");
        let next_close = html[pos..].find("</div>");

        let close_abs = match next_close {
            Some(c) => pos + c,
            None => return None,
        };

        if let Some(o) = next_open {
            let open_abs = pos + o;
            if open_abs < close_abs {
                depth += 1;
                pos = open_abs + 4;
                continue;
            }
        }

        depth -= 1;
        if depth == 0 {
            content_end = close_abs;
            break;
        }
        pos = close_abs + 6;
    }

    let raw = &html[content_start..content_end];

    let raw = raw
        .replace("</p>", "\n\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n");

    let text = strip_html_tags(&raw);

    let text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&ndash;", "\u{2013}")
        .replace("&mdash;", "\u{2014}");

    let paragraphs: Vec<String> = text
        .split("\n\n")
        .map(|p| {
            let mut collapsed = String::with_capacity(p.len());
            let mut prev_ws = false;
            for ch in p.chars() {
                if ch.is_whitespace() {
                    if !prev_ws {
                        collapsed.push(' ');
                    }
                    prev_ws = true;
                } else {
                    collapsed.push(ch);
                    prev_ws = false;
                }
            }
            collapsed.trim().to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();

    if paragraphs.is_empty() {
        return None;
    }

    let trimmed = paragraphs.join("\n\n");

    if trimmed.len() > 2000 {
        if let Some(pos) = trimmed[..2000].rfind(". ") {
            Some(trimmed[..=pos].to_string())
        } else {
            let mut s = trimmed[..2000].to_string();
            s.push_str("...");
            Some(s)
        }
    } else {
        Some(trimmed)
    }
}

#[derive(Deserialize)]
struct JsonLd {
    #[serde(rename = "@type")]
    type_name: Option<String>,
    review: Option<JsonLdReview>,
    #[serde(rename = "datePublished")]
    date_published: Option<String>,
}

#[derive(Deserialize)]
struct JsonLdReview {
    #[serde(rename = "reviewRating")]
    review_rating: Option<JsonLdRating>,
    author: Option<JsonLdAuthor>,
    #[serde(rename = "datePublished")]
    date_published: Option<String>,
    #[serde(rename = "reviewBody")]
    review_body: Option<String>,
}

#[derive(Deserialize)]
struct JsonLdRating {
    #[serde(rename = "ratingValue")]
    rating_value: Option<serde_json::Value>,
    #[serde(rename = "bestRating")]
    best_rating: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct JsonLdAuthor {
    name: Option<String>,
}

fn parse_json_ld(html: &str, review_url: &str) -> Option<EditorialReview> {
    let marker = "application/ld+json";
    let mut search_from = 0;

    loop {
        let tag_pos = match html[search_from..].find(marker) {
            Some(p) => p,
            None => break,
        };
        let abs_pos = search_from + tag_pos;

        let content_start = match html[abs_pos..].find('>') {
            Some(p) => abs_pos + p + 1,
            None => break,
        };
        let content_end = match html[content_start..].find("</script>") {
            Some(p) => content_start + p,
            None => break,
        };

        let json_str = html[content_start..content_end].trim();

        // Try single object
        if let Ok(ld) = serde_json::from_str::<JsonLd>(json_str) {
            if ld.type_name.as_deref() == Some("MusicAlbum") {
                if let Some(review) = extract_review_from_ld(&ld, review_url) {
                    return Some(review);
                }
            }
        }

        // Try array
        if let Ok(arr) = serde_json::from_str::<Vec<JsonLd>>(json_str) {
            for ld in &arr {
                if ld.type_name.as_deref() == Some("MusicAlbum") {
                    if let Some(review) = extract_review_from_ld(ld, review_url) {
                        return Some(review);
                    }
                }
            }
        }

        search_from = content_end;
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    None
}

fn extract_review_from_ld(ld: &JsonLd, review_url: &str) -> Option<EditorialReview> {
    let review = ld.review.as_ref()?;

    let rating = review.review_rating.as_ref().and_then(|r| {
        let value = parse_numeric_value(r.rating_value.as_ref()?)?;
        let best = r
            .best_rating
            .as_ref()
            .and_then(|b| parse_numeric_value(b))
            .unwrap_or(10.0);

        if best > 0.0 && best != 10.0 {
            Some((value / best) * 10.0)
        } else {
            Some(value)
        }
    });

    let reviewer = review.author.as_ref().and_then(|a| a.name.clone());

    let review_date = review
        .date_published
        .clone()
        .or_else(|| ld.date_published.clone());

    let excerpt = review.review_body.as_ref().map(|body| {
        let cleaned = clean_review_body(body);
        let trimmed = cleaned.trim();
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
    });

    if rating.is_none() && excerpt.is_none() {
        return None;
    }

    Some(EditorialReview {
        source: "thelineofbestfit".to_string(),
        source_url: review_url.to_string(),
        excerpt,
        rating,
        rating_count: None,
        reviewer,
        review_date,
    })
}

fn clean_review_body(body: &str) -> String {
    let mut s = body.to_string();

    if let Some(inner) = s.strip_prefix("<![CDATA[") {
        if let Some(inner) = inner.strip_suffix("]]>") {
            s = inner.to_string();
        }
    }

    s = s
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&ndash;", "\u{2013}")
        .replace("&mdash;", "\u{2014}")
        .replace("&amp;", "&");

    let text = strip_html_tags(&s);

    let mut collapsed = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
            }
            prev_ws = true;
        } else {
            collapsed.push(ch);
            prev_ws = false;
        }
    }

    collapsed.trim().to_string()
}

fn parse_numeric_value(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}
