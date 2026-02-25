use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SummaryResponse {
    extract: Option<String>,
}

/// Fetch a short Wikipedia page summary by title (for music-relevance validation).
async fn get_page_summary(
    client: &reqwest::Client,
    title: &str,
) -> anyhow::Result<Option<String>> {
    let encoded = urlencoding::encode(title);
    let url = format!(
        "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
        encoded
    );

    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !resp.status().is_success() {
        anyhow::bail!("Wikipedia summary returned status {}", resp.status());
    }

    let body: SummaryResponse = resp.json().await?;
    Ok(body.extract.filter(|s| !s.is_empty()))
}

/// Fetch a full plain-text extract from Wikipedia using the MediaWiki API.
async fn get_full_extract(
    client: &reqwest::Client,
    title: &str,
) -> anyhow::Result<Option<String>> {
    let resp = client
        .get("https://en.wikipedia.org/w/api.php")
        .query(&[
            ("action", "query"),
            ("titles", title),
            ("prop", "extracts"),
            ("explaintext", "true"),
            ("exsectionformat", "wiki"),
            ("format", "json"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await?;
    let pages = &body["query"]["pages"];

    if let Some(obj) = pages.as_object() {
        for (page_id, page) in obj {
            if page_id == "-1" {
                return Ok(None);
            }
            if let Some(extract) = page["extract"].as_str() {
                let cleaned = clean_extract(extract);
                if !cleaned.is_empty() {
                    return Ok(Some(cleaned));
                }
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// MusicBrainz → Wikidata → Wikipedia resolution
// ---------------------------------------------------------------------------

/// Resolve a MusicBrainz entity to its Wikipedia title via URL rels → Wikidata.
/// `entity_type` is "artist" or "release-group".
pub async fn resolve_title_from_mbid(
    client: &reqwest::Client,
    entity_type: &str,
    mbid: &str,
) -> anyhow::Result<Option<String>> {
    let url = format!(
        "https://musicbrainz.org/ws/2/{}/{}?inc=url-rels&fmt=json",
        entity_type, mbid
    );

    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await?;
    let relations = body["relations"].as_array();

    let mut wikipedia_url: Option<String> = None;
    let mut wikidata_id: Option<String> = None;

    if let Some(rels) = relations {
        for rel in rels {
            let resource = rel["url"]["resource"].as_str().unwrap_or_default();
            // Direct Wikipedia link — extract title
            if resource.contains("en.wikipedia.org/wiki/") {
                if let Some(title) = resource.split("/wiki/").nth(1) {
                    let decoded = urlencoding::decode(title).unwrap_or_default().to_string();
                    return Ok(Some(decoded));
                }
            }
            // Store for fallback
            if resource.contains("wikipedia.org") {
                wikipedia_url = Some(resource.to_string());
            }
            if resource.contains("wikidata.org") {
                if let Some(qid) = resource.rsplit('/').next() {
                    if qid.starts_with('Q') {
                        wikidata_id = Some(qid.to_string());
                    }
                }
            }
        }
    }

    // Try Wikidata → English Wikipedia title
    if let Some(qid) = wikidata_id {
        if let Ok(Some(title)) = resolve_wikidata_to_enwiki(client, &qid).await {
            return Ok(Some(title));
        }
    }

    // Non-English Wikipedia URL — can't extract a usable title easily
    if wikipedia_url.is_some() {
        tracing::debug!("found non-English Wikipedia link for {} {}, skipping", entity_type, mbid);
    }

    Ok(None)
}

/// Resolve a Wikidata Q-ID to the English Wikipedia article title.
async fn resolve_wikidata_to_enwiki(
    client: &reqwest::Client,
    qid: &str,
) -> anyhow::Result<Option<String>> {
    let resp = client
        .get("https://www.wikidata.org/w/api.php")
        .query(&[
            ("action", "wbgetentities"),
            ("ids", qid),
            ("props", "sitelinks"),
            ("sitelfilter", "enwiki"),
            ("format", "json"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await?;
    let title = body["entities"][qid]["sitelinks"]["enwiki"]["title"].as_str();
    Ok(title.map(|t| t.to_string()))
}

// ---------------------------------------------------------------------------
// Public search functions
// ---------------------------------------------------------------------------

/// Search Wikipedia for an album page and return its full extract.
/// If `resolved_wp_title` is provided (from MusicBrainz → Wikidata), use it directly.
/// Otherwise falls back to name-based disambiguation search.
pub async fn search_album(
    client: &reqwest::Client,
    title: &str,
    artist_name: &str,
    resolved_wp_title: Option<&str>,
) -> anyhow::Result<Option<String>> {
    // Strategy 0: Pre-resolved Wikipedia title from MusicBrainz (most reliable)
    if let Some(wp_title) = resolved_wp_title {
        if let Ok(Some(text)) = get_full_extract(client, wp_title).await {
            return Ok(Some(text));
        }
    }

    // Strategy 1: "{Title} ({artist} album)"
    let specific = format!("{} ({} album)", title, artist_name);
    if let Ok(Some(text)) = resolve_and_fetch(client, &specific).await {
        return Ok(Some(text));
    }

    // Strategy 2: "{Title} (album)"
    let album_query = format!("{} (album)", title);
    if let Ok(Some(text)) = resolve_and_fetch(client, &album_query).await {
        return Ok(Some(text));
    }

    // Strategy 3: Search API
    let search_query = format!("{} {} album", artist_name, title);
    if let Some(text) = search_and_validate(client, &search_query).await? {
        return Ok(Some(text));
    }

    Ok(None)
}

/// Search Wikipedia for an artist page and return its full extract.
/// If `resolved_wp_title` is provided (from MusicBrainz → Wikidata), use it directly.
/// Otherwise falls back to name-based disambiguation search.
pub async fn search_artist(
    client: &reqwest::Client,
    artist_name: &str,
    resolved_wp_title: Option<&str>,
) -> anyhow::Result<Option<String>> {
    // Strategy 0: Pre-resolved Wikipedia title from MusicBrainz (most reliable)
    if let Some(wp_title) = resolved_wp_title {
        if let Ok(Some(text)) = get_full_extract(client, wp_title).await {
            return Ok(Some(text));
        }
    }

    // Strategy 1: Direct lookup
    if let Ok(Some(text)) = resolve_and_fetch(client, artist_name).await {
        return Ok(Some(text));
    }

    // Strategy 2: "{Name} (band)" — single most-likely disambiguation suffix
    let query = format!("{} (band)", artist_name);
    if let Ok(Some(text)) = resolve_and_fetch(client, &query).await {
        return Ok(Some(text));
    }

    // Strategy 3: Search API
    let search_query = format!("{} musician OR band OR singer", artist_name);
    if let Some(text) = search_and_validate(client, &search_query).await? {
        return Ok(Some(text));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate via short summary, then fetch full extract.
async fn resolve_and_fetch(
    client: &reqwest::Client,
    title: &str,
) -> anyhow::Result<Option<String>> {
    let summary = get_page_summary(client, title).await?;
    match &summary {
        Some(s) if is_music_related(s) => {}
        _ => return Ok(None),
    }

    if let Some(full) = get_full_extract(client, title).await? {
        Ok(Some(full))
    } else {
        Ok(summary)
    }
}

/// Search Wikipedia and validate the top result is music-related.
async fn search_and_validate(
    client: &reqwest::Client,
    query: &str,
) -> anyhow::Result<Option<String>> {
    let resp = client
        .get("https://en.wikipedia.org/w/api.php")
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", query),
            ("srlimit", "1"),
            ("format", "json"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await?;
    let results = body["query"]["search"].as_array();

    if let Some(results) = results {
        for item in results {
            if let Some(t) = item["title"].as_str() {
                if let Ok(Some(text)) = resolve_and_fetch(client, t).await {
                    return Ok(Some(text));
                }
            }
        }
    }

    Ok(None)
}

/// Basic heuristic: does this summary look like it's about music?
fn is_music_related(text: &str) -> bool {
    let lower = text.to_lowercase();
    let music_terms = [
        "album", "studio album", "record", "song", "track", "band", "musician",
        "singer", "rapper", "hip hop", "rock", "pop", "jazz", "electronic",
        "punk", "metal", "folk", "r&b", "soul", "classical", "released",
        "debut", "recorded", "label", "genre", "music", "vocalist", "guitarist",
        "drummer", "producer", "remix", "ep ", "lp ", "single",
    ];
    music_terms.iter().any(|term| lower.contains(term))
}

// ---------------------------------------------------------------------------
// Extract cleaning
// ---------------------------------------------------------------------------

/// Sections to stop at — everything from here down is metadata, not prose.
const STOP_SECTIONS: &[&str] = &[
    "references", "external links", "see also", "charts", "certifications",
    "track listing", "tracklist", "personnel", "credits", "discography",
    "awards", "footnotes", "notes", "bibliography", "further reading",
    "selected discography", "band members", "members",
];

/// Clean up a Wikipedia plain-text extract.
fn clean_extract(text: &str) -> String {
    let mut result = String::new();
    let mut prev_was_newline = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Check for section headers — stop if it's a non-prose section
        if trimmed.starts_with("==") {
            let header = trimmed
                .trim_start_matches('=')
                .trim_end_matches('=')
                .trim()
                .to_lowercase();
            if STOP_SECTIONS.iter().any(|s| header == *s) {
                break;
            }
            // Skip the header line itself but continue reading content
            continue;
        }

        // Skip empty lines (collapse multiple)
        if trimmed.is_empty() {
            if !prev_was_newline && !result.is_empty() {
                result.push_str("\n\n");
                prev_was_newline = true;
            }
            continue;
        }

        if prev_was_newline || result.is_empty() {
            result.push_str(trimmed);
        } else {
            result.push(' ');
            result.push_str(trimmed);
        }
        prev_was_newline = false;
    }

    let trimmed = result.trim();

    // Cap at ~5000 chars, breaking at a paragraph boundary.
    // Use floor_char_boundary to avoid panicking on multi-byte UTF-8 chars.
    if trimmed.len() > 5000 {
        let boundary = trimmed.floor_char_boundary(5000);
        if let Some(pos) = trimmed[..boundary].rfind("\n\n") {
            return trimmed[..pos].to_string();
        }
        if let Some(pos) = trimmed[..boundary].rfind(". ") {
            return trimmed[..=pos].to_string();
        }
    }

    trimmed.to_string()
}
