-- Add library_id to download_queue so downloads go to the correct library
ALTER TABLE download_queue ADD COLUMN library_id TEXT;
