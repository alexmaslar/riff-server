-- Performance indexes for common query patterns

-- Scanner does lookups by file_path to detect existing tracks
CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);

-- Album browsing sorted by year
CREATE INDEX IF NOT EXISTS idx_albums_year ON albums(year DESC);

-- Rated albums filtering/sorting (partial index — only non-NULL ratings)
CREATE INDEX IF NOT EXISTS idx_albums_rating ON albums(rating DESC) WHERE rating IS NOT NULL;
