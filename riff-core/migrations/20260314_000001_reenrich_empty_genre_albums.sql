-- Re-enrich albums that matched but got no genres, so the new
-- release-group / artist genre fallback can fill them in.
UPDATE albums SET metadata_status = 'pending'
WHERE metadata_status = 'matched' AND (genre IS NULL OR genre = '[]');
