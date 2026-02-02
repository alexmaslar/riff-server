pub mod metadata;

use metadata::TrackMetadata;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

const SUPPORTED_EXTENSIONS: &[&str] = &["flac", "m4a", "wav", "aiff", "aif"];

pub struct ScanResult {
    pub artists_added: u32,
    pub albums_added: u32,
    pub tracks_added: u32,
    pub errors: Vec<String>,
}

pub async fn scan_library(pool: &SqlitePool, library_path: &str) -> anyhow::Result<ScanResult> {
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
            Ok(m) => m,
            Err(e) => {
                // Fallback to directory structure
                match metadata::metadata_from_path(file_path) {
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

        // Upsert artist
        let artist_name = meta.artist.clone();
        let artist_id = if let Some(&id) = artist_cache.get(&artist_name) {
            id
        } else {
            let id = upsert_artist(pool, &artist_name).await?;
            if artist_cache.insert(artist_name.clone(), id).is_none() {
                result.artists_added += 1;
            }
            id
        };

        // Upsert album
        let album_key = (artist_id, meta.album.clone());
        let album_id = if let Some(&id) = album_cache.get(&album_key) {
            id
        } else {
            let id = upsert_album(pool, artist_id, &meta).await?;
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
        insert_track(pool, album_id, &meta, &file_str).await?;
        result.tracks_added += 1;
    }

    info!(
        "scan complete: {} artists, {} albums, {} tracks added, {} errors",
        result.artists_added, result.albums_added, result.tracks_added, result.errors.len()
    );

    Ok(result)
}

async fn upsert_artist(pool: &SqlitePool, name: &str) -> anyhow::Result<Uuid> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM artists WHERE name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await?;

    if let Some((id_str,)) = row {
        Ok(Uuid::parse_str(&id_str)?)
    } else {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO artists (id, name) VALUES (?, ?)")
            .bind(id.to_string())
            .bind(name)
            .execute(pool)
            .await?;
        Ok(id)
    }
}

async fn upsert_album(pool: &SqlitePool, artist_id: Uuid, meta: &TrackMetadata) -> anyhow::Result<Uuid> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM albums WHERE artist_id = ? AND title = ?")
            .bind(artist_id.to_string())
            .bind(&meta.album)
            .fetch_optional(pool)
            .await?;

    if let Some((id_str,)) = row {
        Ok(Uuid::parse_str(&id_str)?)
    } else {
        let id = Uuid::new_v4();
        let genre_json = serde_json::to_string(&meta.genre)?;
        let style_json = serde_json::to_string(&meta.style)?;

        sqlx::query(
            "INSERT INTO albums (id, title, artist_id, year, genre, style) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&meta.album)
        .bind(artist_id.to_string())
        .bind(meta.year)
        .bind(&genre_json)
        .bind(&style_json)
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
) -> anyhow::Result<()> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tracks (id, album_id, title, track_number, disc_number, duration_seconds, file_path, format, sample_rate, bit_depth, file_size_bytes)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(meta.bit_depth)
    .bind(meta.file_size_bytes)
    .execute(pool)
    .await?;
    Ok(())
}
