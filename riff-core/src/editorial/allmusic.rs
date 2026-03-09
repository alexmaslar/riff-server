use super::html::strip_html_tags;
use super::util::{clean_title, slugify, url_encode};
use crate::plugin::capabilities::{EditorialProvider, EditorialResult, EditorialReview};
use async_trait::async_trait;
use serde::Deserialize;

pub struct AllMusicProvider {
    http: reqwest::Client,
}

impl AllMusicProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { http }
    }
}

#[async_trait]
impl EditorialProvider for AllMusicProvider {
    fn provider_name(&self) -> &str {
        "allmusic"
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
    let cleaned = clean_title(title);
    let album_url = search_for_album(http, artist, cleaned).await?;

    // Fetch album page for rating from JSON-LD
    let resp = http
        .get(&album_url)
        .header("Accept", "text/html")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;
    let mut review = parse_album_page(&album_url, &body, artist)?;

    // Fetch review text from the AJAX endpoint
    let review_url = format!("{}/reviewAjax", album_url);
    if let Ok(resp) = http
        .get(&review_url)
        .header("Accept", "text/html, */*; q=0.01")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Referer", &album_url)
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(html) = resp.text().await {
                let (excerpt, reviewer) = parse_review_ajax(&html);
                review.excerpt = excerpt;
                if reviewer.is_some() {
                    review.reviewer = reviewer;
                }
            }
        }
    }

    Some(review)
}

async fn search_for_album(
    http: &reqwest::Client,
    artist: &str,
    title: &str,
) -> Option<String> {
    let title_slug = slugify(title);
    let artist_slug = slugify(artist);

    let query = format!("{} {}", artist, title);
    if let Some(url) = search_and_match(http, &query, &title_slug, &artist_slug).await {
        return Some(url);
    }

    search_and_match(http, title, &title_slug, &artist_slug).await
}

async fn search_and_match(
    http: &reqwest::Client,
    query: &str,
    title_slug: &str,
    artist_slug: &str,
) -> Option<String> {
    let encoded = url_encode(query);
    let search_url = format!("https://www.allmusic.com/search/albums/{}", encoded);

    let resp = http
        .get(&search_url)
        .header("Accept", "text/html")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let html = resp.text().await.ok()?;
    find_best_album_match(&html, title_slug, artist_slug)
}

fn find_best_album_match(
    html: &str,
    title_slug: &str,
    artist_slug: &str,
) -> Option<String> {
    let album_links = extract_album_links(html);
    let mut first_exact = None;

    // Pass 1: Exact slug match + artist in context
    for (url, context) in &album_links {
        let url_slug = extract_slug_from_url(url);
        if slug_exact_match(&url_slug, title_slug) {
            let context_slug = slugify(context);
            if context_slug.contains(artist_slug) || artist_slug.is_empty() {
                return Some(url.clone());
            }
            if first_exact.is_none() {
                first_exact = Some(url.clone());
            }
        }
    }

    // Pass 2: Contains slug match + artist in context
    for (url, context) in &album_links {
        let url_slug = extract_slug_from_url(url);
        if slug_matches(&url_slug, title_slug) {
            let context_slug = slugify(context);
            if context_slug.contains(artist_slug) || artist_slug.is_empty() {
                return Some(url.clone());
            }
        }
    }

    // Pass 3: Exact slug match without artist context
    first_exact
}

fn slug_exact_match(url_slug: &str, title_slug: &str) -> bool {
    if url_slug == title_slug {
        return true;
    }
    let decoded = simple_url_decode(url_slug);
    let decoded_slug = slugify(&decoded);
    decoded_slug == title_slug
}

fn slug_matches(url_slug: &str, title_slug: &str) -> bool {
    if url_slug.contains(title_slug) && is_close_length(title_slug, url_slug) {
        return true;
    }
    let decoded = simple_url_decode(url_slug);
    let decoded_slug = slugify(&decoded);
    decoded_slug.contains(title_slug) && is_close_length(title_slug, &decoded_slug)
}

fn is_close_length(title_slug: &str, url_slug: &str) -> bool {
    if url_slug.is_empty() {
        return false;
    }
    (title_slug.len() as f64 / url_slug.len() as f64) >= 0.7
}

fn simple_url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                result.push(byte as char);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn extract_album_links(html: &str) -> Vec<(String, String)> {
    let pattern = "href=\"/album/";
    let mut results = Vec::new();
    let mut search_from = 0;

    loop {
        let Some(pos) = html[search_from..].find(pattern) else {
            break;
        };
        let abs_pos = search_from + pos;
        let path_start = abs_pos + "href=\"".len();
        let Some(end_offset) = html[path_start..].find('"') else {
            break;
        };
        let path_end = path_start + end_offset;
        let path = &html[path_start..path_end];

        if path.contains("-mw") {
            let full_url = format!("https://www.allmusic.com{}", path);
            let context_end = (path_end + 2000).min(html.len());
            let context = &html[path_end..context_end];
            if !results.iter().any(|(u, _): &(String, String)| u == &full_url) {
                results.push((full_url, context.to_string()));
            }
        }

        search_from = path_end;
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    results
}

fn extract_slug_from_url(url: &str) -> String {
    let path = url.split("/album/").nth(1).unwrap_or("");
    if let Some(mw_pos) = path.rfind("-mw") {
        path[..mw_pos].to_string()
    } else {
        path.to_string()
    }
}

#[derive(Deserialize)]
struct AlbumJsonLd {
    #[serde(rename = "aggregateRating")]
    aggregate_rating: Option<AggregateRating>,
    #[serde(rename = "byArtist")]
    by_artist: Option<Vec<ByArtist>>,
}

#[derive(Deserialize)]
struct ByArtist {
    name: Option<String>,
}

#[derive(Deserialize)]
struct AggregateRating {
    #[serde(rename = "ratingValue")]
    rating_value: Option<String>,
    #[serde(rename = "ratingCount")]
    rating_count: Option<u32>,
    #[serde(rename = "bestRating")]
    best_rating: Option<String>,
}

fn parse_review_ajax(html: &str) -> (Option<String>, Option<String>) {
    let reviewer = html.find("<h3>").and_then(|start| {
        let inner_start = start + 4;
        let inner_end = html[inner_start..].find("</h3>")? + inner_start;
        let h3_text = strip_html_tags(&html[inner_start..inner_end]);
        h3_text
            .find(" Review by ")
            .map(|pos| h3_text[pos + " Review by ".len()..].trim().to_string())
    });

    let excerpt = {
        let mut paragraphs = Vec::new();
        let mut search_from = 0;
        while let Some(p_pos) = html[search_from..].find("<p>") {
            let abs_start = search_from + p_pos + 3;
            if let Some(end_offset) = html[abs_start..].find("</p>") {
                let abs_end = abs_start + end_offset;
                let text = strip_html_tags(&html[abs_start..abs_end]);
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    paragraphs.push(trimmed.to_string());
                }
                search_from = abs_end + 4;
            } else {
                break;
            }
        }
        if paragraphs.is_empty() {
            None
        } else {
            Some(paragraphs.join("\n\n"))
        }
    };

    (excerpt, reviewer)
}

fn parse_album_page(url: &str, html: &str, artist: &str) -> Option<EditorialReview> {
    let json_ld = extract_album_json_ld(html)?;
    let album: AlbumJsonLd = serde_json::from_str(&json_ld).ok()?;

    // Verify artist from JSON-LD
    let artist_slug = slugify(artist);
    if !artist_slug.is_empty() {
        let artist_ok = album.by_artist.as_ref().map_or(false, |artists| {
            artists
                .iter()
                .any(|a| a.name.as_ref().map_or(false, |n| slugify(n).contains(&artist_slug)))
        });
        if !artist_ok {
            return None;
        }
    }

    let agg = album.aggregate_rating?;
    let rating_value: f64 = agg.rating_value.as_deref()?.parse().ok()?;
    let best: f64 = agg
        .best_rating
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10.0);

    let rating = if best > 0.0 {
        (rating_value / best) * 10.0
    } else {
        return None;
    };

    if !(0.0..=10.0).contains(&rating) {
        return None;
    }

    Some(EditorialReview {
        source: "allmusic".to_string(),
        source_url: url.to_string(),
        excerpt: None,
        rating: Some(rating),
        rating_count: agg.rating_count,
        reviewer: None,
        review_date: None,
    })
}

fn extract_album_json_ld(html: &str) -> Option<String> {
    let marker = "application/ld+json";
    let mut search_from = 0;

    loop {
        let tag_pos = html[search_from..].find(marker)?;
        let abs_pos = search_from + tag_pos;

        let content_start = html[abs_pos..].find('>')? + abs_pos + 1;
        let content_end = html[content_start..].find("</script>")? + content_start;
        let json_str = html[content_start..content_end].trim();

        if json_str.contains("\"MusicAlbum\"") {
            if json_str.starts_with('[') {
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(json_str) {
                    for item in &arr {
                        let s = item.to_string();
                        if s.contains("\"MusicAlbum\"") {
                            return Some(s);
                        }
                    }
                }
            }
            return Some(json_str.to_string());
        }

        search_from = content_end;
        if search_from >= html.len().saturating_sub(50) {
            break;
        }
    }

    None
}
