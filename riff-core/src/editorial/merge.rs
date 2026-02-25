use serde_json::json;
use std::collections::HashSet;

use super::{critiquebrainz, discogs, lastfm};

pub struct MergedAlbumEditorial {
    pub summary: Option<String>,
    pub summary_source: Option<String>,
    pub rating: Option<f64>,
    pub rating_sources: String,
    pub moods: Vec<String>,
    pub descriptors: Vec<String>,
    pub keywords: Vec<String>,
    pub reviews: Vec<MergedReview>,
}

pub struct MergedReview {
    pub source: String,
    pub text: String,
    pub rating: Option<f64>,
    pub rating_count: Option<u32>,
    pub license: Option<String>,
    pub source_updated_at: Option<String>,
}

pub struct MergedArtistEditorial {
    pub bio: Option<String>,
    pub bio_source: Option<String>,
}

/// Merge album editorial content from multiple sources.
pub fn merge_album(
    lastfm: Option<&lastfm::AlbumResult>,
    wikipedia: Option<&str>,
    critiquebrainz: Option<&critiquebrainz::CritiqueBrainzResult>,
    discogs: Option<&discogs::AlbumResult>,
) -> MergedAlbumEditorial {
    // Summary priority: Wikipedia > Last.fm > CritiqueBrainz top review > Discogs notes
    let (summary, summary_source) = if let Some(wp) = wikipedia {
        (Some(wp.to_string()), Some("wikipedia".to_string()))
    } else if let Some(lfm) = lastfm.and_then(|l| l.summary.as_deref()) {
        (Some(lfm.to_string()), Some("lastfm".to_string()))
    } else if let Some(cb) = critiquebrainz.and_then(|c| c.reviews.first()) {
        (Some(cb.text.clone()), Some("critiquebrainz".to_string()))
    } else if let Some(notes) = discogs.and_then(|d| d.notes.as_deref()) {
        (Some(notes.to_string()), Some("discogs".to_string()))
    } else {
        (None, None)
    };

    // Rating: normalize all to 0-10 scale, weighted average by count
    let mut rating_entries = Vec::new();
    if let Some(cb) = critiquebrainz {
        if let Some(avg) = cb.avg_rating {
            let normalized = avg * 2.0; // 1-5 → 2-10
            rating_entries.push(("critiquebrainz", normalized, cb.rating_count));
        }
    }
    if let Some(d) = discogs {
        if let Some(avg) = d.rating {
            let normalized = avg * 2.0; // 1-5 → 2-10
            rating_entries.push(("discogs", normalized, d.rating_count));
        }
    }

    let (rating, rating_sources) = if rating_entries.is_empty() {
        (None, "[]".to_string())
    } else {
        let total_weight: u32 = rating_entries.iter().map(|(_, _, c)| *c).sum();
        let weighted_sum: f64 = rating_entries
            .iter()
            .map(|(_, r, c)| r * *c as f64)
            .sum();
        let avg = if total_weight > 0 {
            weighted_sum / total_weight as f64
        } else {
            rating_entries.iter().map(|(_, r, _)| *r).sum::<f64>() / rating_entries.len() as f64
        };
        let sources: Vec<serde_json::Value> = rating_entries
            .iter()
            .map(|(source, rating, count)| {
                json!({
                    "source": source,
                    "rating": rating,
                    "count": count,
                })
            })
            .collect();
        (Some(avg), serde_json::to_string(&sources).unwrap_or_else(|_| "[]".to_string()))
    };

    // Tags: deduplicated union of Last.fm tags + Discogs styles
    let mut all_tags: HashSet<String> = HashSet::new();
    if let Some(lfm) = lastfm {
        all_tags.extend(lfm.tags.iter().cloned());
    }
    if let Some(d) = discogs {
        all_tags.extend(d.styles.iter().cloned());
    }
    let tags: Vec<String> = all_tags.into_iter().collect();

    // Split tags into moods/descriptors/keywords heuristically
    // For now, put all tags into keywords since we don't have a mood lexicon
    let keywords = tags;

    // Build reviews list
    let mut reviews = Vec::new();
    if let Some(cb) = critiquebrainz {
        for review in &cb.reviews {
            reviews.push(MergedReview {
                source: "critiquebrainz".to_string(),
                text: review.text.clone(),
                rating: review.rating.map(|r| r * 2.0),
                rating_count: None,
                license: review.license.clone(),
                source_updated_at: review.created.clone(),
            });
        }
    }

    MergedAlbumEditorial {
        summary,
        summary_source,
        rating,
        rating_sources,
        moods: Vec::new(),
        descriptors: Vec::new(),
        keywords,
        reviews,
    }
}

/// Merge artist bio from multiple sources.
/// Priority: Wikipedia > Last.fm > Discogs profile
pub fn merge_artist(
    wikipedia: Option<&str>,
    lastfm: Option<&lastfm::ArtistResult>,
    discogs: Option<&discogs::ArtistProfileResult>,
) -> MergedArtistEditorial {
    if let Some(wp) = wikipedia {
        MergedArtistEditorial {
            bio: Some(wp.to_string()),
            bio_source: Some("wikipedia".to_string()),
        }
    } else if let Some(lfm) = lastfm.and_then(|l| l.bio.as_deref()) {
        MergedArtistEditorial {
            bio: Some(lfm.to_string()),
            bio_source: Some("lastfm".to_string()),
        }
    } else if let Some(profile) = discogs.and_then(|d| d.profile.as_deref()) {
        MergedArtistEditorial {
            bio: Some(profile.to_string()),
            bio_source: Some("discogs".to_string()),
        }
    } else {
        MergedArtistEditorial {
            bio: None,
            bio_source: None,
        }
    }
}
