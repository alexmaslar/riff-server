use crate::config::LibraryEntry;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;
use tracing::info;
use uuid::Uuid;

pub async fn init_pool() -> anyhow::Result<SqlitePool> {
    let data_dir = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?
        .join("riff");

    std::fs::create_dir_all(&data_dir)?;

    let db_path = data_dir.join("riff.db");
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

    let options = SqliteConnectOptions::from_str(&db_url)?
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .create_if_missing(true)
        .optimize_on_close(true, Some(1000))
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(20)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

/// Sync the `libraries` table with the config entries.
/// - Matches by `path`: updates name/isolated if changed, inserts new rows.
/// - Removes DB rows whose path is no longer in config (wipes their data first).
/// - Backfills `library_id` on existing content that has NULL library_id.
pub async fn sync_libraries(pool: &SqlitePool, libraries: &[LibraryEntry]) -> anyhow::Result<()> {
    // Fetch all existing rows
    let existing: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT id, name, path, isolated FROM libraries",
    )
    .fetch_all(pool)
    .await?;

    let config_paths: std::collections::HashSet<&str> =
        libraries.iter().map(|l| l.path.as_str()).collect();

    // Remove DB rows whose path is no longer in config
    for (db_id, _name, db_path, _isolated) in &existing {
        if !config_paths.contains(db_path.as_str()) {
            info!("library path {:?} removed from config, wiping data", db_path);
            wipe_library_data(pool, db_id).await?;
            sqlx::query("DELETE FROM libraries WHERE id = ?")
                .bind(db_id)
                .execute(pool)
                .await?;
        }
    }

    let db_by_path: std::collections::HashMap<&str, &(String, String, String, bool)> = existing
        .iter()
        .map(|row| (row.2.as_str(), row))
        .collect();

    // Upsert config entries
    for (i, entry) in libraries.iter().enumerate() {
        if let Some(row) = db_by_path.get(entry.path.as_str()) {
            let db_id = &row.0;
            // Update name/isolated/display_order and per-library config if changed
            sqlx::query(
                "UPDATE libraries SET name = ?, isolated = ?, display_order = ?, \
                 auto_enrich = ?, album_summaries = ?, album_ratings = ?, \
                 album_recommendations = ?, artist_bios = ?, artist_recommendations = ?, \
                 scan_interval = ? \
                 WHERE id = ?",
            )
            .bind(&entry.name)
            .bind(entry.isolated)
            .bind(i as i64)
            .bind(entry.auto_enrich)
            .bind(entry.album_summaries)
            .bind(entry.album_ratings)
            .bind(entry.album_recommendations)
            .bind(entry.artist_bios)
            .bind(entry.artist_recommendations)
            .bind(entry.scan_interval.map(|v| v as i64))
            .bind(db_id)
            .execute(pool)
            .await?;
        } else {
            // Insert new library
            let id = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO libraries (id, name, path, isolated, display_order, \
                 auto_enrich, album_summaries, album_ratings, \
                 album_recommendations, artist_bios, artist_recommendations, \
                 scan_interval) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&entry.name)
            .bind(&entry.path)
            .bind(entry.isolated)
            .bind(i as i64)
            .bind(entry.auto_enrich)
            .bind(entry.album_summaries)
            .bind(entry.album_ratings)
            .bind(entry.album_recommendations)
            .bind(entry.artist_bios)
            .bind(entry.artist_recommendations)
            .bind(entry.scan_interval.map(|v| v as i64))
            .execute(pool)
            .await?;
            info!("registered new library {:?} at {:?} (id={})", entry.name, entry.path, id);
        }
    }

    // Backfill: assign NULL library_id rows to the first library (migration from single-library)
    let first_lib: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM libraries ORDER BY display_order LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    if let Some((first_id,)) = first_lib {
        for table in &["artists", "albums", "tracks", "playlists"] {
            let sql = format!("UPDATE {table} SET library_id = ? WHERE library_id IS NULL");
            let result = sqlx::query(&sql)
                .bind(&first_id)
                .execute(pool)
                .await?;
            if result.rows_affected() > 0 {
                info!(
                    "backfilled {} rows in {} with library_id={}",
                    result.rows_affected(),
                    table,
                    first_id
                );
            }
        }
        // Daily mixes are ephemeral (regenerated regularly) — delete stale NULL ones
        // instead of backfilling, which would violate the UNIQUE constraint.
        let deleted = sqlx::query("DELETE FROM daily_mixes WHERE library_id IS NULL")
            .execute(pool)
            .await?;
        if deleted.rows_affected() > 0 {
            info!("deleted {} stale daily_mixes with NULL library_id", deleted.rows_affected());
        }
    }

    Ok(())
}

/// Resolve a library query param into a JSON array of library IDs for use with `json_each()`.
/// - `None` → all non-isolated library IDs (the default "All Music" context)
/// - `Some(id)` → validates that library exists, returns `["id"]`
pub async fn resolve_library_ids(
    pool: &SqlitePool,
    library_id: Option<&str>,
) -> anyhow::Result<String> {
    match library_id {
        None => {
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT id FROM libraries WHERE isolated = 0")
                    .fetch_all(pool)
                    .await?;
            let ids: Vec<&str> = rows.iter().map(|(id,)| id.as_str()).collect();
            Ok(serde_json::to_string(&ids)?)
        }
        Some(id) => {
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM libraries WHERE id = ?)",
            )
            .bind(id)
            .fetch_one(pool)
            .await?;
            if !exists {
                anyhow::bail!("library not found: {}", id);
            }
            Ok(serde_json::to_string(&[id])?)
        }
    }
}

/// Delete all content belonging to a specific library, preserving users and settings.
pub async fn wipe_library_data(pool: &SqlitePool, library_id: &str) -> anyhow::Result<()> {
    // Order matters: delete children before parents
    sqlx::query(
        "DELETE FROM daily_mix_tracks WHERE mix_id IN (SELECT id FROM daily_mixes WHERE library_id = ?)
         OR track_id IN (SELECT id FROM tracks WHERE library_id = ?)",
    )
    .bind(library_id)
    .bind(library_id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM daily_mixes WHERE library_id = ? OR library_id IS NULL")
        .bind(library_id)
        .execute(pool)
        .await?;

    sqlx::query(
        "DELETE FROM playlist_tracks WHERE playlist_id IN (SELECT id FROM playlists WHERE library_id = ?)
         OR track_id IN (SELECT id FROM tracks WHERE library_id = ?)",
    )
    .bind(library_id)
    .bind(library_id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM playlists WHERE library_id = ?")
        .bind(library_id)
        .execute(pool)
        .await?;

    // play_history references tracks — delete via join
    sqlx::query(
        "DELETE FROM play_history WHERE track_id IN (SELECT id FROM tracks WHERE library_id = ?)",
    )
    .bind(library_id)
    .execute(pool)
    .await?;

    // favorites can reference albums, artists, or tracks — delete those in library
    sqlx::query(
        "DELETE FROM favorites WHERE (entity_type = 'album' AND entity_id IN (SELECT id FROM albums WHERE library_id = ?))
         OR (entity_type = 'artist' AND entity_id IN (SELECT id FROM artists WHERE library_id = ?))
         OR (entity_type = 'track' AND entity_id IN (SELECT id FROM tracks WHERE library_id = ?))",
    )
    .bind(library_id)
    .bind(library_id)
    .bind(library_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "DELETE FROM album_credits WHERE album_id IN (SELECT id FROM albums WHERE library_id = ?)",
    )
    .bind(library_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "DELETE FROM album_recommendations WHERE album_id IN (SELECT id FROM albums WHERE library_id = ?)
         OR recommended_album_id IN (SELECT id FROM albums WHERE library_id = ?)",
    )
    .bind(library_id)
    .bind(library_id)
    .execute(pool)
    .await?;

    sqlx::query(
        "DELETE FROM artist_recommendations WHERE artist_id IN (SELECT id FROM artists WHERE library_id = ?)
         OR recommended_artist_id IN (SELECT id FROM artists WHERE library_id = ?)",
    )
    .bind(library_id)
    .bind(library_id)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM tracks WHERE library_id = ?")
        .bind(library_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM albums WHERE library_id = ?")
        .bind(library_id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM artists WHERE library_id = ?")
        .bind(library_id)
        .execute(pool)
        .await?;

    // Clear user default_library_id references so the library row can be deleted
    sqlx::query("UPDATE users SET default_library_id = NULL WHERE default_library_id = ?")
        .bind(library_id)
        .execute(pool)
        .await?;

    info!("library data wiped for library_id={}", library_id);
    Ok(())
}

/// Update file paths when a library is relocated to a new directory.
/// Preserves all metadata by updating path prefixes instead of wiping data.
pub async fn relocate_library_paths(
    pool: &SqlitePool,
    library_id: &str,
    old_path: &str,
    new_path: &str,
) -> anyhow::Result<()> {
    // Update track file paths
    let tracks_updated = sqlx::query(
        "UPDATE tracks SET file_path = REPLACE(file_path, ?, ?) WHERE library_id = ?"
    )
    .bind(old_path)
    .bind(new_path)
    .bind(library_id)
    .execute(pool)
    .await?
    .rows_affected();

    // Update album cover art paths
    let covers_updated = sqlx::query(
        "UPDATE albums SET cover_art_path = REPLACE(cover_art_path, ?, ?)
         WHERE library_id = ? AND cover_art_path IS NOT NULL"
    )
    .bind(old_path)
    .bind(new_path)
    .bind(library_id)
    .execute(pool)
    .await?
    .rows_affected();

    info!(
        "library paths relocated for library_id={}: {} tracks, {} covers updated",
        library_id, tracks_updated, covers_updated
    );
    Ok(())
}

/// Get the library_id for a given library path.
pub async fn get_library_id_by_path(pool: &SqlitePool, path: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM libraries WHERE path = ?",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}
