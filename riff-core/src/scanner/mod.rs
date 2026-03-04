pub mod metadata;

use metadata::TrackMetadata;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::{info, warn};
use uuid::Uuid;
use walkdir::WalkDir;

const SUPPORTED_EXTENSIONS: &[&str] = &["flac", "m4a", "wav", "aiff", "aif", "mp3", "ogg", "oga"];

pub struct ScanResult {
    pub artists_added: u32,
    pub albums_added: u32,
    pub tracks_added: u32,
    pub tracks_removed: u32,
    pub albums_removed: u32,
    pub artists_removed: u32,
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
        tracks_removed: 0,
        albums_removed: 0,
        artists_removed: 0,
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

    // Pre-load all existing file paths for this library to avoid N+1 queries
    let existing_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.file_path FROM tracks t WHERE t.library_id = ?",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;
    let existing_paths: HashSet<String> = existing_rows.into_iter().map(|(p,)| p).collect();

    // Cache: artist name -> artist id, (artist_id, album_title) -> album_id
    let mut artist_cache: HashMap<String, Uuid> = HashMap::new();
    let mut album_cache: HashMap<(Uuid, String), Uuid> = HashMap::new();

    for file_path in &audio_files {
        let file_str = file_path.to_string_lossy().to_string();

        // Skip if track already exists (checked against pre-loaded set)
        if existing_paths.contains(&file_str) {
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

    // Remove tracks whose files no longer exist, then empty albums and orphaned artists
    let (tracks_removed, albums_removed, artists_removed) =
        remove_stale_entries(pool, library_id).await?;
    result.tracks_removed = tracks_removed;
    result.albums_removed = albums_removed;
    result.artists_removed = artists_removed;

    // Merge any duplicate artists/albums from prior scans
    let (artists_deduped, albums_deduped) = deduplicate_library(pool, library_id).await?;

    info!(
        "scan complete: +{} artists, +{} albums, +{} tracks, -{} tracks, -{} albums, -{} artists, {} errors, deduped {} artists + {} albums",
        result.artists_added, result.albums_added, result.tracks_added,
        result.tracks_removed, result.albums_removed, result.artists_removed,
        result.errors.len(), artists_deduped, albums_deduped
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
        "INSERT INTO tracks (id, album_id, title, track_number, disc_number, duration_seconds, file_path, format, sample_rate, bit_depth, file_size_bytes, composer, language, bpm_tag, musical_key, mood, replay_gain_track_gain, replay_gain_track_peak, replay_gain_album_gain, replay_gain_album_peak, musicbrainz_recording_id, isrc, library_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(&meta.isrc)
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

/// Remove tracks whose files no longer exist on disk, then delete empty albums
/// and orphaned artists. Returns (tracks_removed, albums_removed, artists_removed).
async fn remove_stale_entries(
    pool: &SqlitePool,
    library_id: &str,
) -> anyhow::Result<(u32, u32, u32)> {
    // Find all tracks in this library
    let all_tracks: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT t.id, t.file_path, t.album_id FROM tracks t
         JOIN albums a ON t.album_id = a.id
         WHERE a.library_id = ?",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    // Identify stale tracks (file no longer on disk)
    let mut stale_track_ids: Vec<String> = Vec::new();
    let mut affected_album_ids: HashSet<String> = HashSet::new();
    for (track_id, file_path, album_id) in &all_tracks {
        if !Path::new(file_path).exists() {
            stale_track_ids.push(track_id.clone());
            affected_album_ids.insert(album_id.clone());
        }
    }

    // Batch-delete stale tracks and their related data in a transaction
    if !stale_track_ids.is_empty() {
        let ids_json = serde_json::to_string(&stale_track_ids)?;
        let mut tx = pool.begin().await?;

        sqlx::query("DELETE FROM play_history WHERE track_id IN (SELECT value FROM json_each(?))")
            .bind(&ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM playlist_tracks WHERE track_id IN (SELECT value FROM json_each(?))")
            .bind(&ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM daily_mix_tracks WHERE track_id IN (SELECT value FROM json_each(?))")
            .bind(&ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM favorites WHERE entity_type = 'track' AND entity_id IN (SELECT value FROM json_each(?))")
            .bind(&ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM tracks WHERE id IN (SELECT value FROM json_each(?))")
            .bind(&ids_json)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
    }

    let tracks_removed = stale_track_ids.len() as u32;
    if tracks_removed > 0 {
        info!("removed {} stale tracks", tracks_removed);
    }

    // Find empty albums (no remaining tracks)
    let empty_albums: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.id, a.artist_id FROM albums a
         WHERE a.library_id = ? AND NOT EXISTS (SELECT 1 FROM tracks WHERE album_id = a.id)",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    let mut orphan_candidate_artist_ids: HashSet<String> =
        HashSet::new();

    if !empty_albums.is_empty() {
        let album_ids: Vec<String> = empty_albums.iter().map(|(id, _)| id.clone()).collect();
        let album_ids_json = serde_json::to_string(&album_ids)?;
        let mut tx = pool.begin().await?;

        sqlx::query("DELETE FROM favorites WHERE entity_type = 'album' AND entity_id IN (SELECT value FROM json_each(?))")
            .bind(&album_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM album_credits WHERE album_id IN (SELECT value FROM json_each(?))")
            .bind(&album_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM album_recommendations WHERE album_id IN (SELECT value FROM json_each(?)) OR recommended_album_id IN (SELECT value FROM json_each(?))")
            .bind(&album_ids_json)
            .bind(&album_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM albums WHERE id IN (SELECT value FROM json_each(?))")
            .bind(&album_ids_json)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        for (_, artist_id) in &empty_albums {
            orphan_candidate_artist_ids.insert(artist_id.clone());
        }
    }

    let albums_removed = empty_albums.len() as u32;
    if albums_removed > 0 {
        info!("removed {} empty albums", albums_removed);
    }

    // Find orphaned artists (no remaining albums)
    let orphaned_artists: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM artists
         WHERE library_id = ? AND NOT EXISTS (SELECT 1 FROM albums WHERE artist_id = artists.id)",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await?;

    if !orphaned_artists.is_empty() {
        let artist_ids: Vec<String> = orphaned_artists.iter().map(|(id,)| id.clone()).collect();
        let artist_ids_json = serde_json::to_string(&artist_ids)?;
        let mut tx = pool.begin().await?;

        sqlx::query("DELETE FROM favorites WHERE entity_type = 'artist' AND entity_id IN (SELECT value FROM json_each(?))")
            .bind(&artist_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM artist_recommendations WHERE artist_id IN (SELECT value FROM json_each(?)) OR recommended_artist_id IN (SELECT value FROM json_each(?))")
            .bind(&artist_ids_json)
            .bind(&artist_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM artist_top_tracks WHERE artist_id IN (SELECT value FROM json_each(?))")
            .bind(&artist_ids_json)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM artists WHERE id IN (SELECT value FROM json_each(?))")
            .bind(&artist_ids_json)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
    }

    let artists_removed = orphaned_artists.len() as u32;
    if artists_removed > 0 {
        info!("removed {} orphaned artists", artists_removed);
    }

    Ok((tracks_removed, albums_removed, artists_removed))
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
        let mut tx = pool.begin().await?;
        for dupe in &artists[1..] {
            let dupe_id = &dupe.0;

            sqlx::query("UPDATE albums SET artist_id = ? WHERE artist_id = ?")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            sqlx::query("DELETE FROM artists WHERE id = ?")
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            merged += 1;
        }
        tx.commit().await?;
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
        let mut tx = pool.begin().await?;
        for dupe in &albums[1..] {
            let dupe_id = &dupe.0;

            // Move tracks to kept album
            sqlx::query("UPDATE tracks SET album_id = ? WHERE album_id = ?")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            // Move album credits
            sqlx::query("DELETE FROM album_credits WHERE album_id = ?")
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            // Move favorites
            sqlx::query("UPDATE OR IGNORE favorites SET item_id = ? WHERE item_id = ? AND item_type = 'album'")
                .bind(keep_id)
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            sqlx::query("DELETE FROM albums WHERE id = ?")
                .bind(dupe_id)
                .execute(&mut *tx)
                .await?;

            merged += 1;
        }
        tx.commit().await?;
    }

    if merged > 0 {
        info!("deduplicated {} duplicate album entries", merged);
    }
    Ok(merged)
}
