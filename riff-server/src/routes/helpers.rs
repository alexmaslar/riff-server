use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

/// Decode a JSON string array, logging a warning on parse failure.
pub fn decode_json_array(s: &str) -> Vec<String> {
    serde_json::from_str(s)
        .inspect_err(|e| tracing::warn!("failed to decode JSON array: {e}"))
        .unwrap_or_default()
}

/// Shared query parameter for streaming quality selection.
/// Used by tracks.rs, streaming.rs, and hls.rs.
#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub quality: Option<String>,
}

/// Convert a database row containing standard album fields to a JSON value.
///
/// Expects the row to have the following named columns:
/// id, title, artist_id, name (artist name), year, genre, style,
/// label, cover_art_path, added_at, play_count
pub fn album_row_to_json(row: &sqlx::sqlite::SqliteRow) -> Value {
    let genre_str: String = row.get("genre");
    let style_str: String = row.get("style");
    let genre = decode_json_array(&genre_str);
    let style = decode_json_array(&style_str);
    let source: Option<String> = row.try_get("source").ok().flatten();
    json!({
        "id": row.get::<String, _>("id"),
        "title": row.get::<String, _>("title"),
        "artist_id": row.get::<String, _>("artist_id"),
        "artist_name": row.get::<String, _>("name"),
        "year": row.get::<Option<i32>, _>("year"),
        "genre": genre,
        "style": style,
        "label": row.get::<Option<String>, _>("label"),
        "cover_art_path": row.get::<Option<String>, _>("cover_art_path"),
        "added_at": row.get::<String, _>("added_at"),
        "play_count": row.get::<i64, _>("play_count"),
        "source": source,
    })
}
