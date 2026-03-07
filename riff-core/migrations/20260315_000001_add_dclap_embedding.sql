-- Add DCLAP neural embedding column for perceptual audio similarity
-- 512 x f32 = 2048 bytes per track, stored as BLOB
ALTER TABLE tracks ADD COLUMN dclap_embedding BLOB;
