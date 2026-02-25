-- Restore original review text from editorial_reviews for albums that were
-- polished by the on-device model. The unmodified review text is preserved
-- in editorial_reviews.text; copy it back to albums.summary.
UPDATE albums SET summary = (
    SELECT er.text FROM editorial_reviews er
    WHERE er.entity_type = 'album' AND er.entity_id = albums.id
    ORDER BY er.created_at DESC LIMIT 1
)
WHERE summary_polished = 1
  AND id IN (SELECT entity_id FROM editorial_reviews WHERE entity_type = 'album');

-- Reset polish flags and clear excerpts so they get re-extracted from the
-- original (unmodified) text.
UPDATE albums SET summary_polished = 0, summary_excerpt = NULL;
