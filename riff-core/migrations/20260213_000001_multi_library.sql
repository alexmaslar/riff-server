-- Libraries table
CREATE TABLE IF NOT EXISTS libraries (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    isolated INTEGER NOT NULL DEFAULT 0,
    display_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Add library_id to content tables (nullable for migration, backfilled in Rust)
ALTER TABLE artists ADD COLUMN library_id TEXT REFERENCES libraries(id);
ALTER TABLE albums ADD COLUMN library_id TEXT REFERENCES libraries(id);
ALTER TABLE tracks ADD COLUMN library_id TEXT REFERENCES libraries(id);
ALTER TABLE daily_mixes ADD COLUMN library_id TEXT REFERENCES libraries(id);
ALTER TABLE playlists ADD COLUMN library_id TEXT REFERENCES libraries(id);

-- Indexes for library-scoped queries
CREATE INDEX IF NOT EXISTS idx_artists_library ON artists(library_id);
CREATE INDEX IF NOT EXISTS idx_albums_library ON albums(library_id);
CREATE INDEX IF NOT EXISTS idx_tracks_library ON tracks(library_id);
CREATE INDEX IF NOT EXISTS idx_daily_mixes_library ON daily_mixes(library_id);
CREATE INDEX IF NOT EXISTS idx_playlists_library ON playlists(library_id);
