CREATE TABLE IF NOT EXISTS lb_similar_artists (
    artist_mbid TEXT NOT NULL,
    similar_artist_mbid TEXT NOT NULL,
    similar_artist_name TEXT,
    score INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (artist_mbid, similar_artist_mbid)
);
CREATE INDEX IF NOT EXISTS idx_lb_similar_artists_similar ON lb_similar_artists(similar_artist_mbid);

CREATE TABLE IF NOT EXISTS lb_similar_recordings (
    recording_mbid TEXT NOT NULL,
    similar_recording_mbid TEXT NOT NULL,
    similar_recording_name TEXT,
    similar_artist_name TEXT,
    score INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (recording_mbid, similar_recording_mbid)
);
CREATE INDEX IF NOT EXISTS idx_lb_similar_recordings_similar ON lb_similar_recordings(similar_recording_mbid);

CREATE TABLE IF NOT EXISTS lb_enrichment_status (
    entity_type TEXT NOT NULL,
    entity_mbid TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (entity_type, entity_mbid)
);
