-- Play history: records each play event per user per track
CREATE TABLE IF NOT EXISTS play_history (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id),
    track_id TEXT NOT NULL REFERENCES tracks(id),
    played_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (user_id) REFERENCES users(id),
    FOREIGN KEY (track_id) REFERENCES tracks(id)
);

CREATE INDEX IF NOT EXISTS idx_play_history_user_id ON play_history(user_id);
CREATE INDEX IF NOT EXISTS idx_play_history_user_played ON play_history(user_id, played_at DESC);

-- Favorites: composite PK on (user_id, entity_type, entity_id)
CREATE TABLE IF NOT EXISTS favorites (
    user_id TEXT NOT NULL REFERENCES users(id),
    entity_type TEXT NOT NULL CHECK(entity_type IN ('album', 'artist', 'track')),
    entity_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user_id, entity_type, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_favorites_user_type ON favorites(user_id, entity_type);
