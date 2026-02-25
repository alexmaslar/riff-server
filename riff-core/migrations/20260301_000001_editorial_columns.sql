-- Rename AI columns to generic names
ALTER TABLE albums RENAME COLUMN ai_summary TO summary;
ALTER TABLE albums RENAME COLUMN ai_rating TO rating;
ALTER TABLE albums RENAME COLUMN ai_moods TO moods;
ALTER TABLE albums RENAME COLUMN ai_descriptors TO descriptors;
ALTER TABLE albums RENAME COLUMN ai_keywords TO keywords;
ALTER TABLE artists RENAME COLUMN ai_bio TO editorial_bio;

-- Source tracking
ALTER TABLE albums ADD COLUMN summary_source TEXT;
ALTER TABLE albums ADD COLUMN rating_sources TEXT DEFAULT '[]';
ALTER TABLE albums ADD COLUMN summary_updated_at TEXT;
ALTER TABLE artists ADD COLUMN editorial_bio_source TEXT;
ALTER TABLE artists ADD COLUMN editorial_bio_updated_at TEXT;

-- Reviews from individual sources
CREATE TABLE IF NOT EXISTS editorial_reviews (
    id TEXT PRIMARY KEY,
    entity_type TEXT NOT NULL CHECK(entity_type IN ('album', 'artist')),
    entity_id TEXT NOT NULL,
    source TEXT NOT NULL,
    source_url TEXT,
    text TEXT NOT NULL,
    rating REAL,
    rating_count INTEGER,
    license TEXT,
    source_updated_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(entity_type, entity_id, source)
);
CREATE INDEX IF NOT EXISTS idx_editorial_reviews_entity ON editorial_reviews(entity_type, entity_id);
