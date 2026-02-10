use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

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

/// Check if the library path changed since last run.
/// If it changed, wipe all content tables so the next scan starts fresh.
pub async fn check_library_path(pool: &SqlitePool, library_path: &str) -> anyhow::Result<()> {
    let stored: Option<(String,)> =
        sqlx::query_as("SELECT value FROM settings WHERE key = 'library_path'")
            .fetch_optional(pool)
            .await?;

    match stored {
        Some((stored_path,)) if stored_path == library_path => {
            // Path unchanged, nothing to do
        }
        Some((stored_path,)) => {
            info!(
                "library path changed from {:?} to {:?}, wiping library data",
                stored_path, library_path
            );
            wipe_library_data(pool).await?;
            sqlx::query("UPDATE settings SET value = ? WHERE key = 'library_path'")
                .bind(library_path)
                .execute(pool)
                .await?;
        }
        None => {
            // First run with this feature — just store the path
            sqlx::query("INSERT INTO settings (key, value) VALUES ('library_path', ?)")
                .bind(library_path)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

/// Delete all library content, preserving users and settings.
async fn wipe_library_data(pool: &SqlitePool) -> anyhow::Result<()> {
    // Order matters: delete children before parents
    sqlx::query("DELETE FROM playlist_tracks").execute(pool).await?;
    sqlx::query("DELETE FROM play_history").execute(pool).await?;
    sqlx::query("DELETE FROM favorites").execute(pool).await?;
    sqlx::query("DELETE FROM playlists").execute(pool).await?;
    sqlx::query("DELETE FROM album_credits").execute(pool).await?;
    sqlx::query("DELETE FROM tracks").execute(pool).await?;
    sqlx::query("DELETE FROM albums").execute(pool).await?;
    sqlx::query("DELETE FROM artists").execute(pool).await?;
    info!("library data wiped");
    Ok(())
}
