ALTER TABLE albums ADD COLUMN metadata_status TEXT NOT NULL DEFAULT 'pending'
    CHECK(metadata_status IN ('pending', 'matched', 'not_found'));
UPDATE albums SET metadata_status = 'matched' WHERE discogs_id IS NOT NULL;
