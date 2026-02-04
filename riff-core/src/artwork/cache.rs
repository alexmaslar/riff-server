use anyhow::{Context, Result};
use image::DynamicImage;
use sqlx::SqlitePool;
use std::path::PathBuf;
use uuid::Uuid;

/// Get cache directory, creating it if needed
pub fn get_cache_dir() -> Result<PathBuf> {
    let data_dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not find data directory"))?
        .join("riff");

    let cache_dir = data_dir.join("cache").join("covers");
    std::fs::create_dir_all(&cache_dir)
        .context("Failed to create cache directory")?;

    Ok(cache_dir)
}

/// Check if cached effect exists and return path
pub async fn check_cache(
    pool: &SqlitePool,
    album_id: &str,
    effect: &str,
    size: u32,
) -> Option<PathBuf> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT file_path FROM album_art_cache WHERE album_id = ? AND effect = ? AND size = ?"
    )
    .bind(album_id)
    .bind(effect)
    .bind(size as i64)
    .fetch_optional(pool)
    .await
    .ok()?;

    if let Some((path_str,)) = row {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Store generated effect in cache
pub async fn store_cache(
    pool: &SqlitePool,
    album_id: &str,
    effect: &str,
    size: u32,
    image: &DynamicImage,
) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;
    let filename = format!("{}_{}_{}_{}.png", album_id, effect, size, Uuid::new_v4());
    let file_path = cache_dir.join(&filename);

    // Save image to disk
    image.save(&file_path)
        .context("Failed to save cached image")?;

    // Get file size
    let file_size = std::fs::metadata(&file_path)
        .context("Failed to read cached file metadata")?
        .len();

    // Store in database
    let cache_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO album_art_cache (id, album_id, effect, size, file_path, file_size_bytes)
         VALUES (?, ?, ?, ?, ?, ?)"
    )
    .bind(&cache_id)
    .bind(album_id)
    .bind(effect)
    .bind(size as i64)
    .bind(file_path.to_str().unwrap())
    .bind(file_size as i64)
    .execute(pool)
    .await
    .context("Failed to store cache record")?;

    Ok(file_path)
}

/// Clear all cached effects for an album
pub async fn clear_album_cache(pool: &SqlitePool, album_id: &str) -> Result<u64> {
    // Get file paths before deleting records
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT file_path FROM album_art_cache WHERE album_id = ?"
    )
    .bind(album_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch cache records")?;

    // Delete files from disk
    for (path_str,) in rows {
        let path = PathBuf::from(path_str);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Delete records from database
    let result = sqlx::query("DELETE FROM album_art_cache WHERE album_id = ?")
        .bind(album_id)
        .execute(pool)
        .await
        .context("Failed to delete cache records")?;

    Ok(result.rows_affected())
}

/// Clear all cached effects
pub async fn clear_all_cache(pool: &SqlitePool) -> Result<u64> {
    // Get all file paths
    let rows = sqlx::query_as::<_, (String,)>("SELECT file_path FROM album_art_cache")
        .fetch_all(pool)
        .await
        .context("Failed to fetch cache records")?;

    // Delete files from disk
    for (path_str,) in rows {
        let path = PathBuf::from(path_str);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Delete all records from database
    let result = sqlx::query("DELETE FROM album_art_cache")
        .execute(pool)
        .await
        .context("Failed to delete cache records")?;

    Ok(result.rows_affected())
}

/// Get cache statistics
pub async fn get_cache_stats(pool: &SqlitePool) -> Result<(u64, u64)> {
    let row = sqlx::query_as::<_, (i64, i64)>(
        "SELECT COUNT(*), COALESCE(SUM(file_size_bytes), 0) FROM album_art_cache"
    )
    .fetch_one(pool)
    .await
    .context("Failed to fetch cache stats")?;

    Ok((row.0 as u64, row.1 as u64))
}
