ALTER TABLE users ADD COLUMN default_library_id TEXT REFERENCES libraries(id);
