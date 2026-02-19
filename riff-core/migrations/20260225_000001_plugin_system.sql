-- Per-user plugin auth tokens (Last.fm session key, ListenBrainz token, etc.)
CREATE TABLE IF NOT EXISTS plugin_user_tokens (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    plugin_name TEXT NOT NULL,
    token TEXT NOT NULL,
    refresh_token TEXT,
    expires_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE(user_id, plugin_name)
);

-- Streaming provider columns on albums and tracks
ALTER TABLE albums ADD COLUMN provider TEXT;
ALTER TABLE albums ADD COLUMN provider_album_id TEXT;
ALTER TABLE tracks ADD COLUMN provider TEXT;
ALTER TABLE tracks ADD COLUMN provider_track_id TEXT;

CREATE INDEX IF NOT EXISTS idx_albums_provider ON albums(provider, provider_album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_provider ON tracks(provider, provider_track_id);

-- Download queue for streaming provider downloads
CREATE TABLE IF NOT EXISTS download_queue (
    id TEXT PRIMARY KEY NOT NULL,
    provider TEXT NOT NULL,
    provider_album_id TEXT NOT NULL,
    album_title TEXT NOT NULL,
    artist_name TEXT NOT NULL,
    cover_art_url TEXT,
    quality TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    tracks_total INTEGER NOT NULL DEFAULT 0,
    tracks_completed INTEGER NOT NULL DEFAULT 0,
    current_track TEXT,
    error TEXT,
    local_album_id TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at TEXT,
    UNIQUE(provider, provider_album_id)
);
