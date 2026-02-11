-- Rename discogs_id/discogs_artist_id columns to generic names
-- (migrated from Discogs to MusicBrainz, column names should be provider-agnostic)

ALTER TABLE artists RENAME COLUMN discogs_id TO external_id;
ALTER TABLE albums RENAME COLUMN discogs_id TO external_id;
ALTER TABLE album_credits RENAME COLUMN discogs_artist_id TO external_artist_id;
