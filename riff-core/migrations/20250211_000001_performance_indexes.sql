-- Performance indexes for large libraries

-- play_history: composite index for listening_stats queries
CREATE INDEX IF NOT EXISTS idx_play_history_user_completed
  ON play_history(user_id, completed, played_at);

-- play_history: index for track joins
CREATE INDEX IF NOT EXISTS idx_play_history_track
  ON play_history(track_id);

-- favorites: index for entity lookups
CREATE INDEX IF NOT EXISTS idx_favorites_user_entity
  ON favorites(user_id, entity_type, entity_id);

-- albums: index for recently added sort
CREATE INDEX IF NOT EXISTS idx_albums_added_at
  ON albums(added_at);
