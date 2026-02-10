CREATE TABLE IF NOT EXISTS artist_top_tracks (
    artist_id TEXT NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    track_name TEXT NOT NULL,
    rank INTEGER NOT NULL,
    playcount INTEGER NOT NULL DEFAULT 0,
    listeners INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (artist_id, rank)
);
