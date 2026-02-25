-- Reset polished flags so on-device polish re-runs with grounded prompts
-- that include album metadata (year, label, genre) to prevent hallucination.
UPDATE albums SET summary_polished = 0, summary_excerpt = NULL WHERE summary_polished = 1;
UPDATE artists SET editorial_bio_polished = 0 WHERE editorial_bio_polished = 1;
