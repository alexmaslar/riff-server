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
    let artist_score = string_similarity(&normalize(local_artist), &normalize(&discogs_artist));

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
    title_score * 0.40 + artist_score * 0.30 + year_score * 0.20 + 0.5 * 0.10
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
