pub mod metadata;

use metadata::TrackMetadata;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

const SUPPORTED_EXTENSIONS: &[&str] = &["flac", "m4a", "wav", "aiff", "aif", "mp3", "ogg", "oga"];

pub struct ScanResult {
    pub artists_added: u32,
    pub albums_added: u32,
    pub tracks_added: u32,
    pub errors: Vec<String>,
}

pub async fn scan_library(pool: &SqlitePool, library_path: &str, library_id: &str) -> anyhow::Result<ScanResult> {
    let path = Path::new(library_path);
    if !path.exists() {
        anyhow::bail!("library path does not exist: {}", library_path);
    }

    let mut result = ScanResult {
        artists_added: 0,
        albums_added: 0,
        tracks_added: 0,
        errors: Vec::new(),
    };

    // Collect all audio files
    let mut audio_files = Vec::new();
    for entry in WalkDir::new(path).follow_links(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                result.errors.push(format!("walk error: {}", e));
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        // Skip macOS resource fork files (._filename)
        if entry.path().file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n.starts_with("._"))
        {
            continue;
        }

        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());

        if let Some(ext) = ext {
            if SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                audio_files.push(entry.into_path());
            }
        }
    }

    info!("found {} audio files to scan", audio_files.len());

    // Cache: artist name -> artist id, (artist_id, album_title) -> album_id
    let mut artist_cache: HashMap<String, Uuid> = HashMap::new();
    let mut album_cache: HashMap<(Uuid, String), Uuid> = HashMap::new();

    for file_path in &audio_files {
        let file_str = file_path.to_string_lossy().to_string();

        // Skip if track already exists
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tracks WHERE file_path = ?)")
            .bind(&file_str)
            .fetch_one(pool)
            .await?;

        if exists {
            continue;
        }

        // Extract metadata
        let meta = match metadata::extract_metadata(file_path) {
            Ok(mut m) => {
                // Fill in unknown fields from directory structure
                if m.artist == "Unknown Artist" || m.album == "Unknown Album" {
                    if let Some(path_meta) = metadata::metadata_from_path(file_path, path) {
                        if m.artist == "Unknown Artist" {
                            m.artist = path_meta.artist;
                        }
                        if m.album == "Unknown Album" {
                            m.album = path_meta.album;
                        }
                    }
                }
                m
            }
            Err(e) => {
                // Fallback to directory structure
                match metadata::metadata_from_path(file_path, path) {
                    Some(m) => {
                        warn!("tag extraction failed for {}, using path fallback: {}", file_str, e);
                        m
                    }
                    None => {
                        result.errors.push(format!("{}: {}", file_str, e));
                        continue;
                    }
                }
            }
        };

        // Upsert artist (normalize cache key for case-insensitive matching)
        let artist_key = meta.artist.trim().to_lowercase();
        let artist_id = if let Some(&id) = artist_cache.get(&artist_key) {
            id
        } else {
            let id = upsert_artist(pool, meta.artist.trim(), library_id).await?;
            if artist_cache.insert(artist_key, id).is_none() {
                result.artists_added += 1;
            }
            id
        };

        // Upsert album (normalize cache key for case-insensitive matching)
        let album_key = (artist_id, meta.album.trim().to_lowercase());
        let album_id = if let Some(&id) = album_cache.get(&album_key) {
            id
        } else {
            let id = upsert_album(pool, artist_id, &meta, library_id).await?;
            if album_cache.insert(album_key, id).is_none() {
                result.albums_added += 1;
            }
            // Detect cover art in album directory
            if let Some(parent) = file_path.parent() {
                if let Some(cover) = detect_cover_art(parent) {
                    let cover_str = cover.to_string_lossy().to_string();
                    let _ = sqlx::query("UPDATE albums SET cover_art_path = ? WHERE id = ? AND cover_art_path IS NULL")
                        .bind(&cover_str)
                        .bind(id.to_string())
                        .execute(pool)
                        .await;
                }
            }
            id
        };

        // Insert track
        insert_track(pool, album_id, &meta, &file_str, library_id).await?;
        result.tracks_added += 1;
    }

    // Merge any duplicate artists/albums from prior scans
    let (artists_deduped, albums_deduped) = deduplicate_library(pool, library_id).await?;

    info!(
        "scan complete: {} artists, {} albums, {} tracks added, {} errors, deduped {} artists + {} albums",
        result.artists_added, result.albums_added, result.tracks_added, result.errors.len(),
        artists_deduped, albums_deduped
    );

    Ok(result)
}

async fn upsert_artist(pool: &SqlitePool, name: &str, library_id: &str) -> anyhow::Result<Uuid> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM artists WHERE name COLLATE NOCASE = ? AND library_id = ?")
            .bind(name)
            .bind(library_id)
            .fetch_optional(pool)
            .await?;

    if let Some((id_str,)) = row {
        Ok(Uuid::parse_str(&id_str)?)
    } else {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO artists (id, name, library_id) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(name)
            .bind(library_id)
            .execute(pool)
            .await?;
        Ok(id)
    }
}

async fn upsert_album(pool: &SqlitePool, artist_id: Uuid, meta: &TrackMetadata, library_id: &str) -> anyhow::Result<Uuid> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM albums WHERE artist_id = ? AND title COLLATE NOCASE = ? AND library_id = ?")
            .bind(artist_id.to_string())
            .bind(meta.album.trim())
            .bind(library_id)
            .fetch_optional(pool)
            .await?;

    if let Some((id_str,)) = row {
        Ok(Uuid::parse_str(&id_str)?)
    } else {
        let id = Uuid::new_v4();
        let genre_json = serde_json::to_string(&meta.genre)?;
        let style_json = serde_json::to_string(&meta.style)?;

        sqlx::query(
            "INSERT INTO albums (id, title, artist_id, year, genre, style, library_id) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&meta.album)
        .bind(artist_id.to_string())
        .bind(meta.year)
        .bind(&genre_json)
        .bind(&style_json)
        .bind(library_id)
        .execute(pool)
        .await?;

        Ok(id)
    }
}

const COVER_ART_FILENAMES: &[&str] = &[
    "cover.jpg", "cover.jpeg", "cover.png",
    "folder.jpg", "folder.jpeg", "folder.png",
    "front.jpg", "front.jpeg", "front.png",
    "album.jpg", "album.jpeg", "album.png",
];

fn detect_cover_art(dir: &Path) -> Option<std::path::PathBuf> {
    for name in COVER_ART_FILENAMES {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Case-insensitive fallback
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_lowercase();
            if COVER_ART_FILENAMES.contains(&fname.as_str()) {
                return Some(entry.path());
            }
        }
    }
    None
}

async fn insert_track(
    pool: &SqlitePool,
    album_id: Uuid,
    meta: &TrackMetadata,
    file_path: &str,
    library_id: &str,
) -> anyhow::Result<()> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tracks (id, album_id, title, track_number, disc_number, duration_seconds, file_path, format, sample_rate, bit_depth, file_size_bytes, composer, language, bpm_tag, musical_key, mood, replay_gain_track_gain, replay_gain_track_peak, replay_gain_album_gain, replay_gain_album_peak, musicbrainz_recording_id, library_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(album_id.to_string())
    .bind(&meta.title)
    .bind(meta.track_number)
    .bind(meta.disc_number)
    .bind(meta.duration_seconds)
    .bind(file_path)
    .bind(&meta.format)
    .bind(meta.sample_rate)
    .bind(meta.bit_depth.unwrap_or(0))
    .bind(meta.file_size_bytes)
    .bind(&meta.composer)
    .bind(&meta.language)
    .bind(meta.bpm)
    .bind(&meta.musical_key)
    .bind(&meta.mood)
    .bind(meta.replay_gain_track_gain)
    .bind(meta.replay_gain_track_peak)
    .bind(meta.replay_gain_album_gain)
    .bind(meta.replay_gain_album_peak)
    .bind(&meta.musicbrainz_recording_id)
    .bind(library_id)
    .execute(pool)
    .await?;

    // Set is_compilation on the album if the track tag says so
    if meta.is_compilation {
        sqlx::query("UPDATE albums SET is_compilation = 1 WHERE id = ?")
            .bind(album_id.to_string())
            .execute(pool)
            .await?;
    }

    Ok(())
}

/// Merge duplicate artists and albums (case-insensitive name matches).
/// Returns (artists_merged, albums_merged).
async fn deduplicate_library(pool: &SqlitePool, library_id: &str) -> anyhow::Result<(u32, u32)> {
    let artists_merged = deduplicate_artists(pool, library_id).await?;
    let albums_merged = deduplicate_albums(pool, library_id).await?;
    Ok((artists_merged, albums_merged))
}

async fn deduplicate_artists(pool: &SqlitePool, library_id: &str) -> anyhow::Result<u32> {
    let dupes: Vec<(String,)> = sqlx::query_as(
        "SELECT lower(name) FROM artists WHERE library_id = ? GROUP BY lower(name) HAVING COUNT(*) > 1",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    let mut merged = 0u32;

    for (lower_name,) in &dupes {
        let artists: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM artists WHERE lower(name) = ? AND library_id = ? ORDER BY id ASC",
        )
        .bind(lower_name)
        .bind(library_id)
        .fetch_all(pool)
        .await?;

        if artists.len() < 2 {
            continue;
        }

        let keep_id = &artists[0].0;
        for dupe in &artists[1..] {
            let dupe_id = &dupe.0;

            sqlx::query("UPDATE albums SET artist_id = ? WHERE artist_id = ?")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(pool)
                .await?;

            sqlx::query("DELETE FROM artists WHERE id = ?")
                .bind(dupe_id)
                .execute(pool)
                .await?;

            merged += 1;
        }
    }

    if merged > 0 {
        info!("deduplicated {} duplicate artist entries", merged);
    }
    Ok(merged)
}

async fn deduplicate_albums(pool: &SqlitePool, library_id: &str) -> anyhow::Result<u32> {
    let dupes: Vec<(String, String)> = sqlx::query_as(
        "SELECT artist_id, lower(title) FROM albums WHERE library_id = ? GROUP BY artist_id, lower(title) HAVING COUNT(*) > 1",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    let mut merged = 0u32;

    for (artist_id, lower_title) in &dupes {
        let albums: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM albums WHERE artist_id = ? AND lower(title) = ? AND library_id = ? ORDER BY added_at ASC NULLS LAST, id ASC",
        )
        .bind(artist_id)
        .bind(lower_title)
        .bind(library_id)
        .fetch_all(pool)
        .await?;

        if albums.len() < 2 {
            continue;
        }

        let keep_id = &albums[0].0;
        for dupe in &albums[1..] {
            let dupe_id = &dupe.0;

            // Move tracks to kept album
            sqlx::query("UPDATE tracks SET album_id = ? WHERE album_id = ?")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(pool)
                .await?;

            // Move album credits
            sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
                .bind(dupe_id)
                .execute(pool)
                .await?;

            // Move favorites
            sqlx::query("UPDATE OR IGNORE favorites SET item_id = ? WHERE item_id = ? AND item_type = 'album'")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(pool)
                .await?;

            sqlx::query("DELETE FROM albums WHERE id = ?")
                .bind(dupe_id)
                .execute(pool)
                .await?;

            merged += 1;
        }
    }

    if merged > 0 {
        info!("deduplicated {} duplicate album entries", merged);
    }
    Ok(merged)
}
