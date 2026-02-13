-- Per-library config overrides (NULL = follow global setting)
ALTER TABLE libraries ADD COLUMN auto_enrich INTEGER;
ALTER TABLE libraries ADD COLUMN album_summaries INTEGER;
ALTER TABLE libraries ADD COLUMN album_ratings INTEGER;
ALTER TABLE libraries ADD COLUMN album_recommendations INTEGER;
ALTER TABLE libraries ADD COLUMN artist_bios INTEGER;
ALTER TABLE libraries ADD COLUMN artist_recommendations INTEGER;
