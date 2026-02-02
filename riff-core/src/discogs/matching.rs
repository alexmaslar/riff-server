use tracing::debug;

use super::types::SearchResult;

const THRESHOLD: f64 = 0.6;

pub struct MatchCandidate {
    pub result: SearchResult,
    pub score: f64,
}

/// Score and rank Discogs search results against local album metadata.
/// Returns the best match above the threshold, or None.
pub fn best_match(
    results: Vec<SearchResult>,
    local_artist: &str,
    local_title: &str,
    local_year: Option<i32>,
    local_track_count: Option<usize>,
) -> Option<MatchCandidate> {
    let mut best: Option<MatchCandidate> = None;

    for result in results {
        let score = score_result(&result, local_artist, local_title, local_year, local_track_count);
        if score >= THRESHOLD {
            if best.as_ref().map_or(true, |b| score > b.score) {
                best = Some(MatchCandidate { result, score });
            }
        }
    }

    best
}

fn score_result(
    result: &SearchResult,
    local_artist: &str,
    local_title: &str,
    local_year: Option<i32>,
    _local_track_count: Option<usize>,
) -> f64 {
    // Discogs search titles are typically "Artist - Album Title"
    let (discogs_artist, discogs_title) = split_discogs_title(&result.title);

    let title_score = string_similarity(&normalize(local_title), &normalize(&discogs_title));
    let artist_score = artist_similarity(local_artist, &discogs_artist);

    let year_score = match (local_year, result.year.as_ref().and_then(|y| y.parse::<i32>().ok())) {
        (Some(ly), Some(dy)) => {
            let diff = (ly - dy).unsigned_abs();
            match diff {
                0 => 1.0,
                1 => 0.8,
                2 => 0.5,
                _ => 0.0,
            }
        }
        _ => 0.5, // no year data, neutral
    };

    // Weights: title 0.40, artist 0.30, year 0.20, track count 0.10 (track count not available from search)
    let total = title_score * 0.40 + artist_score * 0.30 + year_score * 0.20 + 0.5 * 0.10;

    debug!(
        "score '{}': title={:.2} artist={:.2} year={:.2} total={:.2}",
        result.title, title_score, artist_score, year_score, total
    );

    total
}

/// Compare artist names with Discogs-aware heuristics.
/// Extracts the primary artist from comma-separated Discogs credits,
/// strips disambiguation numbers like "(13)", and takes the better
/// of primary-vs-primary and full-string comparisons.
fn artist_similarity(local: &str, discogs: &str) -> f64 {
    let norm_local = normalize(local);
    let norm_discogs = normalize(discogs);

    // Full-string bigram comparison
    let full_score = string_similarity(&norm_local, &norm_discogs);

    // Extract primary artist (first name before comma) and strip disambiguation
    let primary_discogs = strip_discogs_disambiguation(
        discogs.split(',').next().unwrap_or(discogs).trim(),
    );
    let primary_local = local.split(',').next().unwrap_or(local).trim();

    let primary_score = string_similarity(
        &normalize(primary_local),
        &normalize(&primary_discogs),
    );

    full_score.max(primary_score)
}

/// Strip Discogs disambiguation numbers like "(13)" from artist names.
fn strip_discogs_disambiguation(name: &str) -> String {
    let trimmed = name.trim();
    if let Some(idx) = trimmed.rfind('(') {
        let after = &trimmed[idx + 1..];
        if after.ends_with(')') && after[..after.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            return trimmed[..idx].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn split_discogs_title(title: &str) -> (String, String) {
    if let Some(idx) = title.find(" - ") {
        let artist = title[..idx].to_string();
        let album = title[idx + 3..].to_string();
        (artist, album)
    } else {
        (String::new(), title.to_string())
    }
}

fn normalize(s: &str) -> String {
    let s = s.to_lowercase();
    // Strip common suffixes like "(Remastered)", "(Deluxe Edition)", etc.
    let s = s
        .replace("(remastered)", "")
        .replace("(deluxe edition)", "")
        .replace("(deluxe)", "")
        .replace("(expanded)", "")
        .replace("[remastered]", "")
        .replace("[deluxe edition]", "");
    s.trim().to_string()
}

fn string_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    // Simple bigram overlap (Dice coefficient)
    let bigrams_a: Vec<(char, char)> = a.chars().zip(a.chars().skip(1)).collect();
    let bigrams_b: Vec<(char, char)> = b.chars().zip(b.chars().skip(1)).collect();

    if bigrams_a.is_empty() || bigrams_b.is_empty() {
        return if a == b { 1.0 } else { 0.0 };
    }

    let matches = bigrams_a.iter().filter(|bg| bigrams_b.contains(bg)).count();
    (2.0 * matches as f64) / (bigrams_a.len() + bigrams_b.len()) as f64
}
