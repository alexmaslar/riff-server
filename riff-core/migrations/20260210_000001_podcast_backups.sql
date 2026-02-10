CREATE TABLE IF NOT EXISTS podcast_backups (
    user_id TEXT PRIMARY KEY REFERENCES users(id),
    data TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
