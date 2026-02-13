-- Update daily_mixes UNIQUE constraint to include library_id
-- SQLite doesn't support ALTER TABLE DROP CONSTRAINT, so recreate the table

CREATE TABLE daily_mixes_new (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mix_date TEXT NOT NULL,
    mix_type TEXT NOT NULL CHECK(mix_type IN ('artist', 'genre', 'deep_cuts', 'decade')),
    title TEXT NOT NULL,
    description TEXT,
    seed_value TEXT,
    created_at TEXT DEFAULT (datetime('now')),
    cover_path TEXT,
    library_id TEXT REFERENCES libraries(id),
    UNIQUE(user_id, mix_date, mix_type, library_id)
);

INSERT INTO daily_mixes_new (id, user_id, mix_date, mix_type, title, description, seed_value, created_at, cover_path, library_id)
    SELECT id, user_id, mix_date, mix_type, title, description, seed_value, created_at, cover_path, library_id
    FROM daily_mixes;

DROP TABLE daily_mixes;
ALTER TABLE daily_mixes_new RENAME TO daily_mixes;

CREATE INDEX IF NOT EXISTS idx_daily_mixes_user_date ON daily_mixes(user_id, mix_date);
CREATE INDEX IF NOT EXISTS idx_daily_mixes_library ON daily_mixes(library_id);
