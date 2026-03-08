-- Remove DCLAP embedding column (DCLAP has been replaced by bliss for all similarity scoring)
ALTER TABLE tracks DROP COLUMN dclap_embedding;
