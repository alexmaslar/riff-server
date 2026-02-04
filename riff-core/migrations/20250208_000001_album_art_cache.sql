-- Album art cache tracking table
-- Stores metadata about generated album art effects
CREATE TABLE IF NOT EXISTS album_art_cache (
    id TEXT PRIMARY KEY NOT NULL,
    album_id TEXT NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    effect TEXT NOT NULL,
    size INTEGER NOT NULL,
    file_path TEXT NOT NULL,
    file_size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(album_id, effect, size)
);

CREATE INDEX IF NOT EXISTS idx_album_art_cache_album_id
    ON album_art_cache(album_id);
