CREATE TABLE IF NOT EXISTS album_recommendations (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL,
    recommended_album_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    score REAL NOT NULL,
    sort_order INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE CASCADE,
    FOREIGN KEY (recommended_album_id) REFERENCES albums(id) ON DELETE CASCADE,
    UNIQUE(album_id, recommended_album_id)
);

CREATE INDEX IF NOT EXISTS idx_album_recommendations_album_id ON album_recommendations(album_id);
