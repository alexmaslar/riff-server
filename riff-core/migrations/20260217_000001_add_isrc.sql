ALTER TABLE tracks ADD COLUMN isrc TEXT;
CREATE INDEX idx_tracks_isrc ON tracks(isrc);
