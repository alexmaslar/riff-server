use tracing::debug;

use super::types::MBSearchResult;

const THRESHOLD: f64 = 0.6;

pub struct MatchCandidate {
    pub result: MBSearchResult,
    pub score: f64,
}

/// Score and rank MusicBrainz search results against local album metadata.
/// Returns the best match above the threshold, or None.
pub fn best_match(
    results: Vec<MBSearchResult>,
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
    result: &MBSearchResult,
    local_artist: &str,
    local_title: &str,
    local_year: Option<i32>,
    local_track_count: Option<usize>,
) -> f64 {
    // MB provides artist and title as separate fields
    let mb_artist = result
        .artist_credit
        .first()
        .map(|ac| ac.artist.name.as_str())
        .unwrap_or("");

    let title_score = string_similarity(&normalize(local_title), &normalize(&result.title));
    let artist_score = artist_similarity(local_artist, mb_artist);

    let mb_year = result
        .date
        .as_ref()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<i32>().ok());

    let year_score = match (local_year, mb_year) {
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

    let track_score = match (local_track_count, result.track_count) {
        (Some(local), Some(mb)) if local > 0 && mb > 0 => {
            let diff = (local as i64 - mb as i64).unsigned_abs();
            match diff {
                0 => 1.0,
                1 => 0.8,
                2 => 0.5,
                _ => 0.0,
            }
        }
        _ => 0.5,
    };

    // Weights: title 0.40, artist 0.30, year 0.20, track count 0.10
    let total = title_score * 0.40 + artist_score * 0.30 + year_score * 0.20 + track_score * 0.10;

    debug!(
        "score '{}' by '{}': title={:.2} artist={:.2} year={:.2} tracks={:.2} total={:.2}",
        result.title, mb_artist, title_score, artist_score, year_score, track_score, total
    );

    total
}

/// Compare artist names, extracting primary artist from comma-separated credits.
fn artist_similarity(local: &str, mb: &str) -> f64 {
    let norm_local = normalize(local);
    let norm_mb = normalize(mb);

    // Full-string bigram comparison
    let full_score = string_similarity(&norm_local, &norm_mb);

    // Extract primary artist (first name before comma)
    let primary_mb = mb.split(',').next().unwrap_or(mb).trim();
    let primary_local = local.split(',').next().unwrap_or(local).trim();

    let primary_score = string_similarity(
        &normalize(primary_local),
        &normalize(primary_mb),
    );

    full_score.max(primary_score)
}

fn normalize(s: &str) -> String {
    let s = s.to_lowercase();
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
