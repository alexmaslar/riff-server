-- Daily algorithmic mixes: auto-generated playlists refreshed daily
CREATE TABLE IF NOT EXISTS daily_mixes (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mix_date TEXT NOT NULL,
    mix_type TEXT NOT NULL CHECK(mix_type IN ('artist', 'genre', 'deep_cuts', 'decade')),
    title TEXT NOT NULL,
    description TEXT,
    seed_value TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    UNIQUE(user_id, mix_date, mix_type)
);

CREATE TABLE IF NOT EXISTS daily_mix_tracks (
    mix_id TEXT NOT NULL REFERENCES daily_mixes(id) ON DELETE CASCADE,
    track_id TEXT NOT NULL REFERENCES tracks(id),
    sort_order INTEGER NOT NULL,
    PRIMARY KEY (mix_id, track_id)
);

CREATE INDEX IF NOT EXISTS idx_daily_mixes_user_date ON daily_mixes(user_id, mix_date);
