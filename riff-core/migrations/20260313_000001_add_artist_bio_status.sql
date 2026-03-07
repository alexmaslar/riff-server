ALTER TABLE artists ADD COLUMN bio_status TEXT NOT NULL DEFAULT 'pending';

-- Artists that already have a bio are 'found'
UPDATE artists SET bio_status = 'found' WHERE bio IS NOT NULL;
