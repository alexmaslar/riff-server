CREATE TABLE IF NOT EXISTS artist_recommendations (
    id TEXT PRIMARY KEY,
    artist_id TEXT NOT NULL,
    recommended_artist_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    score REAL NOT NULL,
    sort_order INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (artist_id) REFERENCES artists(id) ON DELETE CASCADE,
    FOREIGN KEY (recommended_artist_id) REFERENCES artists(id) ON DELETE CASCADE,
    UNIQUE(artist_id, recommended_artist_id)
);
CREATE INDEX IF NOT EXISTS idx_artist_recommendations_artist_id ON artist_recommendations(artist_id);
