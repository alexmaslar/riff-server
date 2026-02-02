CREATE TABLE IF NOT EXISTS album_credits (
    id TEXT PRIMARY KEY NOT NULL,
    album_id TEXT NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    artist_name TEXT NOT NULL,
    role TEXT NOT NULL,
    discogs_artist_id TEXT,
    sort_order INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_album_credits_album_id ON album_credits(album_id);
