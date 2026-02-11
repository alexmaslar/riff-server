use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub display_name: String,
    pub role: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artist {
    pub id: Uuid,
    pub name: String,
    pub external_id: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    pub id: Uuid,
    pub title: String,
    pub artist_id: Uuid,
    pub year: Option<i32>,
    pub external_id: Option<String>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub cover_art_path: Option<String>,
    pub ai_summary: Option<String>,
    pub metadata_status: String,
    pub added_at: NaiveDateTime,
    pub country: Option<String>,
    pub release_notes: Option<String>,
    pub all_labels: Vec<String>,
    pub is_compilation: bool,
    pub play_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: Uuid,
    pub album_id: Uuid,
    pub title: String,
    pub track_number: i32,
    pub disc_number: i32,
    pub duration_seconds: i32,
    pub file_path: String,
    pub format: String,
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub file_size_bytes: i64,
    pub composer: Option<String>,
    pub language: Option<String>,
    pub bpm_tag: Option<f64>,
    pub musical_key: Option<String>,
    pub mood: Option<String>,
    pub replay_gain_track_gain: Option<f64>,
    pub replay_gain_track_peak: Option<f64>,
    pub replay_gain_album_gain: Option<f64>,
    pub replay_gain_album_peak: Option<f64>,
    pub musicbrainz_recording_id: Option<String>,
    pub bpm_analyzed: Option<f64>,
    pub key_analyzed: Option<String>,
    pub loudness_lufs: Option<f64>,
    pub bliss_features: Option<String>,
    pub analysis_status: String,
    pub analyzed_at: Option<String>,
}
