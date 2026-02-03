-- Track tag-extracted fields
ALTER TABLE tracks ADD COLUMN composer TEXT;
ALTER TABLE tracks ADD COLUMN language TEXT;
ALTER TABLE tracks ADD COLUMN bpm_tag REAL;
ALTER TABLE tracks ADD COLUMN musical_key TEXT;
ALTER TABLE tracks ADD COLUMN mood TEXT;
ALTER TABLE tracks ADD COLUMN replay_gain_track_gain REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_track_peak REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_album_gain REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_album_peak REAL;
ALTER TABLE tracks ADD COLUMN musicbrainz_recording_id TEXT;

-- Track audio analysis fields
ALTER TABLE tracks ADD COLUMN bpm_analyzed REAL;
ALTER TABLE tracks ADD COLUMN key_analyzed TEXT;
ALTER TABLE tracks ADD COLUMN loudness_lufs REAL;
ALTER TABLE tracks ADD COLUMN bliss_features TEXT;
ALTER TABLE tracks ADD COLUMN analysis_status TEXT NOT NULL DEFAULT 'pending'
    CHECK(analysis_status IN ('pending', 'analyzing', 'complete', 'failed', 'skipped'));
ALTER TABLE tracks ADD COLUMN analyzed_at TEXT;
CREATE INDEX IF NOT EXISTS idx_tracks_analysis_status ON tracks(analysis_status);

-- Album additional Discogs data
ALTER TABLE albums ADD COLUMN country TEXT;
ALTER TABLE albums ADD COLUMN release_notes TEXT;
ALTER TABLE albums ADD COLUMN all_labels TEXT NOT NULL DEFAULT '[]';
ALTER TABLE albums ADD COLUMN is_compilation INTEGER NOT NULL DEFAULT 0;
