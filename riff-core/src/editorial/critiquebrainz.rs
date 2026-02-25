use serde::Deserialize;

const BASE_URL: &str = "https://critiquebrainz.org/ws/1";

#[derive(Debug, Deserialize)]
struct ReviewsResponse {
    reviews: Vec<Review>,
}

#[derive(Debug, Deserialize)]
struct Review {
    text: Option<String>,
    rating: Option<f64>,
    license: Option<License>,
    created: Option<String>,
}

#[derive(Debug, Deserialize)]
struct License {
    id: Option<String>,
}

pub struct CritiqueReview {
    pub text: String,
    pub rating: Option<f64>,
    pub license: Option<String>,
    pub created: Option<String>,
}

pub struct CritiqueBrainzResult {
    pub reviews: Vec<CritiqueReview>,
    /// Average rating (1-5 scale) across all reviews with ratings.
    pub avg_rating: Option<f64>,
    pub rating_count: u32,
}

/// Fetch reviews for a MusicBrainz release group.
pub async fn get_reviews(
    client: &reqwest::Client,
    release_group_mbid: &str,
) -> anyhow::Result<CritiqueBrainzResult> {
    let resp = client
        .get(format!("{}/review/", BASE_URL))
        .query(&[
            ("entity_id", release_group_mbid),
            ("entity_type", "release_group"),
            ("limit", "5"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("CritiqueBrainz returned status {}", resp.status());
    }

    let body: ReviewsResponse = resp.json().await?;

    let mut reviews = Vec::new();
    let mut rating_sum = 0.0;
    let mut rating_count = 0u32;

    for r in body.reviews {
        if let Some(ref text) = r.text {
            if !text.is_empty() {
                reviews.push(CritiqueReview {
                    text: text.clone(),
                    rating: r.rating,
                    license: r.license.and_then(|l| l.id),
                    created: r.created,
                });
            }
        }
        if let Some(rating) = r.rating {
            rating_sum += rating;
            rating_count += 1;
        }
    }

    let avg_rating = if rating_count > 0 {
        Some(rating_sum / rating_count as f64)
    } else {
        None
    };

    Ok(CritiqueBrainzResult {
        reviews,
        avg_rating,
        rating_count,
    })
}
