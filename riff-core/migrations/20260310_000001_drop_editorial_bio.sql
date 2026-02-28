-- Drop editorial_bio columns from artists table.
-- Artist bios now come from Discogs profile field, stored directly in `bio`.
ALTER TABLE artists DROP COLUMN editorial_bio;
ALTER TABLE artists DROP COLUMN editorial_bio_source;
ALTER TABLE artists DROP COLUMN editorial_bio_updated_at;
ALTER TABLE artists DROP COLUMN editorial_bio_polished;
