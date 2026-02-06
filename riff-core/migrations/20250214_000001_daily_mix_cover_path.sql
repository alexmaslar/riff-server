-- Add cover_path column for blurred mosaic artwork on daily mixes
ALTER TABLE daily_mixes ADD COLUMN cover_path TEXT;
