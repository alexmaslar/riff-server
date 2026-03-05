DROP INDEX IF EXISTS idx_tracks_apple_music_id;
ALTER TABLE albums DROP COLUMN apple_music_id;
ALTER TABLE tracks DROP COLUMN apple_music_id;
