ALTER TABLE tracks ADD COLUMN apple_music_id TEXT;
CREATE INDEX idx_tracks_apple_music_id ON tracks(apple_music_id);
